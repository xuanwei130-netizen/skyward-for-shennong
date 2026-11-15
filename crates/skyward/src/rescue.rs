//! Saving what is worth keeping from a phone that no longer starts, on a
//! thread of its own: the pictures and documents anywhere on it, and a
//! chat app's folders taken whole.

use anyhow::{Context, Result};
use skyward_rescue::{Found, Kind, Progress, Summary};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The most often how far the work has come is told.
const PROGRESS_EVERY: Duration = Duration::from_millis(120);

/// The chat app whose folders are taken whole: everything under it is
/// kept, whatever it is called, because what an app needs to be read
/// again is not for this side to know.
pub const CHAT: &str = "com.tencent.mm";

/// The folder the rescued files are written into, under the owner's own
/// shared storage.
pub const FOLDER: &str = "SkywardRescue";

pub enum Task {
    /// Look across the phone for these kinds of file.
    Scan(Vec<Kind>),
    /// Copy these out into that folder.
    Export { found: Vec<Found>, into: PathBuf },
}

#[derive(Clone)]
pub enum End {
    Done,
    /// Stopped as asked.
    Stopped,
    Failed(String),
}

#[derive(Clone)]
pub enum Event {
    Progress(Progress),
    /// What a scan kept, sent once, when it has ended.
    Found(Vec<Found>),
    /// A chat app's data being opened in place, and how much of it is
    /// done: the longest wait of a rescue, and the one worth telling.
    Unlocking(skyward_wechat::Stage, u64),
    /// What a copy out ended with, and what of the chat app was opened.
    Saved(Summary, Option<skyward_wechat::Given>),
    Ended(End),
}

pub struct Worker {
    events: Receiver<Event>,
    /// Readable while events wait.
    wake: OwnedFd,
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn start(task: Task) -> Result<Worker> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(std::io::Error::last_os_error()).context("make the rescue worker's wake pipe");
        }
        let (wake, signal) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let (tx, events) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let thread = thread::Builder::new()
            .name("rescue".into())
            .spawn(move || {
                let tell = |event: Event| {
                    // The screen only stops listening once it has let the
                    // worker go.
                    let _ = tx.send(event);
                    unsafe { libc::write(signal.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
                };
                let mut told = Instant::now() - PROGRESS_EVERY;
                let mut report = |event: Event| match event {
                    // How far the work has come is the one thing worth
                    // holding back, so that a walk over a hundred thousand
                    // files does not wake the screen a hundred thousand
                    // times.
                    Event::Progress(_) => {
                        if told.elapsed() >= PROGRESS_EVERY {
                            told = Instant::now();
                            tell(event);
                        }
                    }
                    other => tell(other),
                };
                let ended = match run(task, &stop, &mut report) {
                    Ok(()) => End::Done,
                    // A stopped walk ends where it was, whether or not the
                    // thing that ended it was a stop.
                    Err(_) if stop.load(Ordering::Relaxed) => End::Stopped,
                    Err(e) => End::Failed(format!("{e:#}")),
                };
                tell(Event::Ended(ended));
            })
            .context("start the rescue worker")?;
        Ok(Worker { events, wake, cancel, thread: Some(thread) })
    }

    pub fn fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// The events since the last call.
    pub fn events(&self) -> Vec<Event> {
        let mut drain = [0u8; 64];
        while unsafe { libc::read(self.wake.as_raw_fd(), drain.as_mut_ptr().cast(), drain.len()) } > 0 {}
        self.events.try_iter().collect()
    }

    /// Ask the work to stop; it does between two files.
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Worker {
    /// The screen lets the worker go once its work has ended.
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the rescue worker does not panic");
        }
    }
}

fn run(task: Task, stop: &AtomicBool, tell: &mut dyn FnMut(Event)) -> Result<()> {
    match task {
        Task::Scan(kinds) => {
            let mut found: Vec<Found> = Vec::new();
            // The chat app is walked first, so that a picture inside it is
            // kept under the chat app's folder rather than twice over: the
            // walk that keeps everything meets it first.
            if kinds.contains(&Kind::Wechat) {
                let roots = skyward_rescue::chat_roots(CHAT);
                found.extend(skyward_rescue::scan_whole(&roots, Kind::Wechat, stop, &mut |p| {
                    tell(Event::Progress(p))
                })?);
            }
            let media: Vec<Kind> = kinds.iter().copied().filter(|kind| *kind != Kind::Wechat).collect();
            if !media.is_empty() {
                let roots = skyward_rescue::media_roots();
                found.extend(skyward_rescue::scan_media(&roots, &media, stop, &mut |p| {
                    tell(Event::Progress(p))
                })?);
            }
            if !stop.load(Ordering::Relaxed) {
                tell(Event::Found(found));
            }
            Ok(())
        }
        Task::Export { found, into } => {
            let summary = skyward_rescue::export(&found, &into, stop, &mut |p| tell(Event::Progress(p)))?;
            // A chat app's folder is copied out as it lies and then opened
            // where it lies: the copy is what is kept when opening comes to
            // nothing, which is what happens when the passphrase cannot be
            // worked out.
            let opened = if !stop.load(Ordering::Relaxed) && found.iter().any(|file| file.kind == Kind::Wechat) {
                unlock_chat(&into, stop, tell)
            } else {
                None
            };
            if !stop.load(Ordering::Relaxed) {
                tell(Event::Saved(summary, opened));
            }
            Ok(())
        }
    }
}

/// Open the chat app's own folder, under the copy that was just taken.
///
/// Nothing here fails the rescue: a folder that cannot be opened is
/// already copied out whole, and the pictures in it may still come back
/// even when the databases do not.
fn unlock_chat(into: &std::path::Path, stop: &AtomicBool, tell: &mut dyn FnMut(Event)) -> Option<skyward_wechat::Given> {
    let root = skyward_wechat::roots().into_iter().next()?;
    let there = into.join("wechat");
    skyward_wechat::unlock(&root, &there, &|| stop.load(Ordering::Relaxed), &mut |stage, count| {
        tell(Event::Unlocking(stage, count))
    })
    .ok()
}

/// Where a copy out goes: the owner's own shared storage, under a folder
/// named for the moment, so that two rescues in one session do not mix.
///
/// Shared storage is the only place with room for several gigabytes, which
/// is why the data partition has to be open before anything is saved.
pub fn target() -> Result<PathBuf> {
    let base = ["/data/media/0", "/sdcard"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_dir())
        .context("there is no shared storage to write into; open the data partition first")?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let into = base.join(FOLDER).join(stamp.to_string());
    std::fs::create_dir_all(&into).with_context(|| format!("make {}", into.display()))?;
    Ok(into)
}
