# 把 /data 解密移植到新机型：完整流程

以小米 14 Ultra（aurora）与小米 13 Pro（nuwa）两次实际移植为例，整理出可复用的步骤。两次移植各踩过的坑都写在对应步骤里。

## 0. 先懂原理：skyward 的解密架构

skyward 的解密**不走 vold、不走 keystore2**：recovery 自带的 crypto（`skyward-crypto`）在数据分区挂载前，通过 binder 直连 vendor 的 keymint 服务取 FBE 密钥，然后自己挂 /data。

所以"移植解密"不是移植某个程序，而是让四件事成立：

1. vendor 的密钥服务栈跑起来（qseecomd → keymint → gatekeeper，外加机型专属的中间层）
2. 服务能注册（recovery 没有 hwservicemanager 帮 vendor 服务注册的部分要自己补）
3. 设备节点属主正确（没有 init，chown/chmod 都不会自动发生）
4. 加密方案被 crypto 层支持

FBE 方案 crypto 层是自动识别的：它读设备 fstab 的 `fileencryption=` 段，`wrappedkey_v0`（硬件包裹密钥，14 Ultra 就是用它）有专门的分支。**所以加密方案本身通常不用操心**，工作量全在第 1-3 步。

## 1. 确认加密方案（10 分钟）

从任意能拿到 fstab 的地方读一眼——TWRP 镜像也行，skyward-pack 可以直接读镜像里的文件：

```bash
skyward-pack cat twrp-xxx-你的机型.img system/etc/recovery.fstab | grep userdata
```

看 `fileencryption=` 一段。以 14 Ultra 为例：

```
fileencryption=aes-256-xts:aes-256-cts:v2+inlinecrypt_optimized+wrappedkey_v0,
keydirectory=/metadata/vold/metadata_encryption,
metadata_encryption=aes-256-xts:wrappedkey_v0
```

记下方案即可。13 Pro（nuwa）也是 wrappedkey_v0，两代机型方案一致，crypto 层都支持。

## 2. 挖出密钥服务栈（核心工作量，占八成时间）

在能开机的系统里挖（或用 读 vendor 分区的工具）：

```bash
adb shell grep -rE "keymint|gatekeeper|keymaster" /vendor/etc/init/ --include="*.rc" -l
adb shell grep -rE "IKeyMintDevice|IGatekeeper" /vendor/etc/vintf/
adb shell ls /vendor/bin/hw/ | grep -iE "keymint|gatekeeper|keymaster|qsee|mink"
```

要回答四个问题：

1. **keymint 的二进制路径**：QTI 平台一律是 `/vendor/bin/hw/android.hardware.security.keymint-service-qti`
2. **AIDL 还是 HIDL**：AIDL 服务名没有 `@版本号`（`android.hardware.security.keymint.IKeyMintDevice/default`），HIDL 形如 `android.hardware.gatekeeper@1.0::IGatekeeper/default`。**别猜，去 vintf 和 init.rc 里看实际的**
3. **gatekeeper 的代际**：vendor 是 Android 12/13 代的通常是 HIDL @1.0（13 Pro 就是）；Android 14+ 代的可能是 AIDL（`android.hardware.gatekeeper-service-qti`）。14 Ultra（vendor 是 Android 12 代）两种都可能，要实测
4. **机型专属的中间层**：这一步最容易漏。14 Ultra 上挖出了 `hlosminkdaemon`（MIUI 的 keymaster 守护）和 `ssgtzd`（TEE 网关），13 Pro 上没有它们——每代机器的中间层都不一样，要以自己 vendor 的 init.rc 为准

## 3. 写进 devices/<机型>.toml

`[[service]]` 条目的字段：

| 字段 | 作用 |
|---|---|
| `path` / `args` | 二进制与参数；环境不满足时可以用 `/system/bin/sh -c "..."` 包一层准备动作 |
| `libraries` | 告诉链接器去哪找 so，vendor 服务的库在 `/vendor/lib64`、`/odm/lib64` |
| `ready_service` | AIDL 服务注册名，recovery 等 it 出现才算 ready |
| `ready_hardware_service` | HIDL 服务注册名（会触发接口自描述） |
| `ready_property` | 等某个属性变为真（qseecomd 等 `vendor.sys.listeners.registered`） |
| `registers` | 服务声明自己会注册的名字（用于描述发布） |
| `user` / `groups` | 跑在哪个用户下，vendor init.rc 里怎么写就怎么抄 |
| `oneshot` | 跑完就退的脚本类服务 |

**顺序原则**：qseecomd（等 listeners.registered）在最前，keymint 随后，gatekeeper 再后。14 Ultra 的实例顺序（见本仓库 `devices/aurora.toml`）：两个环境准备的 oneshot → qseecomd → hlosminkdaemon → ssgtzd → keymint → gatekeeper → hwservicemanager → boot control。

## 4. 节点属主：已经自动做了，但要知道它的边界

recovery 没有 init，vendor/odm 的 init.rc 里几百条 `chown`/`chmod` 全不会发生。recovery 会解析 `/vendor/etc/init` 和 `/odm/etc/init` 下所有 .rc 的 chown/chmod 行并应用——13 Pro 上是 347 个节点，14 Ultra 上同理自动覆盖。

个别节点仍 root-only 时（比如 TWRP 的 `init.recovery.qcom.rc` 会 `chmod 0660 /dev/qseecom` 这类），在服务前加一个 oneshot 的 `sh` 服务补上，参考 aurora.toml 里已有的写法。

## 5. 构建

```bash
# 底座：从官方 OTA 包里只读出 recovery 分区（几 GB 的包只读几十 MB），SHA-256 强制校验
skyward-pack stock --config devices/<机型>.toml --out stock/recovery.img

# 打镜像：官方 recovery 的 init/adbd/SELinux 策略保留，recovery 服务换成 skyward
skyward-pack recovery --stock stock/recovery.img \
  --binary target/aarch64-linux-android/release/skyward \
  --config devices/<机型>.toml \
  --font resources/fonts/MiSansVF.ttf --mono resources/fonts/KirkMono-Regular.ttf \
  --mono-license resources/fonts/KirkMono-OFL.txt \
  --out out/skyward-<机型>.img
```

## 6. 上机验证：kmsg 三行法

刷入后 adb 里按顺序找三行，**断在哪一行就是哪一环还差**：

1. `N files given the owner and permissions the phone's init gives them` —— 节点属主生效
2. `the directory for the phone's hardware services can run` —— 服务注册环境就绪
3. keymint / gatekeeper 注册成功 —— 之后 /data 自动挂载，或进入解密页输入锁屏密码

## 7. 故障对照表

| 症状 | 原因 | 解法 |
|---|---|---|
| keymint 起来了但 ready_service 一直等不到 | 注册被拒：HIDL 服务没有接口描述，或 ready_service 字符串拼错 | 确认 AIDL/HIDL 代际与字符串；HIDL 的描述已自动处理 |
| qseecomd 反复重启 | SPU 机型问题（14 Ultra 实例）：`vendor.gatekeeper.disable_spu true` 等 props | 参考 aurora.toml 里的 oneshot props 写法 |
| 服务起不来，日志里 EACCES | 节点属主没覆盖到 | 看 init.rc 里该服务的用户，对照 chown 行是否在 init 目录里；不在就 oneshot chmod |
| /data 挂上但目录全空 | 密钥取到了但 fscrypt 策略不匹配 | 确认 fstab 的 fileencryption 段与第 1 步一致 |
| 解密页输密码后失败 | gatekeeper 没就绪或用户不对 | 确认 gatekeeper 服务与凭据代际（HIDL/AIDL）一致 |

## 附：两次移植的实际差异，说明为什么不能照抄

| | 13 Pro（nuwa） | 14 Ultra（aurora） |
|---|---|---|
| SoC | SM8550 | SM8650 |
| vendor 代际 | Android 13 | Android 12 |
| keymint | AIDL | AIDL |
| gatekeeper | HIDL @1.0 | HIDL/AIDL 需实测（vendor 是 12 代，倾向 HIDL） |
| 专属中间层 | 无 | hlosminkdaemon、ssgtzd |
| 加密方案 | wrappedkey_v0 | wrappedkey_v0（相同） |
| 底座 | 官方 OS3.0.311 recovery | 官方 OS3.0.308 recovery |

核心结论：**机制是通用的，服务清单是机型专属的**。清单永远从你自己的 vendor 里挖，不要从别的机型抄。
