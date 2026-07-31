//! The candidate list behind `project-find-file`, and the cache that makes it
//! usable.
//!
//! Two ways to get the list, in order of preference:
//!
//! 1. `git ls-files`, when the root is inside a repository. It is a read of the
//!    index rather than a walk of the disk, it already knows what `.gitignore`
//!    says, and it is faster than anything reimplemented here would be. Both
//!    `--cached` and `--others --exclude-standard`, so a file created thirty
//!    seconds ago — the single most likely thing to be looked for — is in the
//!    list even though it has never been added.
//! 2. Otherwise a plain walk, skipping the directories that are always output
//!    rather than source. It does not read `.gitignore`; see [`SKIP`].
//!
//! Either way the list is capped and says so ([`Files::truncated`]), because a
//! silently short list makes "that file is not in this project" and "this
//! project is too big" look identical from the prompt.

use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

/// How many files a project may contribute before the list is cut short.
///
/// High enough that no repository worth opening in an editor hits it, low
/// enough that the completion UI is scanning a couple of megabytes per
/// keystroke rather than a hundred.
pub const FILE_LIMIT: usize = 50_000;

/// Never descended into by the fallback walk: build output, dependency dumps
/// and virtualenvs, none of which anyone opens on purpose, all of which dwarf
/// the source tree.
///
/// ponytail: a fixed list, not `.gitignore`. Parsing gitignore properly is
/// negative-patterns, `**`, per-directory files and precedence rules — a crate
/// on its own — and the case it would fix is a non-git project with a large
/// ignored directory not named below. The upgrade path is to add a name here.
const SKIP: [&str; 13] = [
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    "dist",
    ".next",
];

/// How long a cached listing is trusted. See [`Cache`].
const TTL: Duration = Duration::from_secs(5);

/// A project's files, as of one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Files {
    pub root: PathBuf,
    /// Relative to [`Files::root`], sorted, deduplicated. `root.join(path)`
    /// opens one; the relative form is what a prompt should show.
    pub files: Vec<PathBuf>,
    /// The cap was reached, so this list is a prefix of the truth. Worth
    /// saying out loud in the prompt — the alternative is a user who believes
    /// a file does not exist.
    pub truncated: bool,
}

/// List `root`'s files. Errors only when `root` is not a directory.
pub fn files(root: &Path) -> Result<Files> {
    list(root, FILE_LIMIT)
}

/// Every directory that contains a listed file, plus the root itself as `"."`,
/// relative and sorted — the candidate list for a project-scoped dired.
///
/// Derived from the file list rather than walked separately, which makes it
/// free and, more usefully, makes it honour exactly the same ignore rules. The
/// cost is that a genuinely empty directory is not in it; git does not track
/// those either, so the two disagree with each other in the same direction.
pub fn directories(files: &Files) -> Vec<PathBuf> {
    let mut dirs = BTreeSet::new();
    dirs.insert(PathBuf::from("."));
    for file in &files.files {
        let mut parent = file.parent();
        while let Some(dir) = parent {
            // `parent` of a bare file name is `""`, which is the root, already
            // present as `.`.
            if dir.as_os_str().is_empty() {
                break;
            }
            // Seen before means every directory above it has been too — this is
            // what keeps the whole pass linear rather than depth-per-file.
            if !dirs.insert(dir.to_path_buf()) {
                break;
            }
            parent = dir.parent();
        }
    }
    dirs.into_iter().collect()
}

/// Listings, per root, so that a completion session walks the project once
/// instead of once per keystroke.
///
/// Owned by the caller: this crate has no globals, and a cache that outlives
/// the frame holding it is a bug that only shows up in the second window.
#[derive(Debug, Default)]
pub struct Cache {
    entries: HashMap<PathBuf, (Files, Instant)>,
}

impl Cache {
    pub fn new() -> Cache {
        Cache::default()
    }

    /// The project's files, walked at most once every five seconds.
    ///
    /// Age is the staleness test, rather than watching the filesystem or
    /// stat-ing the tree. It is correct enough because of what the two failure
    /// directions cost: a listing at most five seconds old can miss a file the
    /// user *just* created, and they will find it on the next prompt or after
    /// [`Cache::forget`]; whereas a listing rebuilt per keystroke makes the
    /// prompt unusable on exactly the repositories that need it most. Five
    /// seconds is shorter than the gap between two deliberate actions and
    /// longer than a completion session, which is the property that matters.
    ///
    /// A borrow rather than a clone: 50,000 paths per keystroke is the cost
    /// this type exists to avoid.
    pub fn files(&mut self, root: &Path) -> Result<&Files> {
        let fresh = self
            .entries
            .get(root)
            .is_some_and(|(_, at)| at.elapsed() < TTL);
        if !fresh {
            // A failed re-listing leaves any previous entry alone, but is still
            // reported: a root that has been deleted should say so once rather
            // than silently serve a list of files that are gone.
            let listed = files(root)?;
            self.entries
                .insert(root.to_path_buf(), (listed, Instant::now()));
        }
        Ok(&self.entries[root].0)
    }

    /// [`directories`], through the cache.
    pub fn directories(&mut self, root: &Path) -> Result<Vec<PathBuf>> {
        Ok(directories(self.files(root)?))
    }

    /// Drop one root's listing, so the next ask re-walks. This is what a manual
    /// refresh — `C-u project-find-file` in Emacs — is bound to.
    pub fn forget(&mut self, root: &Path) {
        self.entries.remove(root);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

// ------------------------------------------------------------------ listing

fn list(root: &Path, limit: usize) -> Result<Files> {
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }
    let (mut files, truncated) = match git_files(root, limit) {
        Some(listed) => listed,
        None => walk_files(root, limit),
    };
    // Sorted after the cap, so a truncated list is an arbitrary subset rather
    // than an alphabetical prefix — which is honest, since `truncated` says the
    // list is incomplete and a prefix would suggest the tail simply does not
    // exist. `dedup` because an unmerged path appears once per conflict stage.
    files.sort();
    files.dedup();
    Ok(Files {
        root: root.to_path_buf(),
        files,
        truncated,
    })
}

/// `None` when `root` is not in a repository, or `git` is not installed — both
/// mean "walk it yourself", and neither is an error worth surfacing.
///
/// Run with `root` as the working directory, which limits the output to `root`'s
/// subtree and prints every path relative to it. That matters when the root came
/// from a `.project` file inside a larger repository: the listing is of the
/// project, not of the repository around it.
///
/// ponytail: `--cached` lists index entries, so a file deleted but not yet
/// staged is offered and opens empty. Filtering it out is one `stat` per file,
/// which is the cost this branch exists to avoid; the upgrade path is
/// `--deduplicate` plus a `git ls-files --deleted` subtraction, both cheap, once
/// anyone actually trips over it. Submodule contents are missing for the same
/// reason: `--recurse-submodules` cannot be combined with `--others`, and
/// dropping `--others` would lose brand-new files, which are worth more.
fn git_files(root: &Path, limit: usize) -> Option<(Vec<PathBuf>, bool)> {
    let out = Command::new("git")
        .current_dir(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        // Never fight the user's own `git` over index.lock for a file list.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }

    // `-z` turns off git's path quoting, so the bytes between two NULs simply
    // *are* the path — no C-escape unescaping, and non-UTF-8 names survive.
    let mut files = Vec::new();
    let mut truncated = false;
    for raw in out.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        if files.len() >= limit {
            truncated = true;
            break;
        }
        files.push(PathBuf::from(os_string(raw)));
    }
    Some((files, truncated))
}

fn walk_files(root: &Path, limit: usize) -> (Vec<PathBuf>, bool) {
    let mut files = Vec::new();
    let room = walk(root, Path::new(""), &mut files, limit);
    (files, !room)
}

/// Returns whether there is still room under `limit`; `false` unwinds the whole
/// recursion.
fn walk(dir: &Path, prefix: &Path, out: &mut Vec<PathBuf>, limit: usize) -> bool {
    // An unreadable directory is skipped rather than failing the listing: one
    // permission-denied node is not a reason to refuse the other 40,000 files.
    let Ok(read) = fs::read_dir(dir) else {
        return true;
    };
    for item in read.flatten() {
        if out.len() >= limit {
            return false;
        }
        let name = item.file_name();
        if SKIP.iter().any(|s| name == OsStr::new(s)) {
            continue;
        }
        let rel = prefix.join(&name);
        // `DirEntry::file_type` is the `lstat` type, so a symlink to a directory
        // is listed as a file and never descended into. That is both what a
        // symlink to `/` deserves and what makes the walk provably finite.
        match item.file_type() {
            Ok(kind) if kind.is_dir() => {
                if !walk(&item.path(), &rel, out, limit) {
                    return false;
                }
            }
            _ => out.push(rel),
        }
    }
    true
}

#[cfg(unix)]
fn os_string(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    OsStr::from_bytes(bytes).to_os_string()
}

#[cfg(not(unix))]
fn os_string(bytes: &[u8]) -> OsString {
    // Paths are UTF-16 there and git hands back UTF-8; lossy is the only
    // conversion available and the only one that ever loses anything.
    String::from_utf8_lossy(bytes).into_owned().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cap is a constant in the public API, so the only way to exercise it
    /// without creating fifty thousand files is from in here.
    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Temp {
            let path = std::env::temp_dir().join(format!("zemacs-project-cap-{tag}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Temp(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn hitting_the_cap_is_reported_rather_than_silently_applied() {
        let temp = Temp::new("walk");
        fs::create_dir(temp.0.join("sub")).unwrap();
        for n in 0..10 {
            fs::write(temp.0.join(format!("f{n}.txt")), "x").unwrap();
            fs::write(temp.0.join("sub").join(format!("g{n}.txt")), "x").unwrap();
        }

        let capped = list(&temp.0, 5).unwrap();
        assert!(capped.truncated);
        assert_eq!(capped.files.len(), 5);

        let whole = list(&temp.0, 100).unwrap();
        assert!(!whole.truncated);
        assert_eq!(whole.files.len(), 20);
    }

    /// Exactly at the cap is not truncation — there is nothing missing.
    #[test]
    fn a_list_that_ends_exactly_at_the_cap_is_not_truncated() {
        let temp = Temp::new("exact");
        for n in 0..4 {
            fs::write(temp.0.join(format!("f{n}.txt")), "x").unwrap();
        }
        let listed = list(&temp.0, 4).unwrap();
        assert!(!listed.truncated);
        assert_eq!(listed.files.len(), 4);
    }

    #[test]
    fn listing_something_that_is_not_a_directory_is_an_error() {
        let temp = Temp::new("notdir");
        let file = temp.0.join("f.txt");
        fs::write(&file, "x").unwrap();
        assert!(list(&file, 10).is_err());
        assert!(list(&temp.0.join("nope"), 10).is_err());
    }
}
