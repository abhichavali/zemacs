//! zemacs-dired — the filesystem half of the Dired clone.
//!
//! Two halves, deliberately kept apart, the same way `zemacs-git` splits talking
//! to git from drawing the status buffer:
//!
//! * **Touching the filesystem** (this module): [`list`] turns a directory into
//!   a [`Listing`], and [`rename`], [`copy`], [`delete`], [`create_dir`] and
//!   [`create_file`] are the operations a dired buffer's keys are bound to.
//!   Everything goes through [`std::fs`] — no `ls`, no shelling out. Unlike git,
//!   there is no porcelain to parse here, and the platform call *is* the API.
//! * **Presentation** ([`render`]): a pure function from a [`Listing`] plus the
//!   user's marks to the buffer's *text* plus one [`Line`] per line of that
//!   text. The UI draws the string and, when the cursor sits on line 12 and the
//!   user presses `d`, looks up `lines[12]` to learn which file to flag. The two
//!   returns must therefore always have the same length — see [`render()`].
//!
//! Why an [`OsString`] name and a [`PathBuf`] path on every [`Entry`]: on unix a
//! file name is bytes, not text, and `b"caf\xe9.rs"` (latin-1) is a perfectly
//! legal name that no `String` can hold. Every operation therefore works on the
//! real `Path`, and `to_string_lossy` appears only in [`render`], where the
//! result is pixels rather than a syscall argument.
//!
//! The dangerous function is [`delete`], which is recursive. It never follows a
//! symlink: a link to a directory is unlinked, and the directory it pointed at
//! is untouched. See that function for how, and `tests/dir.rs` for the proof.
//!
//! ponytail: no watcher and no caching. A [`Listing`] is a snapshot; the UI
//! re-runs [`list`] after every operation, which is one `readdir` plus one
//! `lstat` per entry, and is not worth optimising until a directory with tens of
//! thousands of entries is a real complaint.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};

pub mod render;
pub use render::{human_size, render, Line, MARK_DELETE, MARK_SELECT};

/// One directory entry, as one line of the dired buffer.
///
/// The metadata is the *link's own* (`lstat`), which is what `ls -l` shows for a
/// symlink and what keeps a listing cheap and side-effect free. The one field
/// that follows the link is [`Entry::is_dir`], because the UI needs to know
/// whether pressing `RET` here opens a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// File name only — bytes on unix, so not necessarily UTF-8.
    pub name: OsString,
    /// Full path, and the only thing any operation should be handed.
    pub path: PathBuf,
    /// Whether opening this entry lands in a directory: true for a directory
    /// and for a symlink resolving to one, false for a broken symlink.
    pub is_dir: bool,
    pub is_symlink: bool,
    /// What the symlink points at, verbatim — relative links stay relative, and
    /// it is `Some` even when the target does not exist.
    pub link_target: Option<PathBuf>,
    pub len: u64,
    /// `None` when the platform or filesystem has no mtime, or the entry
    /// vanished between `readdir` and `lstat`.
    pub modified: Option<SystemTime>,
    pub readonly: bool,
    /// Permission and setuid/sticky bits (`mode & 0o7777`) on unix; a plausible
    /// stand-in derived from `readonly` elsewhere. Only the low 9 are rendered.
    pub mode: u32,
}

impl Entry {
    /// A dotfile, which `..` and `.` are not — they are navigation.
    pub fn is_hidden(&self) -> bool {
        !self.is_dot() && self.name.to_string_lossy().starts_with('.')
    }

    /// `.` or `..`.
    pub fn is_dot(&self) -> bool {
        let name = self.name.as_os_str();
        name == OsStr::new(".") || name == OsStr::new("..")
    }
}

/// A directory, listed. Snapshot: nothing here re-reads the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub dir: PathBuf,
    /// `..` first (always), then `.` when hidden files are shown, then
    /// directories, then files — see [`list`].
    pub entries: Vec<Entry>,
    pub show_hidden: bool,
    pub sort: Sort,
}

impl Listing {
    /// Where a name ended up after a re-list, so the UI can keep the cursor on
    /// the file the user was looking at across a rename or a sort change.
    pub fn index_of(&self, name: &OsStr) -> Option<usize> {
        self.entries.iter().position(|e| e.name == name)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Sort {
    /// Case-insensitive, which is what a human reading a column expects.
    #[default]
    Name,
    /// Largest first — the reason to sort by size is to find the big thing.
    Size,
    /// Newest first, for the same reason.
    Modified,
    /// Groups by extension, then by name within each group.
    Extension,
}

impl Sort {
    /// For the buffer header.
    pub fn label(self) -> &'static str {
        match self {
            Sort::Name => "name",
            Sort::Size => "size",
            Sort::Modified => "date",
            Sort::Extension => "extension",
        }
    }
}

// ---------------------------------------------------------------- listing

/// List `dir`.
///
/// `..` is always included, hidden or not, so that going up is always visible
/// and reachable; `.` is included only with `show_hidden`, since it says nothing
/// the header does not. Both sort to the top. Directories sort before files
/// within whichever [`Sort`] is chosen, the way `ls --group-directories-first`
/// does, because in a file manager they are navigation rather than content.
///
/// Errors if the directory cannot be read at all (gone, not a directory, no
/// permission). An individual entry that cannot be `lstat`ed — a broken symlink,
/// or a file deleted while we were walking — degrades to zeroed metadata instead
/// of failing the whole listing.
pub fn list(dir: &Path, show_hidden: bool, sort: Sort) -> Result<Listing> {
    let read = fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))?;

    let mut entries = vec![entry_of(dir.join(".."), OsString::from(".."))];
    if show_hidden {
        entries.push(entry_of(dir.join("."), OsString::from(".")));
    }
    for item in read {
        // A directory that vanishes mid-walk surfaces here as an error rather
        // than as a short listing the user would read as "the files are gone".
        let item = item.with_context(|| format!("listing {}", dir.display()))?;
        let name = item.file_name();
        if !show_hidden && name.to_string_lossy().starts_with('.') {
            continue;
        }
        entries.push(entry_of(item.path(), name));
    }
    sort_entries(&mut entries, sort);

    Ok(Listing {
        dir: dir.to_path_buf(),
        entries,
        show_hidden,
        sort,
    })
}

fn entry_of(path: PathBuf, name: OsString) -> Entry {
    // `symlink_metadata`, not `metadata`: a symlink must be reported as itself.
    // Following it here would make a link to a 4GB file look like a 4GB file,
    // and a link into an unreachable network mount hang the listing.
    let meta = fs::symlink_metadata(&path).ok();
    let is_symlink = meta.as_ref().is_some_and(|m| m.file_type().is_symlink());
    Entry {
        // The one place the link *is* followed, and only to answer "does RET
        // open a directory here?". `is_dir()` swallows the error, so a broken
        // link answers no, which is the truth.
        is_dir: if is_symlink {
            path.is_dir()
        } else {
            meta.as_ref().is_some_and(|m| m.is_dir())
        },
        is_symlink,
        link_target: is_symlink.then(|| fs::read_link(&path).ok()).flatten(),
        len: meta.as_ref().map_or(0, |m| m.len()),
        modified: meta.as_ref().and_then(|m| m.modified().ok()),
        readonly: meta.as_ref().is_some_and(|m| m.permissions().readonly()),
        mode: meta.as_ref().map_or(0, mode_of),
        name,
        path,
    }
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    // The high bits are the file type, which [`Entry`] already carries.
    meta.mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(meta: &fs::Metadata) -> u32 {
    // No mode bits to read; the rwx column is a polite fiction there anyway.
    if meta.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

fn sort_entries(entries: &mut [Entry], sort: Sort) {
    entries.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| match sort {
                Sort::Name => Ordering::Equal,
                Sort::Size => b.len.cmp(&a.len),
                // `None` sorts as older than any `Some`, reversed to the bottom.
                Sort::Modified => b.modified.cmp(&a.modified),
                Sort::Extension => extension(a).cmp(&extension(b)),
            })
            // Always the last word, so the order is total and a listing does not
            // shuffle between two refreshes of an unchanged directory.
            .then_with(|| folded(&a.name).cmp(&folded(&b.name)))
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn rank(entry: &Entry) -> u8 {
    match () {
        _ if entry.name == OsStr::new(".") => 0,
        _ if entry.name == OsStr::new("..") => 1,
        _ if entry.is_dir => 2,
        _ => 3,
    }
}

/// ponytail: allocates per comparison, so sorting is O(n log n) small `String`s.
/// Fine for a directory a human is looking at; a listing big enough to notice
/// wants a decorate-sort-undecorate pass instead.
fn folded(name: &OsStr) -> String {
    name.to_string_lossy().to_lowercase()
}

fn extension(entry: &Entry) -> String {
    Path::new(&entry.name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

// ------------------------------------------------------------- operations

/// Move `from` to `to`.
///
/// Refuses an existing destination rather than clobbering it: the caller has a
/// user in front of it and can prompt, and `fs::rename` would otherwise delete
/// the destination silently.
///
/// ponytail: no cross-device fallback. A rename across a mount point fails with
/// `EXDEV` and is reported as-is; doing it properly is copy-then-delete with a
/// half-copied tree to clean up on failure, which needs a UI to report to.
pub fn rename(from: &Path, to: &Path) -> Result<()> {
    exists(from)?;
    vacant(to)?;
    fs::rename(from, to).with_context(|| format!("renaming {} to {}", from.display(), to.display()))
}

/// Copy `from` to `to`, recursively for a directory.
///
/// Symlinks are copied as symlinks, never followed. That keeps a copy cheap and
/// faithful, and it is also what makes recursion safe: a link pointing back at
/// an ancestor cannot become an infinite walk. A directory copied *into itself*
/// still would, so that is detected and refused before anything is written.
pub fn copy(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from)
        .with_context(|| format!("copying {}: cannot read it", from.display()))?;
    vacant(to)?;

    if meta.file_type().is_symlink() {
        copy_link(from, to)
    } else if meta.is_dir() {
        // `a` into `a/b` would copy `b` into `b/b` into `b/b/b` until the disk
        // filled. This also catches copying a directory onto itself.
        if within(from, to) {
            bail!(
                "refusing to copy {} into itself ({})",
                from.display(),
                to.display()
            );
        }
        copy_tree(from, to)
    } else {
        fs::copy(from, to)
            .map(|_| ())
            .with_context(|| format!("copying {} to {}", from.display(), to.display()))
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir(to).with_context(|| format!("creating {}", to.display()))?;
    for item in fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let item = item.with_context(|| format!("reading {}", from.display()))?;
        // `DirEntry::file_type` is the `lstat` type: a symlink reports as a
        // symlink, so the branch below never walks through one.
        let kind = item
            .file_type()
            .with_context(|| format!("reading {}", item.path().display()))?;
        let (src, dst) = (item.path(), to.join(item.file_name()));
        if kind.is_symlink() {
            copy_link(&src, &dst)?;
        } else if kind.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            fs::copy(&src, &dst)
                .map(|_| ())
                .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
        }
    }
    // Last, not first: a mode-0500 directory has to be writable while it is
    // being filled. `fs::copy` already carries each file's own bits across.
    if let Ok(meta) = fs::metadata(from) {
        let _ = fs::set_permissions(to, meta.permissions());
    }
    Ok(())
}

#[cfg(unix)]
fn copy_link(from: &Path, to: &Path) -> Result<()> {
    let target = fs::read_link(from).with_context(|| format!("reading {}", from.display()))?;
    std::os::unix::fs::symlink(&target, to)
        .with_context(|| format!("linking {} to {}", to.display(), target.display()))
}

#[cfg(not(unix))]
fn copy_link(from: &Path, to: &Path) -> Result<()> {
    // Creating a symlink on Windows needs a privilege the editor may not have,
    // so the link is followed instead — a copy that is wrong in a harmless way
    // beats an operation that always fails.
    if from.is_dir() {
        copy_tree(from, to)
    } else {
        fs::copy(from, to)
            .map(|_| ())
            .with_context(|| format!("copying {} to {}", from.display(), to.display()))
    }
}

/// Delete `path`, recursively for a directory. **The dangerous one.**
///
/// Three rules, in order:
///
/// 1. A symlink is *unlinked*, never followed. Deleting `link -> ~/photos`
///    removes the link and leaves the photos alone. This holds at every level of
///    the recursion, not just the top, because the walk asks
///    [`std::fs::DirEntry::file_type`] — the `lstat` type — rather than testing
///    the target.
/// 2. The walk only ever descends into real directories it found by reading the
///    directory above, so it cannot leave the tree it was pointed at.
/// 3. A path with no final component (`/`, `.`, `..`, or anything ending in
///    `..`) is refused outright. Those are the shapes that delete something
///    other than what the user is looking at.
pub fn delete(path: &Path) -> Result<()> {
    if path.file_name().is_none() {
        bail!("refusing to delete {}: not a named file", path.display());
    }
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("deleting {}: cannot read it", path.display()))?;

    if meta.file_type().is_symlink() || !meta.is_dir() {
        remove_leaf(path)
    } else {
        remove_tree(path)
    }
    .with_context(|| format!("deleting {}", path.display()))
}

/// Anything that is not a directory we must descend into — including a symlink
/// to a directory, which unlinks the link itself.
fn remove_leaf(path: &Path) -> Result<()> {
    // Windows needs `remove_dir` for a directory symlink and `remove_file` for
    // everything else; neither follows the link. The first error is the one
    // worth reporting, since on unix the second call is never the right one.
    fs::remove_file(path)
        .or_else(|first| fs::remove_dir(path).map_err(|_| first))
        .map_err(Into::into)
}

fn remove_tree(dir: &Path) -> Result<()> {
    for item in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let item = item.with_context(|| format!("reading {}", dir.display()))?;
        let kind = item
            .file_type()
            .with_context(|| format!("reading {}", item.path().display()))?;
        // `is_dir` is false for a symlink to a directory, which is the whole
        // safety property: that case falls to `remove_leaf` and is unlinked.
        if kind.is_dir() {
            remove_tree(&item.path())?;
        } else {
            remove_leaf(&item.path())
                .with_context(|| format!("deleting {}", item.path().display()))?;
        }
    }
    fs::remove_dir(dir).with_context(|| format!("deleting {}", dir.display()))
}

/// Create a directory named `name` inside `parent`, returning its path.
pub fn create_dir(parent: &Path, name: &str) -> Result<PathBuf> {
    let path = child(parent, name)?;
    fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?;
    Ok(path)
}

/// Create an empty file named `name` inside `parent`, returning its path.
///
/// Never truncates: an existing file is an error, not a silently emptied file.
pub fn create_file(parent: &Path, name: &str) -> Result<PathBuf> {
    let path = child(parent, name)?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    Ok(path)
}

// ------------------------------------------------------------------ guards

/// A *name* is a single component. Rejecting separators is what stops a prompt
/// answered with `../../.ssh/authorized_keys` from creating anything outside the
/// directory the user is looking at.
fn child(parent: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        bail!("{name:?} is not a file name");
    }
    let path = parent.join(name);
    vacant(&path)?;
    Ok(path)
}

/// `symlink_metadata`, so that a *dangling* symlink still counts as occupied —
/// `Path::exists` follows links and would call it free, and the rename would
/// then quietly destroy the link.
fn vacant(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        bail!("{} already exists", path.display());
    }
    Ok(())
}

fn exists(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_err() {
        bail!("{} does not exist", path.display());
    }
    Ok(())
}

/// Is `path` inside `dir`, or `dir` itself?
///
/// Both sides are resolved through symlinks first, so a link pointing back into
/// the source is caught. `starts_with` compares whole components, so `/a/bc` is
/// correctly *not* inside `/a/b`.
fn within(dir: &Path, path: &Path) -> bool {
    resolved(path).starts_with(resolved(dir))
}

/// `canonicalize` for a path that may not exist yet: resolve the deepest
/// ancestor that does and re-attach the rest.
fn resolved(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolved(parent).join(name),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guards are pure enough to pin down without a filesystem; the
    /// integration tests drive the real thing.
    #[test]
    fn a_name_is_one_component() {
        for bad in ["", ".", "..", "../escape", "a/b", "a\\b", "a\0b"] {
            assert!(
                child(Path::new("/tmp"), bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        assert_eq!(
            child(Path::new("/tmp"), "notes.txt").unwrap(),
            PathBuf::from("/tmp/notes.txt")
        );
        // Leading dashes and spaces are file names, not flags: nothing here
        // ever reaches a shell or an argv.
        assert!(child(Path::new("/tmp"), "-rf").is_ok());
        assert!(child(Path::new("/tmp"), "my file").is_ok());
    }

    #[test]
    fn within_compares_whole_components() {
        assert!(within(Path::new("/a/b"), Path::new("/a/b/c")));
        assert!(within(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!within(Path::new("/a/b"), Path::new("/a/bc")));
        assert!(!within(Path::new("/a/b"), Path::new("/a")));
    }

    /// `..` at the end is the shape that deletes the wrong directory.
    #[test]
    fn delete_refuses_paths_with_no_final_component() {
        for bad in ["/", ".", "..", "/tmp/..", "/tmp/x/.."] {
            let err = delete(Path::new(bad)).unwrap_err().to_string();
            assert!(err.contains("not a named file"), "{bad}: {err}");
        }
    }
}
