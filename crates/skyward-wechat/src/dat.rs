//! Giving back the pictures a chat app keeps turned over.
//!
//! A picture the app received is not kept as a picture: it is kept as a
//! `.dat` whose bytes are turned over with one byte, sometimes behind a
//! short head. Nothing says which byte or which head, so both are worked
//! out from the file itself: the first bytes have to read as a picture's
//! mark and the last ones as that picture's ending, and only the pair that
//! satisfies both is taken.
//!
//! Working it out this way rather than from a table of the app's versions
//! is deliberate: the head has changed twice already, and a file that is
//! refused is a photograph that is gone.

use anyhow::{Context, Result};
use skyward_files::Ownership;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// How far into a file a head may run before the picture starts.
const LONGEST_HEAD: usize = 16;

/// How much of a file is read to tell what it is, at each end.
const LOOK: usize = 32;

/// A picture's shape, as its first bytes and its last ones tell it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Jpeg,
    Png,
    Gif,
    Webp,
    Heif,
}

impl Format {
    /// What a picture of this shape is called once it is back.
    pub fn ext(self) -> &'static str {
        match self {
            Format::Jpeg => "jpg",
            Format::Png => "png",
            Format::Gif => "gif",
            Format::Webp => "webp",
            Format::Heif => "heic",
        }
    }

    /// Whether `head` begins as a picture of this shape does.
    fn starts(self, head: &[u8]) -> bool {
        match self {
            Format::Jpeg => head.starts_with(&[0xff, 0xd8, 0xff]),
            Format::Png => head.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
            Format::Gif => head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a"),
            Format::Webp => head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP",
            Format::Heif => {
                head.len() >= 12
                    && &head[4..8] == b"ftyp"
                    && matches!(
                        &head[8..12],
                        b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" | b"avif" | b"avis"
                    )
            }
        }
    }

    /// Whether a file of `len` bytes, beginning as this shape does and
    /// ending as it does, is one.
    ///
    /// A picture with a mark at its end is known by that mark. One that
    /// says up front how long it is is known by that instead, which is why
    /// the shapes that cannot say either are not offered: a two-letter mark
    /// alone is met in rubble, and a photograph taken for rubble is lost.
    fn fits(self, head: &[u8], tail: &[u8], len: u64) -> bool {
        match self {
            Format::Jpeg => tail.ends_with(&[0xff, 0xd9]),
            Format::Png => tail.ends_with(&[0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82]),
            Format::Gif => tail.ends_with(&[0x3b]),
            Format::Webp => {
                head.len() >= 8 && u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as u64 + 8 == len
            }
            Format::Heif => {
                head.len() >= 4 && u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as u64 == len
            }
        }
    }
}

/// How a picture was kept: what to skip at the front, and what its bytes
/// were turned over with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Turn {
    pub skip: usize,
    pub key: u8,
    pub format: Format,
}

/// What a file is, from both its ends: none when it is not a picture.
pub fn turn_of(head: &[u8], tail: &[u8], len: u64) -> Option<Turn> {
    // A picture kept as it is needs no turning, and that is worth asking
    // first: most of them are.
    if let Some(format) = format_of(head) {
        if format.fits(head, tail, len) {
            return Some(Turn { skip: 0, key: 0, format });
        }
    }
    // A file turned over from its first byte has no head at all, which is
    // how the app kept pictures for years; one with a head is tried from
    // the shortest head up.
    for skip in 0..=LONGEST_HEAD {
        let Some(start) = head.get(skip..) else { break };
        if start.len() < 12 {
            break;
        }
        for key in 1u8..=u8::MAX {
            let plain: Vec<u8> = start.iter().map(|b| b ^ key).collect();
            let Some(format) = format_of(&plain) else { continue };
            // The other end has to agree: a mark alone can be met by
            // chance in rubble, a mark and an ending together almost never.
            let turned_tail: Vec<u8> = tail.iter().map(|b| b ^ key).collect();
            if format.fits(&plain, &turned_tail, len - skip as u64) {
                return Some(Turn { skip, key, format });
            }
        }
    }
    None
}

/// What the first bytes say a picture is, whatever it is called.
fn format_of(head: &[u8]) -> Option<Format> {
    const ALL: [Format; 5] = [Format::Jpeg, Format::Png, Format::Gif, Format::Webp, Format::Heif];
    ALL.into_iter().find(|format| format.starts(head))
}

/// What putting the chat app's pictures back came to.
#[derive(Clone, Debug, Default)]
pub struct Pictures {
    pub restored: u64,
    pub failed: u64,
}

/// Every picture under `root` that can be given back, written under `into`
/// with the places they came from kept and a name that says what they are.
///
/// A picture at `…/image2/ab/cd/1234.dat` that comes back as a JPEG is
/// written to `into/pictures/image2/ab/cd/1234.jpg`. A `.dat` that turns
/// out not to be a picture at all — a voice clip, a video, a thumbnail in
/// another shape — is counted and left alone.
pub fn restore_folder(
    root: &Path,
    into: &Path,
    stop: &dyn Fn() -> bool,
    tell: &mut dyn FnMut(u64),
) -> Result<Pictures> {
    let mut given = Pictures::default();
    let mut stack = vec![root.to_path_buf()];
    let mut buffer = vec![0u8; 1 << 20];
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
            if !kept_as_picture(&path) || meta.len() < 64 {
                continue;
            }
            let Some(turn) = look(&path) else {
                given.failed += 1;
                continue;
            };
            let Some(rest) = path.strip_prefix(root).ok() else { continue };
            let target = into.join("pictures").join(rest).with_extension(turn.format.ext());
            match give_back(&path, &target, turn, &mut buffer) {
                Ok(()) => {
                    given.restored += 1;
                    tell(given.restored);
                }
                Err(_) => given.failed += 1,
            }
        }
    }
    Ok(given)
}

/// Whether a file is one the app keeps a picture in: it names them `.dat`,
/// and a few builds name them `.bin` or nothing at all.
fn kept_as_picture(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        None => true,
        Some(ext) => matches!(ext.to_ascii_lowercase().as_str(), "dat" | "bin"),
    }
}

/// What a file is, from its two ends.
fn look(path: &Path) -> Option<Turn> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut head = [0u8; LONGEST_HEAD + LOOK];
    let read = file.read(&mut head).ok()?;
    let head = &head[..read];
    let mut tail = [0u8; LOOK];
    file.seek(SeekFrom::Start(len.saturating_sub(LOOK as u64))).ok()?;
    let read = file.read(&mut tail).ok()?;
    turn_of(head, &tail[..read], len)
}

/// Write `src` out at `dst` with its head skipped and its bytes turned
/// back, a block at a time.
fn give_back(src: &Path, dst: &Path, turn: Turn, buffer: &mut [u8]) -> Result<()> {
    let mut file = File::open(src).with_context(|| format!("open {}", src.display()))?;
    if let Some(parent) = dst.parent() {
        skyward_files::make_folders(parent, Ownership::Shared).with_context(|| format!("make {}", parent.display()))?;
    }
    let mut sink = BufWriter::new(skyward_files::make_file(dst, Ownership::Shared).with_context(|| format!("make {}", dst.display()))?);
    file.seek(SeekFrom::Start(turn.skip as u64))?;
    loop {
        let read = file.read(buffer)?;
        if read == 0 {
            break;
        }
        if turn.key != 0 {
            for byte in &mut buffer[..read] {
                *byte ^= turn.key;
            }
        }
        sink.write_all(&buffer[..read])?;
    }
    sink.flush()?;
    Ok(())
}

/// Every place a chat app keeps its own folder, one per user.
pub fn roots(package: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut push = |path: PathBuf| {
        let real = path.canonicalize().unwrap_or_else(|_| path.clone());
        if path.is_dir() && !found.contains(&real) {
            found.push(real);
        }
    };
    push(Path::new("/data/data").join(package));
    if let Ok(entries) = fs::read_dir("/data/user") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.parse::<u32>().is_ok() {
                push(entry.path().join(package));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn somewhere(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skyward-pictures-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn a_jpeg() -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0, 1];
        bytes.extend(std::iter::repeat_n(0x42u8, 200));
        bytes.extend_from_slice(&[0xff, 0xd9]);
        bytes
    }

    #[test]
    fn a_picture_kept_as_it_is_is_known_for_one() {
        let jpeg = a_jpeg();
        let turn = turn_of(&jpeg, &jpeg[jpeg.len() - LOOK..], jpeg.len() as u64).unwrap();
        assert_eq!(turn, Turn { skip: 0, key: 0, format: Format::Jpeg });
    }

    #[test]
    fn a_turned_picture_is_known_by_both_of_its_ends() {
        let jpeg = a_jpeg();
        for key in [0x37u8, 0x5a, 0x8c, 0xf3] {
            let turned: Vec<u8> = jpeg.iter().map(|b| b ^ key).collect();
            let turn = turn_of(&turned, &turned[turned.len() - LOOK..], turned.len() as u64).unwrap();
            assert_eq!(turn, Turn { skip: 0, key, format: Format::Jpeg }, "key {key:#x}");
        }
    }

    #[test]
    fn a_turned_picture_behind_a_head_is_still_found() {
        let jpeg = a_jpeg();
        let key = 0x37u8;
        let turned: Vec<u8> = jpeg.iter().map(|b| b ^ key).collect();
        for head in 1..=3usize {
            let mut kept = vec![0x07, 0x08, 0x00];
            kept.truncate(head);
            kept.extend_from_slice(&turned);
            let turn = turn_of(&kept, &kept[kept.len() - LOOK..], kept.len() as u64).unwrap();
            assert_eq!(turn, Turn { skip: head, key, format: Format::Jpeg }, "head {head}");
        }
    }

    #[test]
    fn a_voice_clip_is_not_taken_for_a_picture() {
        let mut clip = b"#!AMR\n".to_vec();
        clip.extend(std::iter::repeat_n(0x11u8, 400));
        assert_eq!(turn_of(&clip, &clip[clip.len() - LOOK..], clip.len() as u64), None);
    }

    #[test]
    fn giving_back_writes_every_byte_after_the_head() {
        let root = somewhere("give");
        let jpeg = a_jpeg();
        let key = 0x5au8;
        let mut kept = vec![0x07, 0x08, 0x00];
        kept.extend(jpeg.iter().map(|b| b ^ key));
        let src = root.join("1234.dat");
        fs::write(&src, &kept).unwrap();
        let into = root.join("out");
        let given = restore_folder(&root, &into, &|| false, &mut |_| {}).unwrap();
        assert_eq!(given.restored, 1);
        let out = into.join("pictures/1234.jpg");
        assert_eq!(fs::read(&out).unwrap(), jpeg, "back as it went in");
        let _ = fs::remove_dir_all(&root);
    }
}
