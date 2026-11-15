//! Giving back what a chat app keeps under a lock.
//!
//! Two things in the app's folder are worth opening and both are kept with
//! ciphers rather than with a database: the pictures it received, each one
//! turned over with a byte of its own, and the databases it writes its
//! messages into. Neither can be read by opening the file, and neither
//! needs the app to do it: what is needed is here, and nothing is written
//! that a test does not open again.

pub mod dat;
pub mod db;
pub mod key;

use anyhow::Result;
use std::path::{Path, PathBuf};

/// The app whose folder is opened, and the name it goes by in the system.
pub const PACKAGE: &str = "com.tencent.mm";

/// What giving a chat app's data back came to.
#[derive(Clone, Debug, Default)]
pub struct Given {
    /// The passphrase that opened the databases, once one did.
    pub passphrase: Option<String>,
    /// Databases put back, each with the pages it holds.
    pub databases: Vec<(String, u64)>,
    /// Pictures put back.
    pub pictures: u64,
    /// Files that were one of the two but did not come back whole.
    pub failed: u64,
}

impl Given {
    /// Whether anything at all was given back.
    pub fn is_empty(&self) -> bool {
        self.databases.is_empty() && self.pictures == 0
    }

    /// How much was given back, in words the page can use: the databases
    /// and the pictures, each counted.
    pub fn count(&self) -> (usize, u64) {
        (self.databases.len(), self.pictures)
    }
}

/// Which half of the work is under way, for the page to say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    /// Working out the passphrase and putting the databases back.
    Databases,
    /// Turning the pictures back.
    Pictures,
}

/// Every place the app keeps its folder, one per user of the phone.
pub fn roots() -> Vec<PathBuf> {
    dat::roots(PACKAGE)
}

/// Give back the pictures and the databases under `root`, written under
/// `into` with the places they came from kept.
///
/// The passphrase is worked out from what the app wrote about the account;
/// when no passphrase opens a database, the pictures are still given back,
/// because most of what a person wants from a phone that will not start is
/// in them.
pub fn unlock(root: &Path, into: &Path, stop: &dyn Fn() -> bool, tell: &mut dyn FnMut(Stage, u64)) -> Result<Given> {
    let mut given = Given::default();
    let passes = key::passphrases(root);
    // One database is enough to say which passphrase is the right one, and
    // finding it first is what keeps every other database from being tried
    // against every passphrase.
    // One database says both which passphrase is right and which shape the
    // rest are in: asking each one about every shape would stretch the
    // passphrase four times over for every file in the folder.
    let found = a_database(root)
        .and_then(|sample| passes.into_iter().find_map(|pass| db::opens(&sample, &pass).map(|scheme| (pass, scheme))));
    if let Some((pass, known)) = &found {
        tell(Stage::Databases, 0);
        let opened = db::decrypt_folder(root, into, pass, *known, stop, &mut |count| tell(Stage::Databases, count))?;
        given.databases = opened.databases;
        given.failed += opened.failed;
        given.passphrase = Some(pass.clone());
    }
    tell(Stage::Pictures, 0);
    let pictures = dat::restore_folder(root, into, stop, &mut |count| tell(Stage::Pictures, count))?;
    given.pictures = pictures.restored;
    given.failed += pictures.failed;
    Ok(given)
}

/// A database to try the passphrases against, the one the messages are in
/// first, since it is the one a rescue is asked for.
fn a_database(root: &Path) -> Option<PathBuf> {
    let mut any = None;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if !name.ends_with(".db") || name.ends_with("-wal") || name.ends_with("-shm") {
                continue;
            }
            if name == "EnMicroMsg.db" {
                return Some(path);
            }
            if any.is_none() {
                any = Some(path);
            }
        }
    }
    any
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn somewhere(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skyward-unlock-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_folder_of_pictures_alone_still_comes_back() {
        let root = somewhere("pictures");
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe0, 0, 0x10];
        jpeg.extend(std::iter::repeat_n(0x41u8, 200));
        jpeg.extend_from_slice(&[0xff, 0xd9]);
        let turned: Vec<u8> = jpeg.iter().map(|b| b ^ 0x37).collect();
        fs::create_dir_all(root.join("MicroMsg/abc/image2")).unwrap();
        fs::write(root.join("MicroMsg/abc/image2/1.dat"), &turned).unwrap();
        let into = root.join("out");
        let given = unlock(&root, &into, &|| false, &mut |_, _| {}).unwrap();
        assert_eq!(given.pictures, 1);
        assert!(given.passphrase.is_none(), "no database was there to open");
        assert!(into.join("pictures/MicroMsg/abc/image2/1.jpg").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nothing_to_give_back_is_said_rather_than_guessed() {
        let root = somewhere("empty");
        let into = root.join("out");
        let given = unlock(&root, &into, &|| false, &mut |_, _| {}).unwrap();
        assert!(given.is_empty());
        assert_eq!(given.count(), (0, 0));
        let _ = fs::remove_dir_all(&root);
    }

    /// What a phone that will not start still holds is the only copy of
    /// itself: opening the chat app's folder writes what it finds under
    /// `into` and leaves the folder it read byte for byte as it was —
    /// same bytes, same size, same time, same mode — so that a rescue can
    /// be tried again, or given up, without the phone paying for it.
    #[test]
    fn opening_a_folder_leaves_it_as_it_was() {
        use std::os::unix::fs::MetadataExt;

        let root = somewhere("untouched");
        let into = somewhere("untouched-into");
        // A database locked with the passphrase the account number works
        // out to, and a picture turned over with a byte of its own.
        let db = root.join("MicroMsg/abc/EnMicroMsg.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        fs::write(&db, include_bytes!("../tests/data/locked.db")).unwrap();
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe0, 0, 0x10];
        jpeg.extend(std::iter::repeat_n(0x41u8, 200));
        jpeg.extend_from_slice(&[0xff, 0xd9]);
        let turned: Vec<u8> = jpeg.iter().map(|b| b ^ 0x37).collect();
        let picture = root.join("MicroMsg/abc/image2/1.dat");
        fs::create_dir_all(picture.parent().unwrap()).unwrap();
        fs::write(&picture, &turned).unwrap();
        // The account the databases were locked for: `md5("10001")[:7]`
        // is `d89f3a3`, the passphrase they were sealed with.
        fs::create_dir_all(root.join("shared_prefs")).unwrap();
        fs::write(
            root.join("shared_prefs/auth_info_key_prefs.xml"),
            r#"<?xml version='1.0' encoding='utf-8' standalone='yes' ?><map><string name="auth_uin">10001</string></map>"#,
        )
        .unwrap();

        let before = tree(&root);
        let given = unlock(&root, &into, &|| false, &mut |_, _| {}).unwrap();

        // It did open what was there, so that what is compared after is
        // the folder as it was read rather than as it would be missed.
        assert_eq!(given.passphrase.as_deref(), Some("d89f3a3"));
        assert_eq!(given.databases.len(), 1, "the database was not opened");
        assert_eq!(given.pictures, 1, "the picture was not given back");
        let copy = into.join("databases/MicroMsg/abc/EnMicroMsg.db");
        let conn = rusqlite::Connection::open(&copy).unwrap();
        let said: String = conn.query_row("SELECT x FROM t", [], |row| row.get(0)).unwrap();
        assert_eq!(said, "hello");
        assert!(into.join("pictures/MicroMsg/abc/image2/1.jpg").is_file());

        // And nothing it read was touched, nor anything added beside it.
        assert_eq!(tree(&root), before, "the chat app's own folder was changed");
        let meta = fs::metadata(&picture).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o644, "the picture keeps the mode it had");
        let db_meta = fs::metadata(&db).unwrap();
        assert_eq!(db_meta.len(), 8192, "the database still holds the pages it came with");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&into);
    }

    /// Every file under `root`, with what it holds and what it says about
    /// itself, in a fixed order so two of these can be compared.
    fn tree(root: &std::path::Path) -> Vec<(String, u64, i64, u32, Vec<u8>)> {
        use std::os::unix::fs::MetadataExt;
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                let meta = entry.metadata().unwrap();
                if meta.is_dir() {
                    stack.push(path);
                    continue;
                }
                let name = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.push((name, meta.len(), meta.mtime(), meta.mode(), fs::read(&path).unwrap()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}
