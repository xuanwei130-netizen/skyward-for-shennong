//! Opening the databases a chat app keeps locked.
//!
//! Every database the app writes is a SQLite file with each of its pages
//! put through a cipher: the first page's opening sixteen bytes are given
//! over to the salt the passphrase is stretched with, and every page keeps
//! room at its end for the mark it is read with and the check that proves
//! it whole. Nothing here links against SQLCipher — the two things it does
//! that matter, stretching a passphrase and putting a page back together,
//! are few enough to be done here, and a test opens what was written to
//! hold them to it.

use aes::cipher::{BlockDecryptMut, KeyIvInit};
use aes::Aes256;
use anyhow::{bail, Context, Result};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::Sha512;
use skyward_files::Ownership;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

type PageCipher = cbc::Decryptor<Aes256>;

/// How many bytes at the front of a file are the salt the passphrase is
/// stretched with.
const SALT: usize = 16;

/// The shapes a chat app's database has been kept in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scheme {
    /// Bytes to a page, which the file no longer says: its first page is a
    /// cipher too.
    pub page_size: usize,
    /// How many times the passphrase is stretched.
    pub kdf_iter: u32,
    /// Whether the key's second half and each page's check are made with
    /// SHA-512, as the fourth cipher does, or with SHA-1, as the third did.
    pub sha512: bool,
}

impl Scheme {
    /// The third cipher with the rounds phones were given: a kilobyte a
    /// page, four thousand rounds. What the chat app kept for years.
    pub const THIRD: Scheme = Scheme { page_size: 1024, kdf_iter: 4000, sha512: false };
    /// The third cipher at the page size it defaults to, with the same
    /// rounds.
    pub const THIRD_WIDE: Scheme = Scheme { page_size: 4096, kdf_iter: 4000, sha512: false };
    /// The third cipher at its own default rounds.
    pub const THIRD_DEFAULT: Scheme = Scheme { page_size: 4096, kdf_iter: 64000, sha512: false };
    /// The fourth cipher.
    pub const FOURTH: Scheme = Scheme { page_size: 4096, kdf_iter: 256000, sha512: true };

    /// Every shape worth trying, the one the app has kept longest first.
    pub const ALL: [Scheme; 4] = [Scheme::THIRD, Scheme::THIRD_WIDE, Scheme::FOURTH, Scheme::THIRD_DEFAULT];

    /// What a page keeps back at its end: the sixteen bytes it is read with
    /// and the check that it is whole, rounded up to whole blocks.
    pub fn reserve(self) -> usize {
        (SALT + self.hmac_len()).div_ceil(16) * 16
    }

    fn hmac_len(self) -> usize {
        if self.sha512 {
            64
        } else {
            20
        }
    }

    /// A name for the shape, as the page that reports it writes it.
    pub fn name(self) -> &'static str {
        match (self.page_size, self.kdf_iter, self.sha512) {
            (1024, 4000, false) => "1024 · 4000 · SHA-1",
            (4096, 4000, false) => "4096 · 4000 · SHA-1",
            (4096, 256000, true) => "4096 · 256000 · SHA-512",
            (4096, 64000, false) => "4096 · 64000 · SHA-1",
            _ => "unknown",
        }
    }
}

/// What opening a database came to.
#[derive(Clone, Debug)]
pub struct Opened {
    pub pages: u64,
    pub scheme: Scheme,
}

/// Whether `pass` opens the database at `path`, and in which shape.
///
/// The check at the end of the first page decides: it is made over
/// everything the page holds, so a passphrase that only nearly fits leaves
/// it standing.
pub fn opens(path: &Path, pass: &str) -> Option<Scheme> {
    Scheme::ALL.into_iter().find(|scheme| opens_as(path, pass, *scheme))
}

/// Whether `pass` opens the database at `path` in this shape in
/// particular.
///
/// Stretching a passphrase is the slowest thing here by far, so a folder
/// whose shape is already known is asked about that shape first rather
/// than about every one of them.
pub fn opens_as(path: &Path, pass: &str, scheme: Scheme) -> bool {
    let Ok(mut file) = File::open(path) else { return false };
    let mut salt = [0u8; SALT];
    if file.read_exact(&mut salt).is_err() {
        return false;
    }
    let mut page = vec![0u8; scheme.page_size];
    if file.seek(SeekFrom::Start(0)).is_err() || file.read_exact(&mut page).is_err() {
        return false;
    }
    let (key, mac) = keys(pass, &salt, scheme);
    read_page(&page, 1, scheme, &key, &mac).is_some()
}

/// Put `path` back as a database any program can open, at `out`.
///
/// Every page is checked before it is written. A page that does not check
/// would be read as rubble, so the copy stops there and the caller is told
/// which page it was.
pub fn decrypt(path: &Path, out: &Path, pass: &str, scheme: Scheme) -> Result<Opened> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut salt = [0u8; SALT];
    file.read_exact(&mut salt).with_context(|| format!("read {}", path.display()))?;
    // Back to the front: every page is read as it lies, the first one
    // included.
    file.seek(SeekFrom::Start(0)).with_context(|| format!("read {}", path.display()))?;
    let (key, mac) = keys(pass, &salt, scheme);
    let reserve = scheme.reserve();
    let padding = [0u8; 128];
    let mut page = vec![0u8; scheme.page_size];
    if let Some(parent) = out.parent() {
        skyward_files::make_folders(parent, Ownership::Shared).with_context(|| format!("make {}", parent.display()))?;
    }
    let mut sink = BufWriter::new(skyward_files::make_file(out, Ownership::Shared).with_context(|| format!("make {}", out.display()))?);
    let mut pages = 0u64;
    loop {
        match file.read_exact(&mut page) {
            Ok(()) => {}
            // A file that ends on a part page is one that was being written
            // when the phone stopped: what is whole is kept.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        }
        let pgno = pages as u32 + 1;
        let Some(plain) = read_page(&page, pgno, scheme, &key, &mac) else {
            bail!("page {pgno} of {} does not check", path.display());
        };
        if pgno == 1 {
            // The salt took the place of the mark every database begins
            // with, so it is put back before the rest of the page.
            sink.write_all(b"SQLite format 3\0")?;
        }
        sink.write_all(&plain)?;
        // What the page kept back for its own use is left as zeros: the
        // header already says that room is kept back.
        sink.write_all(&padding[..reserve])?;
        pages += 1;
    }
    sink.flush()?;
    if pages == 0 {
        bail!("{} holds no whole page", path.display());
    }
    Ok(Opened { pages, scheme })
}

/// One page of a database, put back; none when the passphrase or the shape
/// is not the one it was kept in.
fn read_page(page: &[u8], pgno: u32, scheme: Scheme, key: &[u8; 32], mac: &[u8; 32]) -> Option<Vec<u8>> {
    let reserve = scheme.reserve();
    let head = if pgno == 1 { SALT } else { 0 };
    let size = scheme.page_size.checked_sub(reserve)?.checked_sub(head)?;
    let iv_at = head + size;
    let mac_at = iv_at + SALT;
    if mac_at + scheme.hmac_len() > page.len() {
        return None;
    }
    if !checks(page, pgno, scheme, mac, head, mac_at) {
        return None;
    }
    let mut plain = page[head..iv_at].to_vec();
    let mut cipher = PageCipher::new_from_slices(key, &page[iv_at..iv_at + SALT]).ok()?;
    for block in plain.as_chunks_mut::<16>().0 {
        cipher.decrypt_block_mut(block.into());
    }
    Some(plain)
}

fn checks(page: &[u8], pgno: u32, scheme: Scheme, mac_key: &[u8], head: usize, mac_at: usize) -> bool {
    let want = &page[mac_at..mac_at + scheme.hmac_len()];
    if scheme.sha512 {
        let mut h = <Hmac<Sha512> as Mac>::new_from_slice(mac_key).expect("any key length is taken");
        h.update(&page[head..mac_at]);
        h.update(&pgno.to_le_bytes());
        h.finalize().into_bytes().as_slice() == want
    } else {
        let mut h = <Hmac<Sha1> as Mac>::new_from_slice(mac_key).expect("any key length is taken");
        h.update(&page[head..mac_at]);
        h.update(&pgno.to_le_bytes());
        h.finalize().into_bytes().as_slice() == want
    }
}

/// The key a passphrase and a salt come to, and the second key a page's
/// check is made with.
fn keys(pass: &str, salt: &[u8], scheme: Scheme) -> ([u8; 32], [u8; 32]) {
    let mut key = [0u8; 32];
    if scheme.sha512 {
        pbkdf2::pbkdf2_hmac::<Sha512>(pass.as_bytes(), salt, scheme.kdf_iter, &mut key);
    } else {
        pbkdf2::pbkdf2_hmac::<Sha1>(pass.as_bytes(), salt, scheme.kdf_iter, &mut key);
    }
    let turned: Vec<u8> = salt.iter().map(|b| b ^ 0x3a).collect();
    let mut mac = [0u8; 32];
    if scheme.sha512 {
        pbkdf2::pbkdf2_hmac::<Sha512>(&key, &turned, 2, &mut mac);
    } else {
        pbkdf2::pbkdf2_hmac::<Sha1>(&key, &turned, 2, &mut mac);
    }
    (key, mac)
}

/// What giving the chat app's data back came to.
#[derive(Clone, Debug, Default)]
pub struct Given {
    /// Databases put back, each with the pages it holds.
    pub databases: Vec<(String, u64)>,
    /// Pictures put back.
    pub pictures: u64,
    /// Files that were one of the two but did not come back whole.
    pub failed: u64,
}

/// Every database under `root` that `pass` opens, written under `into`
/// with the places they came from kept.
///
/// A database at `…/MicroMsg/abc/EnMicroMsg.db` comes out at
/// `into/databases/MicroMsg/abc/EnMicroMsg.db`. One that does not open is
/// left where it is: it is already in the copy of the whole folder, and a
/// half-opened database is worse than none.
pub fn decrypt_folder(
    root: &Path,
    into: &Path,
    pass: &str,
    known: Scheme,
    stop: &dyn Fn() -> bool,
    tell: &mut dyn FnMut(u64),
) -> Result<Given> {
    let mut given = Given::default();
    let mut seen = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if stop() {
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if stop() {
                break;
            }
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
            // A database met twice through another name is put back once.
            if !seen.insert((meta.dev(), meta.ino())) {
                continue;
            }
            let scheme = if opens_as(&path, pass, known) { known } else {
                match opens(&path, pass) {
                    Some(scheme) => scheme,
                    None => continue,
                }
            };
            let Some(rest) = path.strip_prefix(root).ok().map(PathBuf::from) else { continue };
            let target = into.join("databases").join(&rest);
            match decrypt(&path, &target, pass, scheme) {
                Ok(opened) => {
                    given.databases.push((rest.to_string_lossy().into_owned(), opened.pages));
                    tell(given.databases.len() as u64);
                }
                Err(_) => {
                    given.failed += 1;
                    let _ = fs::remove_file(&target);
                }
            }
        }
    }
    Ok(given)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn somewhere(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skyward-wechat-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn a_database(path: &Path, page_size: usize) {
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch(&format!(
            "PRAGMA page_size = {page_size}; CREATE TABLE t(a TEXT, b INTEGER); INSERT INTO t VALUES('hello', 42);"
        ))
        .unwrap();
    }

    /// Pages sealed as the app seals them, so that what is written can be
    /// read back by the very same rules the other way round.
    fn seal(plain: &[u8], scheme: Scheme, pass: &str, salt: &[u8; SALT]) -> Vec<u8> {
        use aes::cipher::BlockEncryptMut;

        let reserve = scheme.reserve();
        let (key, mac) = keys(pass, salt, scheme);
        let mut out: Vec<u8> = Vec::new();
        for (at, page) in plain.chunks_exact(scheme.page_size).enumerate() {
            let pgno = at as u32 + 1;
            let head = if pgno == 1 { SALT } else { 0 };
            let size = scheme.page_size - reserve - head;
            let mut body = page[head..head + size].to_vec();
            // Every page is read with a mark of its own; a fixed one keeps
            // what the test writes the same from run to run.
            let iv = [0x5au8; 16];
            let mut cipher = cbc::Encryptor::<Aes256>::new_from_slices(&key, &iv).unwrap();
            for block in body.as_chunks_mut::<16>().0 {
                cipher.encrypt_block_mut(block.into());
            }
            if pgno == 1 {
                out.extend_from_slice(salt);
            }
            let start = out.len();
            out.extend_from_slice(&body);
            out.extend_from_slice(&iv);
            let tag = if scheme.sha512 {
                let mut h = <Hmac<Sha512> as Mac>::new_from_slice(&mac).unwrap();
                h.update(&out[start..]);
                h.update(&pgno.to_le_bytes());
                h.finalize().into_bytes().to_vec()
            } else {
                let mut h = <Hmac<Sha1> as Mac>::new_from_slice(&mac).unwrap();
                h.update(&out[start..]);
                h.update(&pgno.to_le_bytes());
                h.finalize().into_bytes().to_vec()
            };
            out.extend_from_slice(&tag);
            out.resize((at + 1) * scheme.page_size, 0);
        }
        out
    }

    #[test]
    fn a_database_sealed_and_given_back_is_the_one_that_went_in() {
        for scheme in [Scheme::THIRD, Scheme::FOURTH] {
            let root = somewhere(&format!("round-{}", scheme.page_size));
            let plain = root.join("plain.db");
            a_database(&plain, scheme.page_size);
            let before = fs::read(&plain).unwrap();
            let salt = [0x11u8; SALT];
            let sealed = seal(&before, scheme, "abcdefg", &salt);
            let kept = root.join("sealed.db");
            fs::write(&kept, &sealed).unwrap();

            assert_eq!(opens(&kept, "abcdefg"), Some(scheme), "{}", scheme.name());
            assert_eq!(opens(&kept, "abcdefgh"), None, "a passphrase one letter off");

            let out = root.join("out.db");
            let opened = decrypt(&kept, &out, "abcdefg", scheme).unwrap();
            assert!(opened.pages >= 2);
            let got = fs::read(&out).unwrap();
            assert_eq!(got.len(), before.len());
            assert_eq!(&got[..16], b"SQLite format 3\0");
            // What the cipher kept back at each page's end comes out as the
            // room the header already says is kept back, so every byte the
            // database wrote is where it was.
            let usable = scheme.page_size - scheme.reserve();
            for (at, page) in before.chunks_exact(scheme.page_size).enumerate() {
                let head = if at == 0 { SALT } else { 0 };
                let from = at * scheme.page_size;
                assert_eq!(
                    &got[from + head..from + usable],
                    &page[head..usable],
                    "page {} of {}",
                    at + 1,
                    scheme.name()
                );
            }
            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn a_page_that_does_not_check_is_not_written_out() {
        let root = somewhere("tamper");
        let plain = root.join("plain.db");
        a_database(&plain, Scheme::THIRD.page_size);
        let before = fs::read(&plain).unwrap();
        let salt = [0x22u8; SALT];
        let mut sealed = seal(&before, Scheme::THIRD, "abcdefg", &salt);
        // One byte of the second page's ciphertext is turned over.
        sealed[1024 + 40] ^= 0xff;
        let kept = root.join("sealed.db");
        fs::write(&kept, &sealed).unwrap();
        let out = root.join("out.db");
        assert!(decrypt(&kept, &out, "abcdefg", Scheme::THIRD).is_err(), "a page turned over is not one to keep");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_room_a_page_keeps_back_follows_the_check_it_carries() {
        assert_eq!(Scheme::THIRD.reserve(), 48, "16 of mark and 20 of check, rounded to whole blocks");
        assert_eq!(Scheme::FOURTH.reserve(), 80, "16 of mark and 64 of check");
    }

    #[test]
    fn a_folder_is_walked_and_only_what_opens_is_put_back() {
        let root = somewhere("folder");
        let inner = root.join("MicroMsg/abc");
        fs::create_dir_all(&inner).unwrap();
        let plain = inner.join("EnMicroMsg.db");
        a_database(&plain, Scheme::THIRD.page_size);
        let before = fs::read(&plain).unwrap();
        let sealed = seal(&before, Scheme::THIRD, "abcdefg", &[0x33u8; SALT]);
        fs::write(&plain, &sealed).unwrap();
        fs::write(inner.join("message.db"), b"not a database at all").unwrap();
        let into = root.join("out");
        let given = decrypt_folder(&root, &into, "abcdefg", Scheme::THIRD, &|| false, &mut |_| {}).unwrap();
        assert_eq!(given.databases.len(), 1);
        assert_eq!(given.databases[0].0, "MicroMsg/abc/EnMicroMsg.db");
        assert!(into.join("databases/MicroMsg/abc/EnMicroMsg.db").is_file(), "kept with its places");
        let _ = fs::remove_dir_all(&root);
    }

    /// A database as SQLCipher itself wrote it, sealed with `abcdefg` under
    /// the fourth cipher: what is read back here is read back by the same
    /// rules another program's copy of the file obeys.
    const SEALED: &[u8] = include_bytes!("../tests/data/wechat.db");

    #[test]
    fn a_database_sqlcipher_wrote_opens_and_reads() {
        let root = somewhere("real");
        let kept = root.join("sealed.db");
        fs::write(&kept, SEALED).unwrap();
        let scheme = opens(&kept, "abcdefg").expect("the passphrase it was sealed with");
        assert_eq!(scheme, Scheme::FOURTH, "{}", scheme.name());
        let out = root.join("out.db");
        let opened = decrypt(&kept, &out, "abcdefg", scheme).unwrap();
        assert_eq!(opened.pages, 2);
        let db = rusqlite::Connection::open(&out).unwrap();
        let hello: String = db.query_row("SELECT a FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(hello, "hello");
        assert_eq!(opens(&kept, "abcdefgh"), None, "a passphrase one letter off opens nothing");
        let _ = fs::remove_dir_all(&root);
    }
}
