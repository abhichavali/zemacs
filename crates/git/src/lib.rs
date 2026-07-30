//! zemacs-git — the git half of the Magit clone.
//!
//! Two halves, deliberately kept apart:
//!
//! * **Talking to git** (this module): everything shells out to the `git`
//!   binary through [`std::process::Command`]. No libgit2 — the porcelain
//!   formats below are documented as stable, `git` is already installed on any
//!   machine that has a repository worth looking at, and a linked C library is
//!   a real build liability for an editor that mostly wants five commands.
//! * **Presentation** ([`render`]): a pure function from a [`Status`] to the
//!   status buffer's *text* plus one [`Line`] per line of that text. The UI
//!   draws the string and, when the cursor sits on line 12 and the user presses
//!   `s`, looks up `lines[12]` to learn what to stage. The two returns must
//!   therefore always have the same length — see [`render()`].
//!
//! Why `--porcelain=v2 -z` for status:
//!
//! * v1 is the human format with the colours turned off. It is explicitly not
//!   designed for parsing and it loses information we need: rename sources,
//!   ahead/behind counts, and whether HEAD is unborn or detached.
//! * `-z` terminates each record with NUL and, crucially, turns path quoting
//!   *off*. Without it git C-escapes and double-quotes any path holding a
//!   space, a quote or a non-ASCII byte (`"my dir \"q\"/caf\303\251.rs"`), and
//!   we would have to reimplement that unescaping byte-exactly. With `-z` the
//!   bytes between two NULs simply *are* the path. The one wrinkle is that `-z`
//!   also splits a rename record's `path<TAB>origPath` pair into two
//!   NUL-separated tokens, so parsing a `2 ` record consumes the record *after*
//!   it as well.
//!
//! Nothing here goes near a shell: every argument is a separate
//! [`Command::arg`], and user-supplied paths are passed after `--`, so a file
//! called `; rm -rf ~` or `-n` is just a file.
//!
//! ponytail: file-level staging only. Hunk-level staging is the same shape of
//! work — `git diff -U3 -- <path>` for the text, split on `@@` headers, feed
//! the chosen hunks back through `git apply --cached -` on stdin — but it needs
//! a hunk-aware cursor in the UI first. So [`Line::Hunk`] exists in the line
//! map and is never emitted yet.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};

pub mod render;
pub use render::{render, Line, Section};

/// Everything the status buffer needs to know about a repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Current branch, or `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Upstream tracking branch (`origin/main`), if one is configured.
    pub upstream: Option<String>,
    /// Commits we have that the upstream does not.
    pub ahead: usize,
    /// Commits the upstream has that we do not.
    pub behind: usize,
    pub staged: Vec<FileChange>,
    /// Worktree changes, including conflicts ([`ChangeKind::Unmerged`]) —
    /// staging a conflicted file is exactly how you mark it resolved.
    pub unstaged: Vec<FileChange>,
    pub untracked: Vec<PathBuf>,
    /// HEAD points at a commit rather than a branch.
    pub detached: bool,
    /// Repository has no commits yet, so HEAD is unborn.
    pub unborn: bool,
    /// A merge, rebase, cherry-pick or revert is half-finished.
    pub in_progress: Option<InProgress>,
}

impl Status {
    pub fn is_clean(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty() && self.untracked.is_empty()
    }

    pub fn has_conflicts(&self) -> bool {
        self.unstaged
            .iter()
            .any(|c| c.status == ChangeKind::Unmerged)
    }
}

/// A sequenced operation the repository is in the middle of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProgress {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl InProgress {
    pub fn label(self) -> &'static str {
        match self {
            InProgress::Merge => "merge",
            InProgress::Rebase => "rebase",
            InProgress::CherryPick => "cherry-pick",
            InProgress::Revert => "revert",
        }
    }
}

/// One changed file, on one side of the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Path relative to the repository root — what to hand [`stage`].
    pub path: PathBuf,
    pub status: ChangeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    /// Symlink became a regular file, or similar.
    TypeChanged,
    Renamed {
        from: PathBuf,
    },
    Copied {
        from: PathBuf,
    },
    /// Conflicted: the index holds stages 1/2/3 for this path.
    Unmerged,
}

impl ChangeKind {
    /// The single-letter prefix the status buffer shows, matching git's own.
    pub fn letter(&self) -> char {
        match self {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
            ChangeKind::TypeChanged => 'T',
            ChangeKind::Renamed { .. } => 'R',
            ChangeKind::Copied { .. } => 'C',
            ChangeKind::Unmerged => 'U',
        }
    }
}

// ---------------------------------------------------------------- commands

/// The repository containing `from`, or `None` if it is not in one.
///
/// `from` may be a file; its directory is used.
pub fn repo_root(from: &Path) -> Option<PathBuf> {
    let dir = if from.is_dir() { from } else { from.parent()? };
    let mut out = git(dir, ["rev-parse", "--show-toplevel"]).ok()?;
    // `rev-parse` prints the path raw and unquoted, so trimming the newline is
    // the whole of the parsing.
    while matches!(out.last(), Some(b'\n' | b'\r')) {
        out.pop();
    }
    (!out.is_empty()).then(|| to_path(&out))
}

/// Everything in the working tree and index, in one `git status` call.
///
/// Errors if `repo` is not a repository, rather than reporting a clean tree.
pub fn status(repo: &Path) -> Result<Status> {
    let out = git(
        repo,
        [
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
            "-z",
        ],
    )?;
    let mut status = parse(&out);
    status.in_progress = in_progress(repo);
    Ok(status)
}

/// Stage every change to `path`, including its deletion.
pub fn stage(repo: &Path, path: &Path) -> Result<()> {
    // `-A` so that a deleted file stages as a deletion; `--` so that a path
    // beginning with `-` is a path rather than a flag.
    git(
        repo,
        [
            OsStr::new("add"),
            OsStr::new("-A"),
            OsStr::new("--"),
            path.as_os_str(),
        ],
    )?;
    Ok(())
}

/// Remove `path`'s changes from the index, leaving the working tree alone.
///
/// A staged rename is two index entries, so fully unstaging one means calling
/// this for both the new path and the [`ChangeKind::Renamed`] `from` path.
pub fn unstage(repo: &Path, path: &Path) -> Result<()> {
    // `reset`, not `restore --staged`: restore has to resolve HEAD and so dies
    // in a repository with no commits yet, where unstaging is still legal.
    git(
        repo,
        [
            OsStr::new("reset"),
            OsStr::new("-q"),
            OsStr::new("--"),
            path.as_os_str(),
        ],
    )?;
    Ok(())
}

pub fn stage_all(repo: &Path) -> Result<()> {
    git(repo, ["add", "-A"])?;
    Ok(())
}

pub fn unstage_all(repo: &Path) -> Result<()> {
    git(repo, ["reset", "-q"])?;
    Ok(())
}

/// Commit the index. Returns git's own summary line, e.g.
/// `[main (root-commit) 8fd2b1c] first`.
pub fn commit(repo: &Path, message: &str) -> Result<String> {
    // Refused here rather than by git, so the UI gets a clean error instead of
    // an editor being spawned or a confusing "aborting commit due to empty
    // commit message".
    if message.trim().is_empty() {
        bail!("empty commit message");
    }
    // `--message=` rather than `-m <msg>`: a message that happens to start with
    // a dash cannot then be read as a flag.
    let out = run(repo, ["commit".to_string(), format!("--message={message}")])?;
    Ok(first_line(&out).unwrap_or_else(|| "committed".into()))
}

/// Push to the configured upstream. Fails loudly when there is none — that is
/// something the user has to see, not something to paper over.
///
/// ponytail: no `--set-upstream` variant, so the first push of a new branch has
/// to happen elsewhere. Adding one is `push --set-upstream <remote> HEAD`, but
/// it needs UI for choosing the remote.
pub fn push(repo: &Path) -> Result<String> {
    let out = run(repo, ["push"])?;
    Ok(last_line(&out).unwrap_or_else(|| "pushed".into()))
}

pub fn pull(repo: &Path) -> Result<String> {
    let out = run(repo, ["pull"])?;
    Ok(last_line(&out).unwrap_or_else(|| "pulled".into()))
}

/// Unified diff for one file, from the index (`staged`) or the working tree.
///
/// An untracked file has no diff and yields an empty string.
pub fn diff(repo: &Path, path: &Path, staged: bool) -> Result<String> {
    let mut args: Vec<OsString> = ["diff", "--no-color", "--no-ext-diff"]
        .iter()
        .map(OsString::from)
        .collect();
    if staged {
        args.push("--cached".into());
    }
    args.push("--".into());
    args.push(path.into());
    let out = git(repo, args)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

// ------------------------------------------------------------ running git

/// Run git in `repo`, erroring with git's own stderr when it fails.
fn run<I, S>(repo: &Path, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|a| a.as_ref().to_os_string())
        .collect();
    let out = Command::new("git")
        .current_dir(repo)
        .args(&args)
        // An editor must never be blocked by a child process waiting on a
        // terminal it does not have: no credential prompt on push, no editor
        // spawned for a merge commit message.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        // Lets a status refresh coexist with a `git` the user is running in a
        // terminal, instead of fighting over index.lock.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .with_context(|| format!("running `git {}`", pretty(&args)))?;

    if !out.status.success() {
        let mut msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if msg.is_empty() {
            msg = String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
        bail!("git {} failed: {msg}", pretty(&args));
    }
    Ok(out)
}

fn git<I, S>(repo: &Path, args: I) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Ok(run(repo, args)?.stdout)
}

fn pretty(args: &[OsString]) -> String {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `git commit` puts the interesting part (branch and new sha) first.
fn first_line(out: &Output) -> Option<String> {
    lines(out).next()
}

/// `git push`/`git pull` report progress first and the result last, on stderr.
fn last_line(out: &Output) -> Option<String> {
    lines(out).last()
}

fn lines(out: &Output) -> impl Iterator<Item = String> + '_ {
    let both = [&out.stdout, &out.stderr];
    both.into_iter()
        .flat_map(|b| {
            String::from_utf8_lossy(b)
                .lines()
                .map(|l| l.trim().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|l| !l.is_empty())
}

/// Git records paths as bytes, and on unix a path is bytes — going through
/// `String` would mangle any filename that is not valid UTF-8.
#[cfg(unix)]
fn to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Sequenced operations leave a marker file in the git directory; there is no
/// porcelain that reports them.
fn in_progress(repo: &Path) -> Option<InProgress> {
    let mut out = git(repo, ["rev-parse", "--absolute-git-dir"]).ok()?;
    while matches!(out.last(), Some(b'\n' | b'\r')) {
        out.pop();
    }
    let dir = to_path(&out);
    if dir.join("MERGE_HEAD").exists() {
        Some(InProgress::Merge)
    } else if dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists() {
        Some(InProgress::Rebase)
    } else if dir.join("CHERRY_PICK_HEAD").exists() {
        Some(InProgress::CherryPick)
    } else if dir.join("REVERT_HEAD").exists() {
        Some(InProgress::Revert)
    } else {
        None
    }
}

// ------------------------------------------------------------ the parser

/// Parse `git status --porcelain=v2 --branch -z`.
///
/// Records are NUL-terminated. The leading byte selects the shape:
///
/// ```text
/// # branch.oid <sha>|(initial)
/// # branch.head <branch>|(detached)
/// # branch.upstream <branch>            (only when one is configured)
/// # branch.ab +<ahead> -<behind>        (only when one is configured)
/// 1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
/// 2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\0<origPath>
/// u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
/// ? <path>
/// ```
///
/// `XY` is index status then worktree status, `.` meaning unchanged — which is
/// what puts one file into `staged`, `unstaged`, or both at once.
///
/// Only the final field can contain spaces, so a bounded `splitn` peels the
/// fixed columns off and leaves the path whole. Note the `2 ` record: under
/// `-z` its original path is the *next* NUL-terminated token, not a tab-joined
/// suffix, so the iterator is advanced by hand.
fn parse(out: &[u8]) -> Status {
    let mut status = Status::default();
    let mut records = out.split(|&b| b == 0);

    while let Some(rec) = records.next() {
        if rec.is_empty() {
            continue;
        }
        match rec[0] {
            b'#' => header(rec, &mut status),
            b'1' => {
                let f: Vec<&[u8]> = rec.splitn(9, |&b| b == b' ').collect();
                if f.len() < 9 {
                    continue;
                }
                push_change(&mut status, f[1], to_path(f[8]), None);
            }
            b'2' => {
                // Consumed before any early exit, or the iterator desynchronises
                // and the original path is read as the next record.
                let orig = records.next().map(to_path);
                let f: Vec<&[u8]> = rec.splitn(10, |&b| b == b' ').collect();
                if f.len() < 10 {
                    continue;
                }
                push_change(&mut status, f[1], to_path(f[9]), orig.as_deref());
            }
            b'u' => {
                let f: Vec<&[u8]> = rec.splitn(11, |&b| b == b' ').collect();
                if f.len() < 11 {
                    continue;
                }
                status.unstaged.push(FileChange {
                    path: to_path(f[10]),
                    status: ChangeKind::Unmerged,
                });
            }
            b'?' if rec.len() > 2 => status.untracked.push(to_path(&rec[2..])),
            // `!` ignored entries (never requested) and anything a future git
            // adds: skipping beats failing the whole status.
            _ => {}
        }
    }
    status
}

fn push_change(status: &mut Status, xy: &[u8], path: PathBuf, orig: Option<&Path>) {
    let (x, y) = (
        xy.first().copied().unwrap_or(b'.'),
        xy.get(1).copied().unwrap_or(b'.'),
    );
    if let Some(kind) = kind(x, orig) {
        status.staged.push(FileChange {
            path: path.clone(),
            status: kind,
        });
    }
    if let Some(kind) = kind(y, orig) {
        status.unstaged.push(FileChange { path, status: kind });
    }
}

fn kind(c: u8, from: Option<&Path>) -> Option<ChangeKind> {
    Some(match c {
        b'A' => ChangeKind::Added,
        b'M' => ChangeKind::Modified,
        b'D' => ChangeKind::Deleted,
        b'T' => ChangeKind::TypeChanged,
        // A rename with no source cannot come out of v2; drop it rather than
        // invent a path that would then be staged.
        b'R' => ChangeKind::Renamed {
            from: from?.to_path_buf(),
        },
        b'C' => ChangeKind::Copied {
            from: from?.to_path_buf(),
        },
        b'U' => ChangeKind::Unmerged,
        // `.` unchanged on this side, or a code we do not know.
        _ => return None,
    })
}

fn header(rec: &[u8], status: &mut Status) {
    // Branch names are bytes too, but they land in a `String`, so lossy is
    // forced here; it only ever affects display.
    let rec = String::from_utf8_lossy(rec);
    if let Some(v) = rec.strip_prefix("# branch.oid ") {
        status.unborn = v == "(initial)";
    } else if let Some(v) = rec.strip_prefix("# branch.head ") {
        if v == "(detached)" {
            status.detached = true;
        } else {
            status.branch = Some(v.to_string());
        }
    } else if let Some(v) = rec.strip_prefix("# branch.upstream ") {
        status.upstream = Some(v.to_string());
    } else if let Some(v) = rec.strip_prefix("# branch.ab ") {
        for token in v.split_whitespace() {
            if let Some(n) = token.strip_prefix('+') {
                status.ahead = n.parse().unwrap_or(0);
            } else if let Some(n) = token.strip_prefix('-') {
                status.behind = n.parse().unwrap_or(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parsing is pure, so the awkward records are worth pinning down here
    /// without a repository; the integration tests drive the real thing.
    #[test]
    fn parses_z_records() {
        let out = b"# branch.oid abc123\0# branch.head main\0# branch.upstream origin/main\0\
# branch.ab +2 -3\0\
1 .M N... 100644 100644 100644 aaa bbb my dir \"q\"/caf\xc3\xa9.rs\0\
2 R. N... 100644 100644 100644 ccc ddd R100 new name.txt\0old name.txt\0\
? untracked file.txt\0";
        let s = parse(out);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!((s.ahead, s.behind), (2, 3));
        assert!(!s.unborn && !s.detached);
        assert_eq!(s.unstaged[0].path, PathBuf::from("my dir \"q\"/café.rs"));
        assert_eq!(s.unstaged[0].status, ChangeKind::Modified);
        assert_eq!(s.staged[0].path, PathBuf::from("new name.txt"));
        assert_eq!(
            s.staged[0].status,
            ChangeKind::Renamed {
                from: PathBuf::from("old name.txt")
            }
        );
        assert_eq!(s.untracked, vec![PathBuf::from("untracked file.txt")]);
    }

    #[test]
    fn parses_unborn_and_detached() {
        let s = parse(b"# branch.oid (initial)\0# branch.head main\0");
        assert!(s.unborn);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!((s.ahead, s.behind), (0, 0));

        let s = parse(b"# branch.oid abc\0# branch.head (detached)\0");
        assert!(s.detached);
        assert_eq!(s.branch, None);
    }

    /// A file changed on both sides shows up twice, once per bucket.
    #[test]
    fn splits_index_and_worktree() {
        let s = parse(b"1 MM N... 100644 100644 100644 aaa bbb both.rs\0");
        assert_eq!(s.staged[0].status, ChangeKind::Modified);
        assert_eq!(s.unstaged[0].status, ChangeKind::Modified);
    }
}
