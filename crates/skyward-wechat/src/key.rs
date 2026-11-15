//! Working out the passphrase the chat app's databases are locked with.
//!
//! The app does not keep the passphrase anywhere; it keeps the account's
//! number and works the passphrase out from it and the phone's own name
//! for itself, seven letters of a digest. What the phone's name was is the
//! part no longer written down, so every way it could have been left out
//! is offered, and the check at the end of a database's first page says
//! which one is right.

use md5::{Digest, Md5};
use std::fs;
use std::path::{Path, PathBuf};

/// How many letters of the digest the app keeps.
const LENGTH: usize = 7;

/// The files the account's number is written in, under the app's own
/// folder.
const NUMBERS: &[&str] = &[
    "shared_prefs/auth_info_key_prefs.xml",
    "shared_prefs/system_config_prefs.xml",
    "shared_prefs/app_brand_global_sp.xml",
];

/// Every passphrase worth trying for the app whose folder is `root`, the
/// likeliest first.
///
/// The account's number is what everything is worked out from. Where the
/// phone could not give its own name — which is every phone made since
/// Android 10 — the app stretched the number on its own, and that is the
/// one tried first, because it is what nearly every phone since then has.
pub fn passphrases(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for uin in numbers(root) {
        let mut add = |text: String| {
            if !text.is_empty() && !out.contains(&text) {
                out.push(text);
            }
        };
        // The number as it stands, and the digest of it on its own: the
        // two shapes the app has used when the phone had no name to give.
        add(digest7(&uin));
        add(uin.clone());
        // And with the names a phone may still have been able to give.
        for name in names(root) {
            add(digest7(&format!("{name}{uin}")));
        }
    }
    out
}

/// The sizes of passphrase worth trying, most likely first, with each
/// account's number behind it.
pub fn guess(root: &Path) -> Option<String> {
    passphrases(root).into_iter().next()
}

/// The account numbers written under `root`.
fn numbers(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for name in NUMBERS {
        let Ok(text) = fs::read_to_string(root.join(name)) else { continue };
        for key in ["auth_uin", "default_uin", "last_login_uin", "uin"] {
            for value in values_of(&text, key) {
                let value = value.trim().to_string();
                if !value.is_empty() && !found.contains(&value) {
                    found.push(value);
                }
            }
        }
    }
    found
}

/// The phone's own names, as the app may have written them down.
fn names(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for name in NUMBERS {
        let Ok(text) = fs::read_to_string(root.join(name)) else { continue };
        for key in ["imei", "device_id", "android_id", "deviceid"] {
            for value in values_of(&text, key) {
                let value = value.trim().to_string();
                if !value.is_empty() && !found.contains(&value) {
                    found.push(value);
                }
            }
        }
    }
    found
}

/// Every value written under `key` in the small XML the app keeps its
/// settings in, in either of the two shapes it writes them.
fn values_of(text: &str, key: &str) -> Vec<String> {
    let mut found = Vec::new();
    // `<string name="auth_uin">1234</string>`
    let open = format!("name=\"{key}\"");
    let mut at = 0;
    while let Some(seen) = text[at..].find(&open) {
        let from = at + seen + open.len();
        let rest = &text[from..];
        // `<long name="x" value="1234" />` keeps it in an attribute.
        if let Some(value) = rest.trim_start().strip_prefix("value=\"") {
            if let Some(end) = value.find('"') {
                found.push(value[..end].to_string());
            }
        } else if let Some(value) = rest.split_once('>').map(|(_, after)| after) {
            if let Some(end) = value.find('<') {
                found.push(value[..end].to_string());
            }
        }
        at = from;
    }
    found
}

/// The seven letters the app keeps of a digest.
fn digest7(text: &str) -> String {
    let digest = Md5::digest(text.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..LENGTH.min(hex.len())].to_string()
}

/// Where the app keeps the number it works its passphrase out from, for
/// the page to say what it found.
pub fn setting_files(root: &Path) -> Vec<PathBuf> {
    NUMBERS.iter().map(|name| root.join(name)).filter(|path| path.is_file()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn somewhere(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skyward-pass-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("shared_prefs")).unwrap();
        root
    }

    #[test]
    fn the_number_and_its_digest_are_both_offered() {
        let root = somewhere("uin");
        fs::write(
            root.join("shared_prefs/auth_info_key_prefs.xml"),
            "<?xml version='1.0' encoding='utf-8' standalone='yes' ?>\n<map>\n    <string name=\"auth_uin\">1234567890</string>\n</map>\n",
        )
        .unwrap();
        let passes = passphrases(&root);
        assert!(passes.contains(&"1234567890".to_string()));
        assert!(passes.contains(&digest7("1234567890")), "got {passes:?}");
        assert_eq!(passes[0], digest7("1234567890"), "the digest comes first");
        assert_eq!(digest7("1234567890").len(), 7);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_number_kept_in_an_attribute_is_read_too() {
        let root = somewhere("attr");
        fs::write(
            root.join("shared_prefs/system_config_prefs.xml"),
            "<map>\n    <long name=\"default_uin\" value=\"987654321\" />\n</map>\n",
        )
        .unwrap();
        let passes = passphrases(&root);
        assert!(passes.contains(&"987654321".to_string()), "got {passes:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_phone_that_gave_its_name_has_that_tried_as_well() {
        let root = somewhere("imei");
        fs::write(
            root.join("shared_prefs/system_config_prefs.xml"),
            "<map>\n    <string name=\"auth_uin\">1234567890</string>\n    <string name=\"imei\">860000000000000</string>\n</map>\n",
        )
        .unwrap();
        let passes = passphrases(&root);
        assert!(passes.contains(&digest7("8600000000000001234567890")), "got {passes:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_without_the_number_offers_nothing() {
        let root = somewhere("empty");
        assert!(passphrases(&root).is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
