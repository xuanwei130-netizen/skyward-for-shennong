//! Finding what is worth saving on a phone that no longer starts, and
//! copying it out with the places it came from kept.
//!
//! Nothing here opens a database or decodes a picture: the files are found
//! and copied as they lie, so that a program on another machine can make
//! sense of them once they are there. That keeps this side free of every
//! format that changes between one release of an app and the next.

use anyhow::{Context, Result};
use skyward_files::Ownership;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

/// Folders whose contents are never worth the time it takes to copy them:
/// another copy of what is held elsewhere, or what is built again on the
/// next start.
const SKIPPED: &[&str] = &[
    "cache",
    "code_cache",
    "no_backup",
    "thumbnails",
    ".thumbnails",
    ".trash",
    "lost+found",
    ".gs",
    ".dtr",
    // What an earlier rescue copied out, so that looking again does not
    // find its own work.
    "skywardrescue",
];

/// Pictures smaller than this without a telling extension are icons or
/// previews rather than photographs, and are left where they are.
const SMALLEST_UNNAMED: u64 = 8 * 1024;

/// How many files are looked at between two tellings of how far the walk
/// has come.
const TELL_EVERY: u64 = 200;

/// What a rescue takes out.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    /// Pictures, wherever they lie: the camera's, a chat app's, a
    /// browser's downloads.
    Image,
    /// Films and clips, from the camera, a chat app or a download.
    Video,
    /// Documents and texts.
    Document,
    /// A chat app's whole folder, taken as it lies.
    Wechat,
}

impl Kind {
    /// The folder of the copy this kind is written into.
    pub fn folder(self) -> &'static str {
        match self {
            Kind::Image => "images",
            Kind::Video => "videos",
            Kind::Document => "documents",
            Kind::Wechat => "wechat",
        }
    }
}

/// One file the walk kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub size: u64,
    /// Seconds since the epoch; zero where the phone cannot tell.
    pub modified: i64,
    pub kind: Kind,
}

/// How far a walk or a copy has come.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    /// Files looked at, whether kept or not.
    pub seen: u64,
    /// Files kept.
    pub found: u64,
    /// Bytes of the files kept.
    pub bytes: u64,
    /// The folder the walk is in, or the file being copied.
    pub at: String,
}

/// What a copy out ended with.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub files: u64,
    pub bytes: u64,
    /// Files that could not be copied, counted rather than stopping the
    /// rest: one unreadable file must not cost the whole rescue.
    pub failed: u64,
    pub into: PathBuf,
}

/// The places the owner's own files live, one per user of the phone and
/// one per card in it.
///
/// A phone that keeps a second space keeps a second user, and its pictures
/// are under `/data/media/<user>` as the first user's are under
/// `/data/media/0`. Missing them is the commonest way a rescue comes back
/// short.
pub fn media_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(entries) = fs::read_dir("/data/media") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.parse::<u32>().is_ok() && entry.path().is_dir() {
                roots.push(entry.path());
            }
        }
    }
    if let Ok(entries) = fs::read_dir("/storage") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // `/storage/emulated` holds the same files as `/data/media`,
            // and `/storage/self` points at the running user's own.
            if name == "emulated" || name == "self" {
                continue;
            }
            if entry.path().is_dir() {
                roots.push(entry.path());
            }
        }
    }
    roots
}

/// Every place a chat app keeps its folders, one per user.
pub fn chat_roots(package: &str) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut push = |path: PathBuf| {
        // `/data/data` is very often the same folder as `/data/user/0`, by
        // link or by bind, and walking it twice would take twice as long
        // for the same files.
        let real = path.canonicalize().unwrap_or_else(|_| path.clone());
        if path.is_dir() && !roots.contains(&real) {
            roots.push(real);
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
    roots
}

/// The pictures and documents under `roots`, each file once.
pub fn scan_media(
    roots: &[PathBuf],
    kinds: &[Kind],
    cancel: &AtomicBool,
    tell: &mut dyn FnMut(Progress),
) -> Result<Vec<Found>> {
    walk(roots, None, kinds, cancel, tell)
}

/// Everything under `roots`, as a chat app keeps it: no file is judged
/// worth keeping or not, because what an app needs to be read again is not
/// for this side to know.
pub fn scan_whole(
    roots: &[PathBuf],
    kind: Kind,
    cancel: &AtomicBool,
    tell: &mut dyn FnMut(Progress),
) -> Result<Vec<Found>> {
    walk(roots, Some(kind), &[], cancel, tell)
}

fn walk(
    roots: &[PathBuf],
    whole: Option<Kind>,
    kinds: &[Kind],
    cancel: &AtomicBool,
    tell: &mut dyn FnMut(Progress),
) -> Result<Vec<Found>> {
    let mut found = Vec::new();
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut progress = Progress::default();
    let mut stack: Vec<PathBuf> = roots.iter().rev().cloned().collect();
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            // A folder that cannot be read is passed over rather than
            // ending the walk: a rescue takes what it can reach.
            continue;
        };
        progress.at = dir.to_string_lossy().into_owned();
        for entry in entries.flatten() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                if !skipped(&path) {
                    stack.push(path);
                }
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            progress.seen += 1;
            if progress.seen % TELL_EVERY == 0 {
                tell(progress.clone());
            }
            if meta.len() == 0 {
                continue;
            }
            // A file reachable twice — by a link, a bind, a second name —
            // is kept once, under the name it was met by first.
            if !seen.insert((meta.dev(), meta.ino())) {
                continue;
            }
            let kind = match whole {
                Some(kind) => kind,
                None => match wanted(&path, kinds, meta.len()) {
                    Some(kind) => kind,
                    None => continue,
                },
            };
            progress.found += 1;
            progress.bytes += meta.len();
            found.push(Found { path, size: meta.len(), modified: modified(&meta), kind });
        }
    }
    tell(progress);
    Ok(found)
}

/// Whether a file is one of `kinds`, by its name where the name tells, and
/// by its first bytes where it does not.
///
/// A name that tells of another kind is believed at once: reading the head
/// of every picture on the phone to find the films among them would cost
/// more than the few files a wrong name hides.
fn wanted(path: &Path, kinds: &[Kind], len: u64) -> Option<Kind> {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_ascii_lowercase();
        if let Some(kind) = by_extension(&ext) {
            return kinds.contains(&kind).then_some(kind);
        }
    }
    // A file with no telling name is read, but only past a size below
    // which nothing but an icon or a preview lies.
    if len < SMALLEST_UNNAMED {
        return None;
    }
    let kind = by_head(path)?;
    kinds.contains(&kind).then_some(kind)
}

/// Every name a picture may go by. Long on purpose: a rescue is asked for
/// once, and a picture left behind because of how it was named is gone.
const PICTURE_NAMES: &[&str] = &[
    // What a phone and its apps write every day.
    "jpg", "jpeg", "jpe", "jfif", "jif", "jfi", "png", "apng", "gif", "webp", "bmp", "dib", //
    // The newer encodings.
    "heic", "heif", "hif", "avif", "jxl", "jp2", "j2k", "jpx", "jpf", "jpm", //
    // What a camera writes beside its pictures.
    "dng", "cr2", "cr3", "crw", "nef", "nrw", "arw", "sr2", "srf", "raf", "rw2", "orf", "pef", "srw",
    "raw", "3fr", "fff", "iiq", "k25", "kdc", "mrw", "x3f", //
    // Icons, layers, drawings and the older pictures.
    "tif", "tiff", "ico", "cur", "psd", "psb", "xcf", "tga", "pcx", "ppm", "pgm", "pbm", "pnm", "exr",
    "hdr", "dds", "wdp", "jxr", "svg", "svgz", "xbm", "xpm",
];

/// Every name a film may go by.
const FILM_NAMES: &[&str] = &[
    "mp4", "m4v", "mov", "qt", "3gp", "3g2", "3gpp", "3gpp2", "3gp2", "mkv", "mk3d", "webm", "avi",
    "divx", "flv", "f4v", "wmv", "asf", "mpg", "mpeg", "mpe", "m2v", "mpv", "m1v", "ts", "m2ts", "mts",
    "tp", "trp", "vob", "ogv", "ogm", "rm", "rmvb", "rv", "amv", "mxf", "dv", "y4m", "mj2", "m2p", "m2t",
    "wtv", "h264", "h265", "hevc", "264", "265",
];

/// Every name a document may go by.
const DOCUMENT_NAMES: &[&str] = &[
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "md", "csv", "rtf", "odt", "ods", "odp",
    "epub", "json", "xml", "log", "ofd", "wps", "et", "dps", "xps", "oxps",
];

fn by_extension(ext: &str) -> Option<Kind> {
    if PICTURE_NAMES.contains(&ext) {
        Some(Kind::Image)
    } else if FILM_NAMES.contains(&ext) {
        Some(Kind::Video)
    } else if DOCUMENT_NAMES.contains(&ext) {
        Some(Kind::Document)
    } else {
        None
    }
}

/// How much of a file is read to tell what it is: enough for the brands a
/// film lists after the one it is named by.
const HEAD: usize = 64;

/// What the first bytes say a file is, whatever it is called.
fn by_head(path: &Path) -> Option<Kind> {
    let mut head = [0u8; HEAD];
    let mut file = File::open(path).ok()?;
    let read = file.read(&mut head).ok()?;
    let head = &head[..read];
    if head.len() < 4 {
        return None;
    }
    if picture_head(head) {
        return Some(Kind::Image);
    }
    if film_head(head) {
        return Some(Kind::Video);
    }
    // PDF, and the compound document the older Office files are kept in.
    if head.starts_with(b"%PDF") || head.starts_with(&[0xd0, 0xcf, 0x11, 0xe0]) {
        return Some(Kind::Document);
    }
    None
}

/// What the first bytes say of a picture, whatever it is called.
fn picture_head(head: &[u8]) -> bool {
    // JPEG, PNG, GIF.
    if head.starts_with(&[0xff, 0xd8, 0xff])
        || head.starts_with(&[0x89, 0x50, 0x4e, 0x47])
        || head.starts_with(b"GIF8")
    {
        return true;
    }
    // WebP, and the pictures of the HEIF family, both of which announce
    // themselves by a brand a few bytes in.
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return true;
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        if let Ok(brand) = <&[u8; 4]>::try_from(&head[8..12]) {
            if PICTURE_BRANDS.contains(&brand) {
                return true;
            }
        }
    }
    // TIFF, which the cameras' raw files are under the surface, and the
    // two that keep a mark of their own before their TIFF half.
    if head.starts_with(b"II\x2a\x00")
        || head.starts_with(b"MM\x00\x2a")
        || head.starts_with(b"IIRO")
        || head.starts_with(b"IIU\x00")
        || head.starts_with(b"FUJIFILMCCD-RAW")
        || head.starts_with(b"FOVb")
    {
        return true;
    }
    // JPEG 2000, Photoshop, icons, OpenEXR, Radiance, DDS, JPEG XL, and
    // drawings kept as text.
    head.starts_with(&[0x00, 0x00, 0x00, 0x0c, 0x6a, 0x50, 0x20, 0x20])
        || head.starts_with(b"8BPS")
        || head.starts_with(&[0x00, 0x00, 0x01, 0x00])
        || head.starts_with(&[0x76, 0x2f, 0x31, 0x01])
        || head.starts_with(b"#?RADIANCE")
        || head.starts_with(b"DDS ")
        || head.starts_with(&[0xff, 0x0a])
        || head.windows(4).any(|window| window == b"<svg")
}

/// The brands of the ISO family that stand for a film.
const FILM_BRANDS: &[&[u8; 4]] = &[
    b"isom", b"iso2", b"iso3", b"iso4", b"iso5", b"iso6", b"iso8", b"mp41", b"mp42", b"mp71", b"avc1",
    b"dash", b"M4V ", b"M4VH", b"M4VP", b"mmp4", b"MSNV", b"NDAS", b"qt  ", b"3gp4", b"3gp5", b"3gp6",
    b"3gp7", b"3gr6", b"3gs6", b"3ge6", b"3gg6", b"3g2a", b"3g2b", b"3g2c", b"F4V ", b"F4P ", b"MSDH",
];

/// The brands of the ISO family that stand for a picture.
const PICTURE_BRANDS: &[&[u8; 4]] = &[
    b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1", b"msf1", b"avif",
    b"avis",
];

/// What the first bytes say of a film, whatever it is called.
fn film_head(head: &[u8]) -> bool {
    // The ISO family: MP4, QuickTime, the 3GPP files. The first brand is
    // the one a file is named by, and a brand of its own may still list a
    // known one among those it can be read by.
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        if let Ok(brand) = <&[u8; 4]>::try_from(&head[8..12]) {
            if FILM_BRANDS.contains(&brand) {
                return true;
            }
        }
        let rest = head.get(16..).unwrap_or_default();
        for at in (0..rest.len().saturating_sub(3)).step_by(4) {
            if let Ok(brand) = <&[u8; 4]>::try_from(&rest[at..at + 4]) {
                if FILM_BRANDS.contains(&brand) {
                    return true;
                }
            }
        }
    }
    // Matroska and WebM, AVI, Flash, ASF, MPEG, RealMedia, MXF.
    head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3])
        || (head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"AVI ")
        || head.starts_with(b"FLV\x01")
        || head.starts_with(&[0x30, 0x26, 0xb2, 0x75, 0x8e, 0x66, 0xcf, 0x11])
        || head.starts_with(&[0x00, 0x00, 0x01, 0xba])
        || head.starts_with(&[0x00, 0x00, 0x01, 0xb3])
        || head.starts_with(b".RMF")
        || head.starts_with(&[0x06, 0x0e, 0x2b, 0x34])
}

fn skipped(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SKIPPED.contains(&name.to_ascii_lowercase().as_str()))
}

fn modified(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs() as i64)
}

/// Copy `found` into `into`, each file under the folder of its kind and
/// the places it came from, and write a list of what was taken so the
/// copy can be checked against the phone.
pub fn export(
    found: &[Found],
    into: &Path,
    cancel: &AtomicBool,
    tell: &mut dyn FnMut(Progress),
) -> Result<Summary> {
    // Shared storage gives every entry the owner, the label and the
    // permissions of the folder it is made in; one left as root is one the
    // system cannot read afterwards.
    skyward_files::make_folders(into, Ownership::Shared).with_context(|| format!("make {}", into.display()))?;
    let list = File::create(into.join("MANIFEST.csv")).context("make the list of what is taken")?;
    let mut list = BufWriter::new(list);
    writeln!(list, "source,size,modified,target")?;
    let mut summary = Summary { into: into.to_path_buf(), ..Summary::default() };
    let mut progress = Progress { found: found.len() as u64, ..Progress::default() };
    for (taken, file) in found.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        progress.at = file.path.to_string_lossy().into_owned();
        progress.seen = taken as u64;
        tell(progress.clone());
        let target = free_name(&into.join(place(&file.path, file.kind)));
        let Some(parent) = target.parent() else { continue };
        if skyward_files::make_folders(parent, Ownership::Shared).is_err() {
            summary.failed += 1;
            continue;
        }
        let copied: Result<u64> = (|| {
            let mut source = File::open(&file.path)?;
            let mut sink = skyward_files::make_file(&target, Ownership::Shared)?;
            Ok(std::io::copy(&mut source, &mut sink)?)
        })();
        match copied {
            Ok(copied) => {
                summary.files += 1;
                summary.bytes += copied;
                writeln!(
                    list,
                    "{},{},{},{}",
                    quoted(&file.path.to_string_lossy()),
                    file.size,
                    file.modified,
                    quoted(&target.strip_prefix(into).unwrap_or(&target).to_string_lossy())
                )?;
            }
            // One file that cannot be read is counted, and the rest goes on.
            Err(_) => summary.failed += 1,
        }
    }
    list.flush().context("finish the list of what is taken")?;
    progress.at.clear();
    tell(progress);
    Ok(summary)
}

/// Where a file goes inside the copy: under its kind, then the user or the
/// card it came from, then the folders it lay in.
fn place(path: &Path, kind: Kind) -> PathBuf {
    Path::new(kind.folder()).join(inner(path))
}

fn inner(path: &Path) -> PathBuf {
    let parts: Vec<String> = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let user = |at: usize| parts.get(at).filter(|name| name.parse::<u32>().is_ok()).cloned();
    // `/data/media/<user>/…` and `/data/user/<user>/<app>/…`.
    if parts.len() >= 3 && parts[0] == "data" && parts[1] == "media" {
        if let Some(user) = user(2) {
            return under(format!("user{user}"), &parts[3..]);
        }
    }
    if parts.len() >= 4 && parts[0] == "data" && parts[1] == "user" {
        if let Some(user) = user(2) {
            return under(format!("user{user}"), &parts[3..]);
        }
    }
    if parts.len() >= 3 && parts[0] == "data" && parts[1] == "data" {
        return under("user0".to_string(), &parts[2..]);
    }
    if parts.len() >= 2 && parts[0] == "storage" {
        return under(format!("sdcard-{}", parts[1]), &parts[2..]);
    }
    under("other".to_string(), &parts)
}

fn under(head: String, rest: &[String]) -> PathBuf {
    let mut path = PathBuf::from(head);
    for name in rest {
        path.push(name);
    }
    path
}

/// A name nothing else holds yet, so that two files of the same name from
/// two folders do not cost one of them.
fn free_name(wanted: &Path) -> PathBuf {
    if !wanted.exists() {
        return wanted.to_path_buf();
    }
    let stem = wanted.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = wanted.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 1.. {
        let name = match &ext {
            Some(ext) => format!("{stem}-{n}.{ext}"),
            None => format!("{stem}-{n}"),
        };
        let beside = wanted.with_file_name(name);
        if !beside.exists() {
            return beside;
        }
    }
    wanted.to_path_buf()
}

fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn somewhere(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skyward-rescue-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_name_tells_a_picture_a_film_and_a_document_apart() {
        assert_eq!(by_extension("jpg"), Some(Kind::Image));
        assert_eq!(by_extension("heic"), Some(Kind::Image));
        assert_eq!(by_extension("dng"), Some(Kind::Image), "a camera's own file");
        assert_eq!(by_extension("svg"), Some(Kind::Image), "a drawing kept as text");
        assert_eq!(by_extension("mp4"), Some(Kind::Video));
        assert_eq!(by_extension("mkv"), Some(Kind::Video));
        assert_eq!(by_extension("rmvb"), Some(Kind::Video));
        assert_eq!(by_extension("pdf"), Some(Kind::Document));
        assert_eq!(by_extension("docx"), Some(Kind::Document));
        assert_eq!(by_extension("apk"), None);
        assert_eq!(by_extension("m4a"), None, "a song is neither of the three");
    }

    #[test]
    fn every_name_of_a_picture_and_a_film_is_kept_once() {
        // A name listed twice would be looked for twice in the same list.
        let mut names: Vec<&str> = PICTURE_NAMES.iter().chain(FILM_NAMES).copied().collect();
        let all = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all, "a name is listed twice");
        assert!(PICTURE_NAMES.iter().all(|name| by_extension(name) == Some(Kind::Image)));
        assert!(FILM_NAMES.iter().all(|name| by_extension(name) == Some(Kind::Video)));
        assert!(DOCUMENT_NAMES.iter().all(|name| by_extension(name) == Some(Kind::Document)));
        // And no name is taken for two kinds at once.
        for name in PICTURE_NAMES {
            assert!(!FILM_NAMES.contains(name) && !DOCUMENT_NAMES.contains(name), "{name}");
        }
    }

    #[test]
    fn a_picture_without_a_name_is_known_by_its_first_bytes() {
        let root = somewhere("head");
        let jpeg = root.join("a.bin");
        write(&jpeg, &[0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(by_head(&jpeg), Some(Kind::Image));
        let heic = root.join("b.bin");
        write(&heic, b"\0\0\0\x18ftypheic\0\0\0\0");
        assert_eq!(by_head(&heic), Some(Kind::Image));
        let pdf = root.join("c.bin");
        write(&pdf, b"%PDF-1.7\n");
        assert_eq!(by_head(&pdf), Some(Kind::Document));
        let nothing = root.join("d.bin");
        write(&nothing, b"just words\n");
        assert_eq!(by_head(&nothing), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_film_without_a_name_is_known_by_its_first_bytes() {
        let root = somewhere("filmhead");
        let mp4 = root.join("a.dat");
        write(&mp4, b"\0\0\0\x18ftypisom\0\0\x02\0isomiso2avc1mp41");
        assert_eq!(by_head(&mp4), Some(Kind::Video));
        let own_brand = root.join("b.dat");
        write(&own_brand, b"\0\0\0\x20ftypmp42\0\0\0\0mp42isom\0\0\0\0");
        assert_eq!(by_head(&own_brand), Some(Kind::Video));
        let mov = root.join("c.dat");
        write(&mov, b"\0\0\0\x14ftypqt  \0\0\0\0\0\0\0\0");
        assert_eq!(by_head(&mov), Some(Kind::Video));
        let webm = root.join("d.dat");
        write(&webm, &[0x1a, 0x45, 0xdf, 0xa3, 0x9f, 0x42, 0x86, 0x81, 0x01, 0x42, 0xf7, 0x81]);
        assert_eq!(by_head(&webm), Some(Kind::Video));
        let avi = root.join("e.dat");
        write(&avi, b"RIFF\x24\x08\0\0AVI LIST");
        assert_eq!(by_head(&avi), Some(Kind::Video));
        let flash = root.join("f.dat");
        write(&flash, b"FLV\x01\x05\0\0\0\x09");
        assert_eq!(by_head(&flash), Some(Kind::Video));
        // A picture is not taken for a film by the same test.
        let heic = root.join("g.dat");
        write(&heic, b"\0\0\0\x18ftypmif1\0\0\0\0");
        assert_eq!(by_head(&heic), Some(Kind::Image));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_film_is_kept_when_it_is_asked_for() {
        let root = somewhere("filmwalk");
        write(&root.join("DCIM/Camera/VID_20251114.mp4"), b"\0\0\0\x18ftypisom");
        write(&root.join("DCIM/Camera/a.jpg"), &[0xff, 0xd8, 0xff, 1]);
        write(&root.join("Movies/old.rmvb"), b".RMF\0\0\0\x12");
        let cancel = AtomicBool::new(false);
        let found = scan_media(std::slice::from_ref(&root), &[Kind::Video], &cancel, &mut |_| {}).unwrap();
        let names: Vec<String> = found
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "got {names:?}");
        assert!(names.contains(&"VID_20251114.mp4".to_string()));
        assert!(names.contains(&"old.rmvb".to_string()));
        assert!(found.iter().all(|f| f.kind == Kind::Video));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_walk_keeps_pictures_and_documents_and_their_sizes() {
        let root = somewhere("walk");
        write(&root.join("DCIM/Camera/a.jpg"), &[0xff, 0xd8, 0xff, 1, 2, 3]);
        write(&root.join("Documents/b.pdf"), b"%PDF-1.7 hello");
        write(&root.join("app.apk"), b"not taken");
        write(&root.join("DCIM/.thumbnails/tiny.jpg"), &[0xff, 0xd8, 0xff, 9]);
        let cancel = AtomicBool::new(false);
        let kinds = [Kind::Image, Kind::Document];
        let found = scan_media(std::slice::from_ref(&root), &kinds, &cancel, &mut |_| {}).unwrap();
        let names: Vec<String> = found
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "got {names:?}");
        assert!(names.contains(&"a.jpg".to_string()));
        assert!(names.contains(&"b.pdf".to_string()));
        assert!(found.iter().all(|f| f.size > 0));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_whole_folder_is_taken_even_without_a_telling_name() {
        let root = somewhere("whole");
        write(&root.join("MicroMsg/abc/EnMicroMsg.db"), b"not a known kind");
        write(&root.join("MicroMsg/abc/image2/x.dat"), b"nor this");
        write(&root.join("MicroMsg/abc/cache/junk"), b"nor this");
        let cancel = AtomicBool::new(false);
        let found = scan_whole(std::slice::from_ref(&root), Kind::Wechat, &cancel, &mut |_| {}).unwrap();
        assert_eq!(found.len(), 2, "the cache is passed over");
        assert!(found.iter().all(|f| f.kind == Kind::Wechat));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_reached_twice_is_kept_once() {
        let root = somewhere("twice");
        write(&root.join("a.jpg"), &[0xff, 0xd8, 0xff, 0, 0, 0, 0, 0, 0, 0, 0]);
        fs::hard_link(root.join("a.jpg"), root.join("b.jpg")).unwrap();
        let cancel = AtomicBool::new(false);
        let found = scan_media(std::slice::from_ref(&root), &[Kind::Image], &cancel, &mut |_| {}).unwrap();
        assert_eq!(found.len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_copy_out_keeps_the_places_the_files_came_from() {
        assert_eq!(
            place(Path::new("/data/media/0/DCIM/Camera/a.jpg"), Kind::Image),
            PathBuf::from("images/user0/DCIM/Camera/a.jpg")
        );
        assert_eq!(
            place(Path::new("/data/media/999/Tencent/x.jpg"), Kind::Image),
            PathBuf::from("images/user999/Tencent/x.jpg")
        );
        assert_eq!(
            place(Path::new("/data/data/com.tencent.mm/MicroMsg/a.db"), Kind::Wechat),
            PathBuf::from("wechat/user0/com.tencent.mm/MicroMsg/a.db")
        );
        assert_eq!(
            place(Path::new("/data/user/999/com.tencent.mm/b.db"), Kind::Wechat),
            PathBuf::from("wechat/user999/com.tencent.mm/b.db")
        );
        assert_eq!(
            place(Path::new("/storage/1A2B/DCIM/c.jpg"), Kind::Image),
            PathBuf::from("images/sdcard-1A2B/DCIM/c.jpg")
        );
    }

    #[test]
    fn a_copy_out_writes_the_files_and_the_list() {
        let root = somewhere("export");
        write(&root.join("DCIM/a.jpg"), &[0xff, 0xd8, 0xff, 1]);
        write(&root.join("DCIM/b.jpg"), &[0xff, 0xd8, 0xff, 2]);
        let found = [
            Found { path: root.join("DCIM/a.jpg"), size: 4, modified: 0, kind: Kind::Image },
            Found { path: Path::new("/no/such/file.jpg").to_path_buf(), size: 9, modified: 0, kind: Kind::Image },
            Found { path: root.join("DCIM/b.jpg"), size: 4, modified: 0, kind: Kind::Image },
        ];
        let into = root.join("out");
        let cancel = AtomicBool::new(false);
        let summary = export(&found, &into, &cancel, &mut |_| {}).unwrap();
        assert_eq!(summary.files, 2);
        assert_eq!(summary.failed, 1, "the missing file is counted, not fatal");
        assert_eq!(summary.bytes, 8);
        assert!(into.join("MANIFEST.csv").is_file());
        let list = fs::read_to_string(into.join("MANIFEST.csv")).unwrap();
        assert!(list.starts_with("source,size,modified,target"));
        assert_eq!(list.lines().count(), 3);
        let _ = fs::remove_dir_all(&root);
    }

    /// A rescue reads and writes; what it reads must come out of it as it
    /// went in. The phone that will not start is still the only copy of
    /// what is on it, so a copy out that changed even the time of a file
    /// would be a copy out that damaged the thing it was asked to save.
    #[test]
    fn a_copy_out_leaves_what_it_read_as_it_was() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let root = somewhere("untouched");
        let into = somewhere("untouched-into");
        let picture = root.join("DCIM/Camera/IMG_0001.jpg");
        write(&picture, &[0xff, 0xd8, 0xff, 0xe0, 1, 2, 3, 4]);
        fs::set_permissions(&picture, fs::Permissions::from_mode(0o640)).unwrap();
        write(&root.join("Documents/保修单.pdf"), b"%PDF-1.7 hello");
        write(&root.join("MicroMsg/abc/EnMicroMsg.db"), b"a database as it lies");

        let before = tree(&root);
        let found: Vec<Found> = [
            ("DCIM/Camera/IMG_0001.jpg", Kind::Image),
            ("Documents/保修单.pdf", Kind::Document),
            ("MicroMsg/abc/EnMicroMsg.db", Kind::Wechat),
        ]
        .iter()
        .map(|(name, kind)| Found {
            path: root.join(name),
            size: fs::metadata(root.join(name)).unwrap().len(),
            modified: 0,
            kind: *kind,
        })
        .collect();
        let cancel = AtomicBool::new(false);
        let summary = export(&found, &into, &cancel, &mut |_| {}).unwrap();

        // Everything was taken, so nothing was left out of the comparison
        // for want of being read.
        assert_eq!(summary.files, 3, "{summary:?}");
        assert_eq!(summary.bytes, 8 + 14 + 21);

        // And what was read is exactly what was there: same names, same
        // sizes, same times, same modes, same bytes, nothing added.
        assert_eq!(tree(&root), before, "a copy out changed what it read");
        assert_eq!(fs::metadata(&picture).unwrap().mode() & 0o777, 0o640, "the picture keeps its mode");

        // What was written is a copy, not the file itself: another inode,
        // the same bytes.
        let copy = into.join(place(&picture, Kind::Image));
        assert_eq!(fs::read(&copy).unwrap(), fs::read(&picture).unwrap());
        assert_ne!(fs::metadata(&copy).unwrap().ino(), fs::metadata(&picture).unwrap().ino());

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&into);
    }

    /// Every file under `root`, with what it holds and what it says about
    /// itself, in a fixed order so two of these can be compared.
    fn tree(root: &Path) -> Vec<(String, u64, i64, u32, u64, Vec<u8>)> {
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
                out.push((name, meta.len(), meta.mtime(), meta.mode(), meta.ino(), fs::read(&path).unwrap()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    #[test]
    fn two_files_of_one_name_do_not_cost_each_other() {
        let root = somewhere("clash");
        let first = root.join("a.jpg");
        fs::create_dir_all(&root).unwrap();
        fs::write(&first, b"one").unwrap();
        let second = free_name(&first);
        assert_eq!(second, root.join("a-1.jpg"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn what_builds_again_on_the_next_start_is_passed_over() {
        assert!(skipped(Path::new("/a/cache")));
        assert!(skipped(Path::new("/a/.thumbnails")));
        assert!(skipped(Path::new("/a/Cache")), "however it is spelled");
        assert!(!skipped(Path::new("/a/DCIM")));
        assert!(skipped(Path::new("/data/media/0/SkywardRescue")), "a rescue does not find itself");
    }

    #[test]
    fn a_stop_leaves_the_walk_where_it_was() {
        let root = somewhere("stop");
        write(&root.join("a.jpg"), &[0xff, 0xd8, 0xff, 0]);
        let cancel = AtomicBool::new(true);
        let found = scan_media(std::slice::from_ref(&root), &[Kind::Image], &cancel, &mut |_| {}).unwrap();
        assert!(found.is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
