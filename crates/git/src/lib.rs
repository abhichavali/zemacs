//! zemacs-git — the git half of the Magit clone.
//!
//! Three parts, deliberately kept apart:
//!
//! * **Talking to git** (this module, [`hunk`] and [`rebase`]): everything
//!   shells out to the `git` binary through [`std::process::Command`]. No
//!   libgit2 — the porcelain formats below are documented as stable, `git` is
//!   already installed on any machine that has a repository worth looking at,
//!   and a linked C library is a real build liability for an editor that mostly
//!   wants a dozen commands.
//! * **What the user has opened** ([`View`]): the status buffer folds, and a
//!   file's diff is only read when someone asks to see it, so *something* has to
//!   remember that across refreshes. That is all a `View` is: a [`Status`], the
//!   diffs currently open, and the sections currently shut.
//! * **Presentation** ([`render`]): a pure function from a [`View`] to the
//!   status buffer's *text*, one [`Line`] per line of that text, and the
//!   [`Span`]s that colour it. The UI draws the string and, when the cursor sits
//!   on line 12 and the user presses `s`, looks up `lines[12]` to learn what to
//!   stage. Text and map must therefore always have the same length — see
//!   [`render()`]. The spans name faces ([`Face`]) rather than colours, so a
//!   status buffer picks up whatever theme is loaded for free.
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
//! called `; rm -rf ~` or `-n` is just a file. Revisions cannot be passed after
//! `--`, so they go through [`rev`] instead, which refuses anything that would
//! read as a flag. The single exception is git's own sequence editor, which is a
//! shell command by construction — see [`rebase`] for how that is kept safe.
//!
//! Anything that can lose work is its own named function and says so in its
//! first line: [`reset_hard`], [`discard`], [`discard_untracked`],
//! [`discard_hunk`], [`stash_drop`], [`branch_delete_force`], [`rebase_abort`],
//! [`drop_commit`], [`amend`], [`reword`], [`squash`]. None of them is the
//! fallback arm of anything.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{bail, Context, Result};

mod hunk;
mod rebase;
pub mod render;

pub use hunk::{discard_hunk, file_diff, stage_hunk, unstage_hunk, FileDiff, Hunk};
pub use rebase::{
    amend, drop_commit, parse_todo, plan_last, plan_onto, rebase_abort, rebase_continue,
    rebase_skip, rebase_start, rebase_state, reword, squash, write_todo, Action, Plan, Rebase,
    RebaseOutcome, TodoItem,
};
pub use render::{render, Face, Line, Section, Span};

/// How many commits the status buffer's log section shows. Magit's number.
const RECENT: usize = 10;

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
    /// Detail for the rebase `in_progress` names, when it names one. Carries
    /// the conflicted paths and what is left to replay.
    pub rebase: Option<Rebase>,
    pub stashes: Vec<Stash>,
    /// The last [`RECENT`] commits, newest first. Empty on an unborn HEAD.
    pub recent: Vec<Commit>,
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

    /// Whether `section` still lists `path`. What tells a refresh to keep a
    /// file's diff open or let it fall shut because the file was staged.
    fn holds(&self, section: Section, path: &Path) -> bool {
        let list = match section {
            Section::Staged => &self.staged,
            Section::Unstaged => &self.unstaged,
            _ => return false,
        };
        list.iter().any(|change| change.path == path)
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

/// A commit, at the resolution the status buffer shows it: hash and subject.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Commit {
    /// Abbreviated, as git chose to abbreviate it — long enough to be unique in
    /// this repository, which is the only place it will ever be used.
    pub hash: String,
    pub subject: String,
}

/// One entry of the stash stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    /// `stash@{0}` — what to hand [`stash_pop`] and friends. Note that these
    /// renumber as soon as anything is pushed or dropped, so a name is only
    /// good until the next refresh.
    pub name: String,
    pub subject: String,
}

/// A local branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    /// Whether HEAD is on it.
    pub head: bool,
}

/// The status buffer's state: what git said, plus how much of it is open.
///
/// The UI holds one of these for the life of the buffer, so folds and open
/// diffs survive a refresh. Everything in it is public because the UI is
/// entitled to look; go through the methods to change it, since opening a file
/// means reading a diff.
#[derive(Debug, Clone, Default)]
pub struct View {
    pub status: Status,
    /// Diffs for the files the user has opened, in no useful order — there are
    /// only ever a handful, so a scan beats a map.
    pub diffs: Vec<FileDiff>,
    /// Sections folded shut. Empty is Magit's default: everything open.
    pub collapsed: Vec<Section>,
}

impl View {
    /// Read the repository, with nothing opened.
    pub fn load(repo: &Path) -> Result<View> {
        Ok(View::of(status(repo)?))
    }

    /// Wrap a [`Status`] already in hand.
    pub fn of(status: Status) -> View {
        View {
            status,
            diffs: Vec::new(),
            collapsed: Vec::new(),
        }
    }

    /// Re-read git, keeping the folds and reloading every open diff.
    ///
    /// A file that has left its section — staged, committed, discarded — simply
    /// falls shut, because there is nothing left there to show.
    pub fn refresh(&mut self, repo: &Path) -> Result<()> {
        self.status = status(repo)?;
        let open: Vec<(Section, PathBuf)> = self
            .diffs
            .iter()
            .map(|diff| (diff.section, diff.path.clone()))
            .collect();
        self.diffs.clear();
        for (section, path) in open {
            if self.status.holds(section, &path) {
                if let Ok(diff) = file_diff(repo, &path, section) {
                    self.diffs.push(diff);
                }
            }
        }
        Ok(())
    }

    pub fn is_collapsed(&self, section: Section) -> bool {
        self.collapsed.contains(&section)
    }

    /// Fold a section shut, or open it again.
    pub fn toggle_section(&mut self, section: Section) {
        match self.collapsed.iter().position(|s| *s == section) {
            Some(at) => {
                self.collapsed.remove(at);
            }
            None => self.collapsed.push(section),
        }
    }

    /// The open diff for a file, if it is open.
    pub fn diff_of(&self, section: Section, path: &Path) -> Option<&FileDiff> {
        self.diffs
            .iter()
            .find(|diff| diff.section == section && diff.path == path)
    }

    /// Show a file's diff, or hide it again. Showing reads it from git.
    ///
    /// Errors for a section that has no diffs, [`Section::Untracked`] included:
    /// git has nothing to say about a file it is not tracking.
    pub fn toggle_file(&mut self, repo: &Path, section: Section, path: &Path) -> Result<()> {
        if let Some(at) = self
            .diffs
            .iter()
            .position(|diff| diff.section == section && diff.path == path)
        {
            self.diffs.remove(at);
            return Ok(());
        }
        self.diffs.push(file_diff(repo, path, section)?);
        Ok(())
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

/// Everything in the working tree and index, plus the stash, the recent log and
/// any rebase in flight.
///
/// Errors if `repo` is not a repository, rather than reporting a clean tree.
/// The extra reads after that first one are best-effort: a repository with no
/// commits has no log and one with no stash ref has no stashes, and neither is
/// a reason to have no status buffer.
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
    status.rebase = rebase_state(repo).ok().flatten();
    status.stashes = stashes(repo).unwrap_or_default();
    status.recent = log(repo, RECENT).unwrap_or_default();
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

/// **Destroys work.** Overwrites `path` in the working tree with whatever is in
/// the index, so every unstaged edit to it — everything the status buffer shows
/// under "Unstaged changes" for that file — is gone. Nothing is committed, so
/// no reflog remembers it and there is no way back.
///
/// A file that is only untracked is not touched by this; [`discard_untracked`]
/// is the one that deletes those, and it is separate on purpose.
pub fn discard(repo: &Path, path: &Path) -> Result<()> {
    git(
        repo,
        [OsStr::new("checkout"), OsStr::new("--"), path.as_os_str()],
    )?;
    Ok(())
}

/// **Destroys work.** Deletes `path` from disk. git never had a copy — that is
/// what untracked means — so this is exactly as recoverable as `rm`, which is
/// to say not at all.
pub fn discard_untracked(repo: &Path, path: &Path) -> Result<()> {
    // `-d` so an untracked directory goes too; no `-x`, so an ignored file is
    // left alone even if the user is standing on one.
    git(
        repo,
        [
            OsStr::new("clean"),
            OsStr::new("-q"),
            OsStr::new("-f"),
            OsStr::new("-d"),
            OsStr::new("--"),
            path.as_os_str(),
        ],
    )?;
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

/// Update the remote-tracking branches without touching anything local.
pub fn fetch(repo: &Path) -> Result<String> {
    let out = run(repo, ["fetch"])?;
    Ok(last_line(&out).unwrap_or_else(|| "fetched".into()))
}

/// Unified diff for one file, from the index (`staged`) or the working tree.
///
/// An untracked file has no diff and yields an empty string. [`file_diff`] is
/// the same thing already cut into hunks, which is what the status buffer wants.
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

/// The last `n` commits on HEAD, newest first.
pub fn log(repo: &Path, n: usize) -> Result<Vec<Commit>> {
    let out = git(
        repo,
        [
            "log".to_string(),
            "-z".into(),
            "--no-color".into(),
            format!("--max-count={n}"),
            "--format=%h%x1f%s".into(),
        ],
    )?;
    Ok(records(&out)
        .into_iter()
        .map(|(hash, subject)| Commit { hash, subject })
        .collect())
}

/// The stash stack, newest first.
pub fn stashes(repo: &Path) -> Result<Vec<Stash>> {
    let out = git(
        repo,
        ["stash", "list", "-z", "--no-color", "--format=%gd%x1f%s"],
    )?;
    Ok(records(&out)
        .into_iter()
        .map(|(name, subject)| Stash { name, subject })
        .collect())
}

/// Put the working tree and index on the stash stack and go back to HEAD.
///
/// Nothing is destroyed: the stash entry holds it all. Untracked files are left
/// where they are, which is what `git stash` does without `-u`.
pub fn stash_push(repo: &Path, message: &str) -> Result<String> {
    let mut args = vec!["stash".to_string(), "push".into()];
    if !message.trim().is_empty() {
        args.push(format!("--message={message}"));
    }
    let out = run(repo, args)?;
    Ok(last_line(&out).unwrap_or_else(|| "stashed".into()))
}

/// Apply a stash entry and remove it from the stack.
///
/// A conflict leaves the entry alone, so nothing is lost either way.
pub fn stash_pop(repo: &Path, name: &str) -> Result<String> {
    let out = run(repo, ["stash".to_string(), "pop".into(), rev(name)?.into()])?;
    Ok(last_line(&out).unwrap_or_else(|| "popped".into()))
}

/// Apply a stash entry and leave it on the stack.
pub fn stash_apply(repo: &Path, name: &str) -> Result<String> {
    let out = run(
        repo,
        ["stash".to_string(), "apply".into(), rev(name)?.into()],
    )?;
    Ok(last_line(&out).unwrap_or_else(|| "applied".into()))
}

/// **Destroys work.** Deletes a stash entry and everything in it, without
/// applying it anywhere first. Stash entries are unreachable commits, so this
/// leaves nothing but a reflog line most people do not know how to read. Note
/// also that dropping `stash@{0}` renumbers everything below it.
pub fn stash_drop(repo: &Path, name: &str) -> Result<String> {
    let out = run(
        repo,
        ["stash".to_string(), "drop".into(), rev(name)?.into()],
    )?;
    Ok(last_line(&out).unwrap_or_else(|| "dropped".into()))
}

/// The local branches, in git's order, with HEAD's marked.
pub fn branches(repo: &Path) -> Result<Vec<Branch>> {
    // A branch name can hold neither a space nor a newline, so the plainest
    // possible format is already unambiguous: `<name> *` for HEAD's.
    let out = git(
        repo,
        [
            "for-each-ref",
            "--format=%(refname:short) %(HEAD)",
            "refs/heads/",
        ],
    )?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next()?;
            Some(Branch {
                name: name.to_string(),
                head: fields.next() == Some("*"),
            })
        })
        .collect())
}

/// Make a branch at `start` (or at HEAD) without moving onto it.
pub fn branch_create(repo: &Path, name: &str, start: Option<&str>) -> Result<()> {
    let mut args = vec!["branch".to_string(), rev(name)?.to_string()];
    if let Some(start) = start {
        args.push(rev(start)?.to_string());
    }
    run(repo, args)?;
    Ok(())
}

/// Move HEAD onto `rev`, taking uncommitted changes along where git can.
///
/// git refuses rather than overwrite a modified file, so nothing is lost here.
pub fn checkout(repo: &Path, revision: &str) -> Result<String> {
    let out = run(repo, ["checkout".to_string(), rev(revision)?.to_string()])?;
    Ok(last_line(&out).unwrap_or_else(|| "checked out".into()))
}

/// Make a branch at `start` (or at HEAD) and move onto it.
pub fn checkout_new(repo: &Path, name: &str, start: Option<&str>) -> Result<String> {
    let mut args = vec!["checkout".to_string(), "-b".into(), rev(name)?.to_string()];
    if let Some(start) = start {
        args.push(rev(start)?.to_string());
    }
    let out = run(repo, args)?;
    Ok(last_line(&out).unwrap_or_else(|| "checked out".into()))
}

/// Delete a branch, refusing if its commits are not merged anywhere else.
///
/// That refusal is the whole point of this being separate from
/// [`branch_delete_force`]; it comes back as an error carrying git's own words.
pub fn branch_delete(repo: &Path, name: &str) -> Result<String> {
    let out = run(
        repo,
        ["branch".to_string(), "-d".into(), rev(name)?.to_string()],
    )?;
    Ok(last_line(&out).unwrap_or_else(|| "deleted".into()))
}

/// **Destroys work.** Deletes a branch even when its commits are on no other
/// branch, which orphans every one of them. They live on in the reflog until it
/// expires, and nowhere a status buffer will ever show them.
pub fn branch_delete_force(repo: &Path, name: &str) -> Result<String> {
    let out = run(
        repo,
        ["branch".to_string(), "-D".into(), rev(name)?.to_string()],
    )?;
    Ok(last_line(&out).unwrap_or_else(|| "deleted".into()))
}

/// Move the branch to `rev`, keeping the index and the working tree.
///
/// The commits between end up staged, ready to be recommitted. Nothing is lost.
pub fn reset_soft(repo: &Path, revision: &str) -> Result<()> {
    run(
        repo,
        [
            "reset".to_string(),
            "--soft".into(),
            rev(revision)?.to_string(),
        ],
    )?;
    Ok(())
}

/// Move the branch to `rev` and reset the index to match, keeping the working
/// tree. The changes end up unstaged, which is git's default `reset`.
pub fn reset_mixed(repo: &Path, revision: &str) -> Result<()> {
    run(
        repo,
        [
            "reset".to_string(),
            "--mixed".into(),
            rev(revision)?.to_string(),
        ],
    )?;
    Ok(())
}

/// **Destroys work.** Moves the branch to `rev` and overwrites both the index
/// and the working tree with it. Every staged and unstaged change to a tracked
/// file is gone, uncommitted and unrecoverable; the commits that were passed
/// over survive only in the reflog. Untracked files are the one thing this
/// leaves alone.
pub fn reset_hard(repo: &Path, revision: &str) -> Result<()> {
    run(
        repo,
        [
            "reset".to_string(),
            "--hard".into(),
            rev(revision)?.to_string(),
        ],
    )?;
    Ok(())
}

/// Replay `rev` on top of HEAD.
///
/// ponytail: a conflict comes back as an error carrying git's own words, and
/// leaves the repository mid-cherry-pick — which the status buffer does say, as
/// `in_progress`. Making this return a state the way [`rebase_start`] does needs
/// `--continue`/`--abort` wired up to go with it.
pub fn cherry_pick(repo: &Path, revision: &str) -> Result<String> {
    let out = run(
        repo,
        ["cherry-pick".to_string(), rev(revision)?.to_string()],
    )?;
    Ok(last_line(&out).unwrap_or_else(|| "cherry-picked".into()))
}

/// Commit the inverse of `rev` on top of HEAD.
///
/// Nothing is rewritten: undoing a commit this way is itself a commit. Same
/// conflict caveat as [`cherry_pick`].
pub fn revert(repo: &Path, revision: &str) -> Result<String> {
    let out = run(repo, ["revert".to_string(), rev(revision)?.to_string()])?;
    Ok(last_line(&out).unwrap_or_else(|| "reverted".into()))
}

// ------------------------------------------------------------ running git

/// Run git in `repo`, with extra environment and optional stdin, erroring with
/// git's own stderr when it fails.
pub(crate) fn run_with<I, S>(
    repo: &Path,
    args: I,
    env: &[(&str, &OsStr)],
    stdin: Option<&str>,
) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|a| a.as_ref().to_os_string())
        .collect();
    let mut command = Command::new("git");
    command
        .current_dir(repo)
        .args(&args)
        // An editor must never be blocked by a child process waiting on a
        // terminal it does not have: no credential prompt on push, no editor
        // spawned for a merge commit message.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        // Lets a status refresh coexist with a `git` the user is running in a
        // terminal, instead of fighting over index.lock.
        .env("GIT_OPTIONAL_LOCKS", "0");
    for (key, value) in env {
        command.env(key, value);
    }

    let out = match stdin {
        None => command.output(),
        Some(input) => {
            // `output()` cannot both write and read, so the pipe is wired by
            // hand. stdout and stderr are still pipes, so a patch big enough to
            // fill git's own output buffer cannot deadlock us.
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command
                .spawn()
                .with_context(|| format!("running `git {}`", pretty(&args)))?;
            {
                use std::io::Write;
                let mut pipe = child.stdin.take().context("git took no stdin")?;
                pipe.write_all(input.as_bytes())
                    .with_context(|| format!("feeding `git {}`", pretty(&args)))?;
            }
            child.wait_with_output()
        }
    }
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

fn run<I, S>(repo: &Path, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_with(repo, args, &[], None)
}

pub(crate) fn git<I, S>(repo: &Path, args: I) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Ok(run(repo, args)?.stdout)
}

/// Refuse a revision that git would read as an option.
///
/// Paths get `--`, but revisions cannot: `git branch -- -f` is not a thing. A
/// ref name legally cannot begin with a dash anyway, so anything that does is a
/// mistake or an attack and either way is not a revision.
pub(crate) fn rev(revision: &str) -> Result<&str> {
    if revision.is_empty() || revision.starts_with('-') {
        bail!("{revision:?} is not a revision");
    }
    Ok(revision)
}

/// Resolve a revision to its full hash, erroring if there is no such thing.
pub(crate) fn rev_parse(repo: &Path, revision: &str) -> Result<String> {
    let out = git(
        repo,
        [
            "rev-parse".to_string(),
            "--verify".into(),
            rev(revision)?.to_string(),
        ],
    )?;
    let sha = String::from_utf8_lossy(&out).trim().to_string();
    if sha.is_empty() {
        bail!("{revision} does not name a commit");
    }
    Ok(sha)
}

/// One commit's hash and subject.
pub(crate) fn commit_of(repo: &Path, revision: &str) -> Result<Commit> {
    let out = git(
        repo,
        [
            "show".to_string(),
            "-s".into(),
            "-z".into(),
            "--no-color".into(),
            "--format=%h%x1f%s".into(),
            rev(revision)?.to_string(),
        ],
    )?;
    records(&out)
        .into_iter()
        .next()
        .map(|(hash, subject)| Commit { hash, subject })
        .with_context(|| format!("no commit {revision}"))
}

/// Split `--format=…%x1f…` output run under `-z`: NUL between records, unit
/// separator between the two fields. Chosen over newlines because a commit
/// subject is arbitrary text and these two bytes are the ones it cannot hold.
pub(crate) fn records(out: &[u8]) -> Vec<(String, String)> {
    out.split(|&b| b == 0)
        .filter(|rec| !rec.is_empty())
        .filter_map(|rec| {
            let text = String::from_utf8_lossy(rec);
            let (head, rest) = text.split_once('\u{1f}')?;
            Some((head.to_string(), rest.to_string()))
        })
        .collect()
}

fn pretty(args: &[OsString]) -> String {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `git commit` puts the interesting part (branch and new sha) first.
pub(crate) fn first_line(out: &Output) -> Option<String> {
    lines(out).next()
}

/// `git push`/`git pull` report progress first and the result last, on stderr.
pub(crate) fn last_line(out: &Output) -> Option<String> {
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
pub(crate) fn to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
pub(crate) fn to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// The `.git` directory, which is where every trace of a half-finished
/// operation lives. Not always `repo/.git`: a worktree's is elsewhere.
pub(crate) fn git_dir(repo: &Path) -> Result<PathBuf> {
    let mut out = git(repo, ["rev-parse", "--absolute-git-dir"])?;
    while matches!(out.last(), Some(b'\n' | b'\r')) {
        out.pop();
    }
    Ok(to_path(&out))
}

/// Sequenced operations leave a marker file in the git directory; there is no
/// porcelain that reports them.
fn in_progress(repo: &Path) -> Option<InProgress> {
    let dir = git_dir(repo).ok()?;
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

    /// A revision is the one argument that cannot hide behind `--`.
    #[test]
    fn a_revision_that_looks_like_a_flag_is_refused() {
        assert!(rev("--hard").is_err());
        assert!(rev("-f").is_err());
        assert!(rev("").is_err());
        assert!(rev("stash@{0}").is_ok());
        assert!(rev("origin/main").is_ok());
    }

    #[test]
    fn records_split_on_the_two_bytes_a_subject_cannot_hold() {
        let out = b"abc123\x1ffirst subject\0def456\x1fa \"quoted\" one\0";
        assert_eq!(
            records(out),
            vec![
                ("abc123".to_string(), "first subject".to_string()),
                ("def456".to_string(), "a \"quoted\" one".to_string()),
            ]
        );
        // A record with no separator is dropped rather than half-read.
        assert!(records(b"nonsense\0").is_empty());
    }
}
