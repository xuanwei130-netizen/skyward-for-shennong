![Skyward Recovery](assets/hero.jpg)

# Skyward

English | [简体中文](README.zh-CN.md)

Skyward is a recovery written in Rust, from scratch.

This repository is a fork of [kirklin/skyward](https://github.com/kirklin/skyward)
that builds for one phone only, the Xiaomi 14 Pro (`shennong`), and carries the
partition mounting fixes of this branch.

<p align="center">
  <img src="assets/home.png" width="30%" alt="Home">
  <img src="assets/settings-dark.png" width="30%" alt="Settings in dark mode">
  <img src="assets/time-zone.png" width="30%" alt="Time zone">
</p>

## Features

- Decrypt the data partition (the scheme is provided by @就会叫姐姐 on Coolapk, see
  [Acknowledgements](#acknowledgements))
- Install zips, system update packages and partition images, from internal storage, a USB drive or adb sideload
- Online system updates
- Backup and restore
- Wipe data and factory reset
- Mount partitions, repair and format filesystems
- File manager and terminal
- WLAN
- MTP file transfer, adb and fastbootd
- Reboot to system, recovery, fastboot, bootloader or EDL, and switch slots
- Light and dark themes; Simplified Chinese, Traditional Chinese and English

## Known issues

- USB drives are not recognized when plugged in.
- Some partitions cannot be mounted (tested: this does not affect flashing ROMs).

## Supported devices

| Device | Codename | Built on |
| --- | --- | --- |
| Xiaomi 14 Pro | `shennong` | OS3.0.309.0.WNBCNXM (Android 16) |

The image is built on top of the stock recovery of the version above. Other
phones are not part of this branch; build them from
[kirklin/skyward](https://github.com/kirklin/skyward).

## Installing

Skyward needs an unlocked bootloader. Download the zip from
[Releases](https://github.com/xuanwei130-netizen/skyward-for-shennong/releases).
Check it against `SHA256SUMS`, unzip it, and flash the image inside to the
recovery partition of the slot the phone runs from, with the phone in fastboot
mode:

```sh
unzip skyward-v0.1.2-shennong.1-shennong-OS3.0.309.0.WNBCNXM.zip
fastboot flash recovery skyward-v0.1.2-shennong.1-shennong-OS3.0.309.0.WNBCNXM.img
fastboot reboot recovery
```

Afterwards a newer image, unzipped, installs from Skyward's own Install page. With
**Flash Skyward again** on, a system update or zip that writes over the recovery
partitions gets Skyward back in them.

Without a system in super, Skyward still starts, with its screen and keys, adb,
`adb sideload` and fastbootd, enough to install one. Touch, WLAN and decryption run
from the installed system and come back with it.

adb in Skyward is a root shell that needs no authorization on the phone: anyone
with the phone and a cable can reach its storage while it is in recovery.

## Acknowledgements

The data partition decryption scheme is provided by **@就会叫姐姐** on Coolapk
(酷安). Special thanks.

## Building

Skyward builds with the stable Rust that `rust-toolchain.toml` names, with its
`aarch64-linux-android` target, the Android NDK r29 and
[cargo-ndk](https://github.com/bbqsrc/cargo-ndk). The recovery service is built for
the phone, and the packer for the computer:

```sh
cargo ndk -t arm64-v8a --platform 30 build --release -p skyward
cargo build --release -p skyward-pack
```

The packer takes the stock recovery partition out of the build's full update
package on Xiaomi's server, reading only the part of the package the partition is
in, and checks its SHA-256 against the device file:

```sh
target/release/skyward-pack stock --config devices/shennong.toml --out target/stock/shennong/recovery.img
```

Then it builds the image on top of it:

```sh
target/release/skyward-pack recovery \
  --stock target/stock/shennong/recovery.img \
  --binary target/aarch64-linux-android/release/skyward \
  --config devices/shennong.toml \
  --font resources/fonts/MiSansVF.ttf \
  --mono resources/fonts/KirkMono-Regular.ttf \
  --mono-license resources/fonts/KirkMono-OFL.txt \
  --out target/skyward-shennong.img
```

Tests and lints run on the computer; what runs only on the phone is linted for it:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo ndk -t arm64-v8a --platform 30 clippy -p skyward -p skyward-install -- -D warnings
```

`skyward-preview` draws the pages on the computer, as PNG files, without a phone:
each page whole, as a long screenshot shows it, in the states it can be in, and the
screen at moments of touches and motion. `--lang` picks the language by a locale
such as `en` or `zh-TW`:

```sh
cargo run --release -p skyward-preview -- target/preview --lang en
```

## License

[GPL-3.0-or-later](LICENSE)
