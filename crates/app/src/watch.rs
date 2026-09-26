//! Being *told* that an open file changed, rather than asking four times a
//! second.
//!
//! The auto-revert sweep in `main.rs` is the one place that decides what a
//! change to a file on disk means — the modified-buffer guard, the content
//! comparison, the revert itself. None of that moves here. This only answers
//! "is it worth sweeping *now*", which is the half polling was bad at: a coding
//! agent writes a file and the buffer should follow within a frame, not within
//! whatever is left of a 250 ms tick.
//!
//! kqueue and not FSEvents (which is what `notify` is on macOS, and what the
//! header on the sweep argues against): `EVFILT_VNODE` is per *file*, fires on
//! the write itself, and has no latency window of its own. One fd per open file,
//! opened `O_EVTONLY` so it holds nothing that would stop a volume unmounting.
//!
//! A file *replaced* rather than rewritten — an atomic save, `git checkout` —
//! leaves the fd watching the old inode, which is gone. That arrives as
//! `NOTE_DELETE`/`NOTE_RENAME`; the fd is marked stale and the next [`Watch::sync`]
//! re-arms on whatever the name means now. A name that briefly means nothing
//! (between an unlink and a rename) is simply retried on the next sync, and the
//! slow fallback sweep is what guarantees there is one.
//!
//! ponytail: macOS only. Elsewhere [`Watch::new`] is inert and the sweep polls
//! exactly as it did. The upgrade is an inotify arm in `arm` and the thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub struct Watch {
    /// The kqueue, or -1 when there is none and the caller should poll.
    kq: i32,
    fds: HashMap<PathBuf, i32>,
    /// Something changed since [`Watch::take_fired`] last looked.
    fired: Arc<AtomicBool>,
    /// Fds whose file was deleted or renamed away, to re-arm by name.
    stale: Arc<Mutex<Vec<i32>>>,
}

impl Default for Watch {
    /// Inert: no thread, no kqueue. What the sweep's tests get, so a unit test
    /// of the revert rules never depends on the filesystem telling it anything.
    fn default() -> Self {
        Self {
            kq: -1,
            fds: HashMap::new(),
            fired: Arc::default(),
            stale: Arc::default(),
        }
    }
}

impl Watch {
    #[cfg(target_os = "macos")]
    pub fn new() -> Self {
        let mut w = Self::default();
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return w; // no watcher, and the sweep polls as it always did
        }
        let (fired, stale) = (w.fired.clone(), w.stale.clone());
        let spawned = std::thread::Builder::new()
            .name("zemacs-watch".into())
            .spawn(move || listen(kq, &fired, &stale));
        if spawned.is_ok() {
            w.kq = kq;
        } else {
            unsafe { libc::close(kq) };
        }
        w
    }

    #[cfg(not(target_os = "macos"))]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether anything is being watched, i.e. whether the sweep may slow down.
    pub fn active(&self) -> bool {
        self.kq >= 0
    }

    /// True once per burst of changes.
    pub fn take_fired(&self) -> bool {
        self.fired.swap(false, Ordering::AcqRel)
    }

    /// Watch exactly `paths`: arm the new ones, re-arm the replaced ones, and
    /// drop the ones nothing has open any more. Closing an fd is what removes
    /// its event from the queue, so there is no second bookkeeping step.
    pub fn sync(&mut self, paths: &[&Path]) {
        if self.kq < 0 {
            return;
        }
        let stale = std::mem::take(&mut *self.stale.lock().unwrap_or_else(|e| e.into_inner()));
        self.fds.retain(|p, fd| {
            let keep = paths.contains(&p.as_path()) && !stale.contains(fd);
            if !keep {
                unsafe { libc::close(*fd) };
            }
            keep
        });
        for p in paths {
            if !self.fds.contains_key(*p) {
                if let Some(fd) = arm(self.kq, p) {
                    self.fds.insert(p.to_path_buf(), fd);
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn arm(kq: i32, path: &Path) -> Option<i32> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let change = libc::kevent {
        ident: fd as usize,
        filter: libc::EVFILT_VNODE,
        // `EV_CLEAR`: one event per change rather than one per `kevent` call
        // for as long as the condition holds.
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags: libc::NOTE_WRITE
            | libc::NOTE_EXTEND
            | libc::NOTE_ATTRIB
            | libc::NOTE_DELETE
            | libc::NOTE_RENAME
            | libc::NOTE_REVOKE,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let ok = unsafe { libc::kevent(kq, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    if ok < 0 {
        unsafe { libc::close(fd) };
        return None;
    }
    Some(fd)
}

#[cfg(not(target_os = "macos"))]
fn arm(_kq: i32, _path: &Path) -> Option<i32> {
    None
}

/// The watcher thread: block until a file changes, let the burst settle, then
/// raise the flag and wake the loop.
///
/// The settle is 15 ms of quiet, and it is there for two writers. One that
/// truncates and then writes fires twice, and reading between the two would
/// revert the buffer to an empty file for a frame. And an agent writes several
/// files at once, which should be one sweep and one redraw, not six.
#[cfg(target_os = "macos")]
fn listen(kq: i32, fired: &AtomicBool, stale: &Mutex<Vec<i32>>) {
    const GONE: u32 = libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE;
    let settle = libc::timespec { tv_sec: 0, tv_nsec: 15_000_000 };
    let mut events: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
    loop {
        let mut timeout = std::ptr::null();
        let mut seen = false;
        loop {
            let n = unsafe {
                libc::kevent(kq, std::ptr::null(), 0, events.as_mut_ptr(), 16, timeout)
            };
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return; // the queue itself is broken; the fallback sweep carries on
            }
            if n == 0 {
                break; // quiet for the settle window
            }
            seen = true;
            let mut s = stale.lock().unwrap_or_else(|e| e.into_inner());
            s.extend(
                events[..n as usize]
                    .iter()
                    .filter(|e| e.fflags & GONE != 0)
                    .map(|e| e.ident as i32),
            );
            timeout = &settle;
        }
        if seen {
            fired.store(true, Ordering::Release);
            zemacs_core::wake();
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(w: &Watch) -> bool {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(2) {
            if w.take_fired() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// A write fires; so does a write to the file that *replaced* it, which is
    /// the atomic save an editor or agent does and the case an fd on the old
    /// inode would miss for good.
    #[test]
    fn a_write_fires_and_a_replaced_file_is_rearmed() {
        let dir = std::env::temp_dir().join(format!("zemacs-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one").unwrap();

        let mut w = Watch::new();
        assert!(w.active());
        w.sync(&[file.as_path()]);
        assert!(!w.take_fired());

        std::fs::write(&file, "two").unwrap();
        assert!(wait_for(&w), "an in-place write is noticed");

        let tmp = dir.join("a.txt.tmp");
        std::fs::write(&tmp, "three").unwrap();
        std::fs::rename(&tmp, &file).unwrap();
        assert!(wait_for(&w), "the replace is noticed");
        w.sync(&[file.as_path()]);
        std::fs::write(&file, "four").unwrap();
        assert!(wait_for(&w), "and the new file is watched after it");

        w.sync(&[]);
        assert!(w.fds.is_empty(), "closed files are dropped");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
