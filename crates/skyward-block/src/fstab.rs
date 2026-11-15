//! The table a phone keeps of its own filesystems: where each one is, what
//! it holds, and how it is encrypted.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Where the phone's own partitions keep the table.
pub const FILES: [&str; 2] = ["/vendor/etc/fstab.qcom", "/odm/etc/fstab.qcom"];

/// One filesystem the phone knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub device: String,
    pub mount_point: String,
    pub filesystem: String,
    /// How the phone mounts it: the kernel's flags and the filesystem's
    /// own options, as one list.
    pub options: Vec<String>,
    /// What the phone says about the filesystem beyond how to mount it,
    /// such as how it is encrypted.
    pub extras: Vec<String>,
}

impl Entry {
    /// The value of an extra written as `name=value`.
    pub fn extra(&self, name: &str) -> Option<&str> {
        let prefix = format!("{name}=");
        self.extras.iter().find_map(|e| e.strip_prefix(&prefix))
    }
}

/// Every filesystem a table names. A phone may name the same mount point
/// more than once, once per filesystem it might hold.
pub fn entries(text: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [device, mount_point, filesystem, options, extras] = fields[..] else { continue };
        out.push(Entry {
            device: device.to_string(),
            mount_point: mount_point.to_string(),
            filesystem: filesystem.to_string(),
            options: options.split(',').map(str::to_string).collect(),
            extras: extras.split(',').map(str::to_string).collect(),
        });
    }
    out
}

/// Every entry of the phone's own table, read from wherever the table is.
fn table() -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    for file in FILES {
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read {file}")),
        };
        out.extend(entries(&text));
    }
    Ok(out)
}

/// What the phone's own table says about `mount_point`.
pub fn read(mount_point: &str) -> Result<Vec<Entry>> {
    Ok(table()?.into_iter().filter(|e| e.mount_point == mount_point).collect())
}

/// What the phone's own table says about the partition `name`: the first
/// entry naming it, by whichever path the table reaches it.
pub fn partition(name: &str) -> Result<Option<Entry>> {
    Ok(table()?.into_iter().find(|e| names(e, name)))
}

/// Whether an entry is of the partition `name`.
fn names(entry: &Entry, name: &str) -> bool {
    Path::new(&entry.device).file_name().is_some_and(|n| n == name)
}

/// A name without the suffix one of a pair of slots carries.
fn strip_slot(name: &str) -> &str {
    name.strip_suffix("_a").or_else(|| name.strip_suffix("_b")).unwrap_or(name)
}

/// The other of the two slots, where `suffix` names one of them.
fn other_slot(suffix: &str) -> Option<&'static str> {
    match suffix {
        "_a" => Some("_b"),
        "_b" => Some("_a"),
        _ => None,
    }
}

/// The names the partition an entry names may be reachable by: the base
/// name with the booted slot's suffix first, then the name the table
/// writes, then the other slot's. A table is often the one of a build
/// other than the one running, and says `system_a` where this phone has
/// laid its own copy out as `system`; either way the first name is the
/// one that build would use, so where the phone is the one the table was
/// written for nothing changes.
fn candidate_names(device: &str, suffix: Option<&str>) -> Vec<String> {
    let named = Path::new(device).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let bare = strip_slot(&named).to_string();
    let mut out: Vec<String> = Vec::new();
    let take = |name: String, out: &mut Vec<String>| {
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    };
    match suffix {
        Some(s) => {
            take(format!("{bare}{s}"), &mut out);
            take(named.clone(), &mut out);
            if let Some(other) = other_slot(s) {
                take(format!("{bare}{other}"), &mut out);
            }
            take(bare, &mut out);
        }
        None => take(named, &mut out),
    }
    out
}

/// The partition an entry names, as it is reachable now: of the booted
/// slot, where the phone keeps one of it per slot. A phone's table points
/// at its storage controller by a path a recovery need not have, and may
/// name a partition of the other slot, so each name the entry's is known
/// by is tried in turn, first under the table's own directory and then
/// under by-name.
pub fn device(entry: &Entry) -> Result<String> {
    let suffix = match entry.extras.iter().any(|e| e == "slotselect") {
        true => Some(crate::slot_suffix()?.with_context(|| format!("{} is of a slot, and the phone has none", entry.device))?),
        false => None,
    };
    let mut names = candidate_names(&entry.device, suffix.as_deref());
    if names.is_empty() {
        anyhow::bail!("{} names no partition", entry.device);
    }
    // The booted slot's own name, as the table writes it, comes first: it
    // is the path the phone's own init mounts.
    names.insert(0, format!("{}{}", entry.device, suffix.as_deref().unwrap_or("")));
    let dir = Path::new(&entry.device).parent();
    for name in &names {
        let path = dir.map_or_else(|| PathBuf::from(name), |d| d.join(name));
        if path.exists() {
            return Ok(path.to_string_lossy().into_owned());
        }
    }
    for name in &names {
        let by_name = Path::new(crate::BY_NAME).join(name);
        if by_name.exists() {
            return Ok(by_name.to_string_lossy().into_owned());
        }
    }
    anyhow::bail!("none of {} is here", names.join(", "))
}

/// Mount the partition `name` where and as the phone's own table mounts
/// it, and say where. Whatever shows there already, as what an earlier run
/// of the recovery service mounted does, is taken as it is.
pub fn mount(name: &str) -> Result<PathBuf> {
    let entry = partition(name)?.with_context(|| format!("the phone's own table does not mount {name}"))?;
    let at = PathBuf::from(&entry.mount_point);
    if crate::mount::is_mount_point(&at)? {
        return Ok(at);
    }
    std::fs::create_dir_all(&at).with_context(|| format!("make {}", at.display()))?;
    crate::mount::mount(Path::new(&device(&entry)?), &at, &entry.filesystem, &entry.options)?;
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "\
# a comment
/dev/block/bootdevice/by-name/metadata /metadata f2fs noatime,nosuid wait,check,formattable
/dev/block/bootdevice/by-name/userdata /data f2fs noatime,discard latemount,wait,fileencryption=aes-256-xts:aes-256-cts:v2,keydirectory=/metadata/vold/metadata_encryption,metadata_encryption=aes-256-xts:wrappedkey_v0,quota
/dev/block/bootdevice/by-name/userdata /data mifs noatime,discard latemount,wait,keydirectory=/metadata/vold/metadata_encryption
";

    #[test]
    fn a_mount_point_can_be_named_once_per_filesystem_it_might_hold() {
        let data: Vec<Entry> = entries(TABLE).into_iter().filter(|e| e.mount_point == "/data").collect();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0].filesystem, "f2fs");
        assert_eq!(data[1].filesystem, "mifs");
        assert_eq!(data[0].device, "/dev/block/bootdevice/by-name/userdata");
    }

    #[test]
    fn how_the_phone_mounts_a_filesystem_is_kept_apart_from_the_rest() {
        let data = &entries(TABLE)[1];
        assert_eq!(data.options, ["noatime", "discard"]);
        assert!(data.extras.iter().any(|e| e == "latemount") && !data.extras.iter().any(|e| e == "noatime"));
    }

    #[test]
    fn what_the_phone_says_about_encryption_is_read_off_the_entry() {
        let data = &entries(TABLE)[1];
        assert_eq!(data.extra("metadata_encryption"), Some("aes-256-xts:wrappedkey_v0"));
        assert_eq!(data.extra("keydirectory"), Some("/metadata/vold/metadata_encryption"));
        assert_eq!(data.extra("quota"), None, "a word on its own has no value");
        assert_eq!(data.extra("keydir"), None, "a shorter name is another extra");
    }

    #[test]
    fn an_entry_is_of_the_partition_its_path_ends_in() {
        let modem = &entries(
            "/dev/block/bootdevice/by-name/modem /vendor/firmware_mnt vfat ro,uid=1000,context=u:object_r:firmware_file:s0 wait,slotselect\n",
        )[0];
        assert!(names(modem, "modem"));
        assert!(!names(modem, "modemfirmware") && !names(modem, "mode"), "a name is matched whole");
        assert_eq!(modem.options[0], "ro");
        assert!(modem.extras.iter().any(|e| e == "slotselect"), "one of each slot");
    }

    #[test]
    fn comments_and_short_lines_are_passed_over() {
        assert!(entries("# nothing\n\n/dev/x /y f2fs rw\n").is_empty());
        assert_eq!(entries(TABLE).len(), 3);
    }

    #[test]
    fn the_booted_slots_name_is_tried_before_the_one_the_table_writes() {
        let names = candidate_names("/dev/block/bootdevice/by-name/system", Some("_a"));
        assert_eq!(names, ["system_a", "system", "system_b"], "the booted slot, the table's own word, the other slot");
    }

    #[test]
    fn a_name_of_the_other_slot_still_reaches_the_booted_one_first() {
        let names = candidate_names("/dev/block/bootdevice/by-name/system_b", Some("_a"));
        assert_eq!(names, ["system_a", "system_b", "system"], "one name each, no repeats");
    }

    #[test]
    fn a_phone_of_one_slot_keeps_the_name_the_table_writes() {
        assert_eq!(candidate_names("/dev/block/bootdevice/by-name/vendor", None), ["vendor"]);
    }
}
