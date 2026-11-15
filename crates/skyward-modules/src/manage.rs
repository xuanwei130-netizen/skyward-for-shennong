//! KernelSU modules: what `/data/adb/modules` holds, and turning one off.
//!
//! A KernelSU module is a folder named by its id, with a `module.prop`
//! saying what it is. The framework reads marker files in that folder as
//! the phone boots: `disable` skips the module, `remove` deletes it,
//! `skip_mount` keeps its `system` folder from being overlaid, and `update`
//! says a newer copy waits. Turning a module off is all this page does:
//! the `disable` marker is made, and the module is skipped on the next
//! boot. Nothing is deleted and no script is run, so a module that would
//! not come away cleanly is simply left in place, switched off.
//!
//! Reading the folder needs the data partition decrypted; the page says
//! so until it is.

use anyhow::{bail, Context, Result};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

/// Where KernelSU keeps its modules, on the data partition.
pub const KSU_MODULES: &str = "/data/adb/modules";

/// The marker that turns a module off at the next boot.
pub const DISABLE: &str = "disable";
/// Marks a module for deletion at the next boot. This page never makes it.
pub const REMOVE: &str = "remove";
/// Keeps the module's `system` folder from being overlaid.
pub const SKIP_MOUNT: &str = "skip_mount";
/// Says a newer copy of the module waits in `modules_update`.
pub const UPDATE: &str = "update";
/// A module's metadata, in Java properties form.
pub const MODULE_PROP: &str = "module.prop";
/// Run by KernelSU when it removes a module. Shown, never run from here.
pub const UNINSTALL_SH: &str = "uninstall.sh";

/// What a module's `module.prop` says of it. Fields this page does not
/// show, such as `updateJson`, are left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prop {
    pub id: String,
    pub name: String,
    pub version: String,
    pub version_code: String,
    pub author: String,
    pub description: String,
}

/// Read `module.prop`: `key=value` lines, with `\` at the end of a line
/// joining it to the next, and `#` or `!` starting a comment. A value may
/// hold further `=` characters, as a description often does.
pub fn parse_prop(text: &str) -> Prop {
    let mut prop = Prop::default();
    let mut pending = String::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        let continues = line.ends_with('\\') && !line.ends_with("\\\\");
        let body = if continues { &line[..line.len() - 1] } else { line };
        pending.push_str(body.trim_start());
        if continues {
            continue;
        }
        let line = std::mem::take(&mut pending);
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim().to_string();
        match key.trim() {
            "id" => prop.id = value,
            "name" => prop.name = value,
            "version" => prop.version = value,
            "versionCode" => prop.version_code = value,
            "author" => prop.author = value,
            "description" => prop.description = value,
            _ => {}
        }
    }
    prop
}

/// A KernelSU module, as much of it as the pages show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    /// The folder's name, which is what KernelSU turns on and off.
    pub id: String,
    pub name: String,
    pub version: String,
    pub version_code: String,
    pub author: String,
    pub description: String,
    /// Where the module lives, shown on its page.
    pub dir: PathBuf,
    /// False once the `disable` marker is there.
    pub enabled: bool,
    /// It is already marked for deletion by someone else.
    pub remove_pending: bool,
    /// Its `system` folder is not overlaid.
    pub skip_mount: bool,
    /// A newer copy waits in `modules_update`.
    pub update_pending: bool,
    /// It ships an `uninstall.sh`. Shown as information only.
    pub has_uninstall: bool,
    /// It has a `system` folder to overlay.
    pub has_system: bool,
}

impl Module {
    /// A name a person can read; a module without a name is its id.
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() { &self.id } else { &self.name }
    }

    /// Whether the `disable` marker is already there.
    pub fn closed(&self) -> bool {
        !self.enabled
    }

    /// The markers and contents the page lists under the module.
    pub fn flags(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.remove_pending {
            out.push(REMOVE);
        }
        if self.skip_mount {
            out.push(SKIP_MOUNT);
        }
        if self.update_pending {
            out.push(UPDATE);
        }
        if self.has_uninstall {
            out.push(UNINSTALL_SH);
        }
        if self.has_system {
            out.push("system/");
        }
        out
    }
}

/// Whether a folder name is one a module id can be: no path in it, no
/// dot at its start, nothing a shell would read as an option. KernelSU's
/// own ids are letters, digits, underscores, dots and hyphens.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && !id.starts_with('-')
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        && !id.contains("..")
}

/// Read the modules in `root`, in id order. A folder without a
/// `module.prop` is not a module and is left out; one that cannot be read
/// does not stop the rest of the list.
pub fn list(root: &Path) -> Result<Vec<Module>> {
    let rd = fs::read_dir(root).with_context(|| format!("read {}", root.display()))?;
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_string) else { continue };
        if !valid_id(&id) {
            continue;
        }
        let Ok(text) = fs::read_to_string(dir.join(MODULE_PROP)) else { continue };
        let prop = parse_prop(&text);
        out.push(Module {
            dir: dir.clone(),
            enabled: !dir.join(DISABLE).exists(),
            remove_pending: dir.join(REMOVE).exists(),
            skip_mount: dir.join(SKIP_MOUNT).exists(),
            update_pending: dir.join(UPDATE).exists(),
            has_uninstall: dir.join(UNINSTALL_SH).is_file(),
            has_system: dir.join("system").is_dir(),
            id,
            name: prop.name,
            version: prop.version,
            version_code: prop.version_code,
            author: prop.author,
            description: prop.description,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// How turning a module off went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed {
    /// The module's folder name.
    pub id: String,
    /// The marker made, or the one already there.
    pub marker: PathBuf,
    /// Whether the module had been turned off before.
    pub already: bool,
}

/// Turn the module `id` off by making its `disable` marker. Nothing is
/// removed and no script is run: KernelSU skips the module at the next
/// boot. A module already off is left as it is and reported as such.
pub fn close(root: &Path, id: &str) -> Result<Closed> {
    if !valid_id(id) {
        bail!("{id:?} is not a KernelSU module id");
    }
    let dir = root.join(id);
    if !dir.is_dir() {
        bail!("no KernelSU module named {id}");
    }
    if !dir.join(MODULE_PROP).is_file() {
        bail!("{id} holds no {MODULE_PROP}");
    }
    let marker = dir.join(DISABLE);
    if marker.exists() {
        return Ok(Closed { id: id.to_string(), marker, already: true });
    }
    // An empty file is all KernelSU looks for. create_new keeps a marker
    // someone else made between the check and here from being overwritten.
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .with_context(|| format!("make {}", marker.display()))?;
    Ok(Closed { id: id.to_string(), marker, already: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of its own for each test, gone when the test lets it go.
    fn temp(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skyward-ksu-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn module(root: &Path, id: &str, prop: &str) -> PathBuf {
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MODULE_PROP), prop).unwrap();
        dir
    }

    const PROP: &str = "\
id=zygisksu
name=ZygiskNext
version=v1.2.0
versionCode=120
author=Someone
description=A module that hooks zygote, with an = in its text
updateJson=https://example.invalid/update.json
";

    #[test]
    fn a_prop_is_read_field_by_field() {
        let prop = parse_prop(PROP);
        assert_eq!(prop.id, "zygisksu");
        assert_eq!(prop.name, "ZygiskNext");
        assert_eq!(prop.version, "v1.2.0");
        assert_eq!(prop.version_code, "120");
        assert_eq!(prop.author, "Someone");
        assert_eq!(prop.description, "A module that hooks zygote, with an = in its text");
    }

    #[test]
    fn comments_blank_lines_and_continued_lines_are_read() {
        let prop = parse_prop("# a comment\n! another\n\nname=Long \\  \n  Description\nversion = v2\n");
        assert_eq!(prop.name, "Long Description");
        assert_eq!(prop.version, "v2");
    }

    #[test]
    fn module_ids_are_checked_before_they_reach_a_path() {
        assert!(valid_id("zygisksu"));
        assert!(valid_id("my_module-2.v3"));
        for bad in ["", "..", "../etc", "a/b", "-option", ".hidden", "a b", "a\nb"] {
            assert!(!valid_id(bad), "{bad:?} should not be an id");
        }
    }

    #[test]
    fn modules_are_listed_in_id_order_with_their_markers() {
        let root = temp("list");
        let dir = module(&root, "zygisksu", PROP);
        fs::write(dir.join(SKIP_MOUNT), b"").unwrap();
        fs::write(dir.join(UNINSTALL_SH), b"#!/system/bin/sh\n").unwrap();
        fs::create_dir_all(dir.join("system")).unwrap();
        let off = module(&root, "adaway", "id=adaway\nname=AdAway\n");
        fs::write(off.join(DISABLE), b"").unwrap();
        fs::write(off.join(REMOVE), b"").unwrap();
        fs::write(off.join(UPDATE), b"").unwrap();
        // Not a module: no module.prop.
        fs::create_dir_all(root.join("lost+found")).unwrap();
        // Not an id that may be reached.
        fs::create_dir_all(root.join(".hidden")).unwrap();

        let rows = list(&root).unwrap();
        assert_eq!(rows.len(), 2, "only folders with a module.prop are modules");
        assert_eq!(rows[0].id, "adaway");
        assert!(rows[0].closed());
        assert_eq!(rows[0].display_name(), "AdAway");
        assert_eq!(rows[0].flags(), [REMOVE, UPDATE]);
        assert_eq!(rows[1].id, "zygisksu");
        assert!(!rows[1].closed());
        assert_eq!(rows[1].display_name(), "ZygiskNext");
        assert_eq!(rows[1].flags(), [SKIP_MOUNT, UNINSTALL_SH, "system/"]);
        assert_eq!(rows[1].dir, dir);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_module_without_a_name_is_shown_by_its_id() {
        let root = temp("noname");
        module(&root, "plain", "id=plain\nversion=1\n");
        let rows = list(&root).unwrap();
        assert_eq!(rows[0].display_name(), "plain");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn closing_makes_only_the_disable_marker() {
        let root = temp("close");
        let dir = module(&root, "zygisksu", PROP);
        fs::write(dir.join(UNINSTALL_SH), b"#!/system/bin/sh\necho removed\n").unwrap();
        let before: Vec<_> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();

        let done = close(&root, "zygisksu").unwrap();
        assert!(!done.already);
        assert_eq!(done.marker, dir.join(DISABLE));
        assert!(done.marker.is_file());
        assert_eq!(fs::metadata(&done.marker).unwrap().len(), 0, "an empty marker is all KernelSU wants");

        let after: Vec<_> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(after.len(), before.len() + 1, "nothing was deleted");
        assert!(after.iter().all(|n| before.contains(n) || n == DISABLE), "only the marker was added");
        assert!(dir.join(UNINSTALL_SH).is_file(), "the uninstall script was not run or taken");
        assert!(!dir.join(REMOVE).exists(), "no remove marker was made");
        assert!(!list(&root).unwrap()[0].enabled);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn closing_an_already_closed_module_changes_nothing() {
        let root = temp("already");
        let dir = module(&root, "adaway", "id=adaway\nname=AdAway\n");
        fs::write(dir.join(DISABLE), b"kept as it is").unwrap();
        let done = close(&root, "adaway").unwrap();
        assert!(done.already);
        assert_eq!(fs::read_to_string(dir.join(DISABLE)).unwrap(), "kept as it is", "the old marker was left alone");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_bad_id_or_a_missing_module_is_refused() {
        let root = temp("refuse");
        module(&root, "zygisksu", PROP);
        assert!(close(&root, "../etc").is_err());
        assert!(close(&root, "").is_err());
        assert!(close(&root, "a/b").is_err());
        assert!(close(&root, "not_there").is_err());
        assert!(!root.join("zygisksu").join(DISABLE).exists(), "nothing was touched by a refused close");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_folder_without_a_prop_cannot_be_closed() {
        let root = temp("noprop");
        fs::create_dir_all(root.join("stray")).unwrap();
        assert!(close(&root, "stray").is_err());
        fs::remove_dir_all(root).ok();
    }
}
