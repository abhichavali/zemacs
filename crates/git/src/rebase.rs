//! Interactive rebase, with the editor on our side of the fence.
//!
//! `git rebase -i` writes a todo list, runs `$GIT_SEQUENCE_EDITOR` on it, and
//! then does whatever the file says when the editor exits. Every tool that
//! wants to drive a rebase has to answer the same question: how do you get your
//! todo list into that file without being an editor?
//!
//! Driving a real editor is the wrong answer — it means a subprocess that wants
//! a terminal, inside an editor that already has the buffer. So the todo list is
//! built here, written to a file, and the "editor" is a `cp` that copies it over
//! the one git generated. git then reads back exactly what zemacs decided, and
//! no editor was ever launched.
//!
//! The one subtlety is quoting. git runs the sequence editor through `sh`,
//! appending `"$@"` (the todo file) to whatever string it was given, so the
//! string is a shell command whether we like it or not. The path of *our* file
//! therefore travels in the environment and is expanded inside double quotes —
//! [`SEQUENCE_EDITOR`] is a constant with no interpolation in it, so a
//! repository under `/tmp/it's here; rm -rf ~/` is a directory name and not a
//! command. This is the only place in the crate a shell is involved, and it is
//! git's shell, not ours.
//!
//! A rebase that stops is not a failure. Conflicts are the *expected* outcome of
//! reordering commits, and an `edit` or `break` step stops on purpose, so every
//! entry point returns [`RebaseOutcome`] and reserves `Err` for the cases where
//! nothing happened at all. What it stopped on, and which paths conflict, come
//! back in [`Rebase`] — the same struct [`crate::status`] hangs off the status
//! buffer, so "stopped" renders itself.
//!
//! ponytail: `sh` and `cp`, so this is unix. A Windows port needs a sequence
//! editor that exists there; the rest of the module is unchanged.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::{bail, Context, Result};

use crate::{git_dir, rev, rev_parse, run_with, Commit};

/// What git runs instead of opening an editor: `cp -- <ours> <git's>`.
///
/// git turns this into `sh -c 'cp -- "$ZEMACS_REBASE_TODO" "$@"' … <todofile>`,
/// so the destination is git's own argument and the source is an environment
/// variable expanded inside quotes — one word no matter what is in it. `--`
/// because a git directory can sit under a path beginning with a dash.
const SEQUENCE_EDITOR: &str = r#"cp -- "$ZEMACS_REBASE_TODO""#;

/// One step of a rebase todo list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup,
    Drop,
    Exec,
    Break,
}

impl Action {
    /// The word git writes in the todo file.
    pub fn keyword(self) -> &'static str {
        match self {
            Action::Pick => "pick",
            Action::Reword => "reword",
            Action::Edit => "edit",
            Action::Squash => "squash",
            Action::Fixup => "fixup",
            Action::Drop => "drop",
            Action::Exec => "exec",
            Action::Break => "break",
        }
    }

    /// Every spelling git accepts, including the one-letter abbreviations a
    /// hand-edited todo list is full of.
    pub fn parse(word: &str) -> Option<Action> {
        Some(match word {
            "p" | "pick" => Action::Pick,
            "r" | "reword" => Action::Reword,
            "e" | "edit" => Action::Edit,
            "s" | "squash" => Action::Squash,
            "f" | "fixup" => Action::Fixup,
            "d" | "drop" => Action::Drop,
            "x" | "exec" => Action::Exec,
            "b" | "break" => Action::Break,
            _ => return None,
        })
    }

    /// Whether the step names a commit. `exec` and `break` do not.
    pub fn takes_commit(self) -> bool {
        !matches!(self, Action::Exec | Action::Break)
    }
}

/// One line of a todo list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub action: Action,
    /// Abbreviated hash, empty for [`Action::Exec`] and [`Action::Break`].
    pub hash: String,
    /// The commit's subject — or, for `exec`, the command line to run, because
    /// that is the field git puts it in and keeping them apart would mean two
    /// shapes of item for one shape of line.
    pub subject: String,
}

impl TodoItem {
    pub fn pick(hash: impl Into<String>, subject: impl Into<String>) -> TodoItem {
        TodoItem {
            action: Action::Pick,
            hash: hash.into(),
            subject: subject.into(),
        }
    }
}

/// A rebase worked out but not yet run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// What the todo list replays onto. `None` means it reaches the root
    /// commit, which git spells `--root` rather than as a revision.
    pub base: Option<String>,
    /// Oldest first, the order git writes and applies them in.
    pub todo: Vec<TodoItem>,
}

/// A rebase that is half-finished, whether zemacs started it or a terminal did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rebase {
    /// The branch being rebased, `None` when it started from a detached HEAD.
    pub branch: Option<String>,
    /// Abbreviated hash of what it is replaying onto.
    pub onto: String,
    /// Steps applied, and steps in the whole list.
    pub done: usize,
    pub total: usize,
    /// The commit it stopped on, when it stopped on one.
    pub stopped: Option<Commit>,
    /// Paths with unresolved conflicts. **Empty is meaningful**: it means the
    /// rebase stopped deliberately, at an `edit` or a `break`, and is waiting
    /// rather than complaining.
    pub conflicts: Vec<PathBuf>,
    /// What is left to do, oldest first, excluding the step it stopped on.
    pub todo: Vec<TodoItem>,
}

impl Rebase {
    pub fn is_conflicted(&self) -> bool {
        !self.conflicts.is_empty()
    }
}

/// How a rebase command ended. Neither variant is an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// Ran to the end; nothing is in progress. Carries git's last word, for the
    /// echo area.
    Done(String),
    /// Stopped and wants the user — a conflict to resolve, or an `edit`/`break`
    /// step to work at. Check [`Rebase::is_conflicted`] to tell which.
    Stopped { rebase: Rebase, message: String },
}

// -------------------------------------------------------------- todo files

/// Parse a `git-rebase-todo`.
///
/// Comments and blank lines are dropped: they are git's own help text and
/// regenerating them is git's job. A line whose first word is not an action is
/// an **error** rather than a skip — silently dropping a `pick` would silently
/// drop a commit, and this file is the only record of what the rebase is
/// supposed to do.
///
/// ponytail: the `label`/`reset`/`merge` steps that `rebase --rebase-merges`
/// writes are rejected here. Supporting them means three more variants and a
/// second commit-or-label field on [`TodoItem`]; nothing in the status buffer
/// asks for it yet.
pub fn parse_todo(text: &str) -> Result<Vec<TodoItem>> {
    let mut items = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = match line.split_once(char::is_whitespace) {
            Some((word, rest)) => (word, rest.trim_start()),
            None => (line, ""),
        };
        let Some(action) = Action::parse(word) else {
            bail!("unknown rebase todo step {line:?}");
        };
        // `exec`'s argument is a command, not a hash, so it stays whole.
        let (hash, subject) = if action.takes_commit() {
            match rest.split_once(char::is_whitespace) {
                Some((hash, subject)) => (hash.to_string(), subject.trim_start().to_string()),
                None => (rest.to_string(), String::new()),
            }
        } else {
            (String::new(), rest.to_string())
        };
        items.push(TodoItem {
            action,
            hash,
            subject,
        });
    }
    Ok(items)
}

/// Serialise a todo list back to the file format.
///
/// `parse_todo(&write_todo(&items))` gives `items` back unchanged, which is what
/// makes it safe to read a rebase's remaining steps, reorder them, and hand them
/// back.
pub fn write_todo(items: &[TodoItem]) -> String {
    let mut text = String::new();
    for item in items {
        text.push_str(item.action.keyword());
        if !item.hash.is_empty() {
            text.push(' ');
            text.push_str(&item.hash);
        }
        if !item.subject.is_empty() {
            text.push(' ');
            text.push_str(&item.subject);
        }
        text.push('\n');
    }
    text
}

// ---------------------------------------------------------------- planning

/// The todo list `git rebase -i HEAD~n` would have opened an editor on.
///
/// Fewer than `n` commits is not an error: the plan then reaches the root and
/// `base` is `None`.
pub fn plan_last(repo: &Path, n: usize) -> Result<Plan> {
    if n == 0 {
        bail!("nothing to rebase");
    }
    let todo = log_todo(repo, &[format!("--max-count={n}"), "HEAD".into()])?;
    if todo.is_empty() {
        bail!("no commits to rebase");
    }
    // Fails exactly when the range runs off the start of history, which is the
    // `--root` case rather than a problem.
    Ok(Plan {
        base: rev_parse(repo, &format!("HEAD~{n}")).ok(),
        todo,
    })
}

/// Replay everything `base` does not have onto `base` — Magit's `r e`.
pub fn plan_onto(repo: &Path, base: &str) -> Result<Plan> {
    let base = rev_parse(repo, base)?;
    let todo = log_todo(repo, &[format!("{base}..HEAD")])?;
    if todo.is_empty() {
        bail!("nothing to rebase onto {base}");
    }
    Ok(Plan {
        base: Some(base),
        todo,
    })
}

/// A plan whose todo list starts at `commit`, so `todo[0]` is that commit and
/// `base` is its parent. The building block under reword/squash/drop.
fn plan_from(repo: &Path, commit: &str) -> Result<Plan> {
    let commit = rev_parse(repo, commit)?;
    let base = rev_parse(repo, &format!("{commit}^")).ok();
    let range = match &base {
        Some(base) => format!("{base}..HEAD"),
        None => "HEAD".to_string(),
    };
    let todo = log_todo(repo, &[range])?;
    // The abbreviated hash git prints is a prefix of the full one it resolved,
    // so this both checks the range came out right and that `commit` is on the
    // current branch at all.
    match todo.first() {
        Some(first) if commit.starts_with(&first.hash) => Ok(Plan { base, todo }),
        _ => bail!("{commit} is not on the current branch"),
    }
}

/// `git log`, oldest first, as pick lines.
///
/// `-z` separates commits with NUL and `%x1f` separates the two fields, so a
/// subject can hold anything printable without splitting the record.
fn log_todo(repo: &Path, args: &[String]) -> Result<Vec<TodoItem>> {
    let mut argv = vec![
        "log".to_string(),
        "-z".into(),
        "--reverse".into(),
        "--no-color".into(),
        "--format=%h%x1f%s".into(),
    ];
    argv.extend(args.iter().cloned());
    Ok(crate::records(&crate::git(repo, argv)?)
        .into_iter()
        .map(|(hash, subject)| TodoItem::pick(hash, subject))
        .collect())
}

// ----------------------------------------------------------------- running

/// Run `plan`, with its todo list already decided, spawning no editor.
///
/// A conflict comes back as [`RebaseOutcome::Stopped`], not as an error. git
/// refuses to start at all if the working tree is dirty, and that *is* an error.
pub fn rebase_start(repo: &Path, plan: &Plan) -> Result<RebaseOutcome> {
    if plan.todo.is_empty() {
        bail!("empty rebase todo list");
    }
    if plan.todo.iter().all(|i| i.action == Action::Drop) {
        // git calls this "nothing to do" and aborts halfway through setting up;
        // saying so here is clearer than letting it.
        bail!("a rebase that drops every commit has nothing to do");
    }
    // Lives in the git directory so it is on the same filesystem as the todo
    // file it replaces and cannot collide with another repository's rebase.
    let file = git_dir(repo)?.join("zemacs-rebase-todo");
    fs::write(&file, write_todo(&plan.todo))
        .with_context(|| format!("writing {}", file.display()))?;

    let mut args = vec!["rebase".to_string(), "-i".into()];
    match &plan.base {
        Some(base) => args.push(rev(base)?.to_string()),
        None => args.push("--root".into()),
    }
    let out = run_with(
        repo,
        args,
        &[
            ("GIT_SEQUENCE_EDITOR", OsStr::new(SEQUENCE_EDITOR)),
            ("ZEMACS_REBASE_TODO", file.as_os_str()),
        ],
        None,
    );
    // git has read it by now whether it stopped or finished; leaving it behind
    // would make the next rebase's failure look like a stale success.
    let _ = fs::remove_file(&file);
    outcome(repo, out)
}

/// Carry on after resolving a conflict or finishing an `edit` step.
///
/// Stopping again on the next conflict is [`RebaseOutcome::Stopped`], the same
/// as the first one.
pub fn rebase_continue(repo: &Path) -> Result<RebaseOutcome> {
    outcome(repo, run_with(repo, ["rebase", "--continue"], &[], None))
}

/// Drop the commit the rebase is stopped on and carry on.
///
/// Nothing is destroyed: the commit is still in the reflog and on whatever the
/// rebase started from, it just does not get replayed.
pub fn rebase_skip(repo: &Path) -> Result<RebaseOutcome> {
    outcome(repo, run_with(repo, ["rebase", "--skip"], &[], None))
}

/// **Destroys work.** Throws away every commit the rebase has replayed so far,
/// along with any conflict resolution done in the working tree since it stopped,
/// and puts HEAD, the index and the working tree back exactly where the rebase
/// began. Edits made to files during an `edit` step and never committed are gone
/// and are in no reflog.
pub fn rebase_abort(repo: &Path) -> Result<()> {
    run_with(repo, ["rebase", "--abort"], &[], None)?;
    Ok(())
}

/// The half-finished rebase, if there is one.
///
/// Reads the sequencer's own state directory, because there is no porcelain that
/// reports it: `rebase-merge` for the interactive backend (everything this
/// module starts), `rebase-apply` for the patch backend a `git rebase` in a
/// terminal might have left.
pub fn rebase_state(repo: &Path) -> Result<Option<Rebase>> {
    let dir = git_dir(repo)?;
    let merge = dir.join("rebase-merge");
    let apply = dir.join("rebase-apply");
    let state = if merge.is_dir() {
        merge
    } else if apply.is_dir() {
        apply
    } else {
        return Ok(None);
    };

    let head_name = read(&state, "head-name");
    let todo = read(&state, "git-rebase-todo")
        .and_then(|text| parse_todo(&text).ok())
        .unwrap_or_default();
    // The merge backend tracks progress as two files of steps; the patch backend
    // as two counters. Counting lines covers both without asking which we are in.
    let done = read(&state, "done")
        .map(|text| steps(&text))
        .or_else(|| number(&state, "next"))
        .unwrap_or(0);
    let total = if todo.is_empty() {
        number(&state, "last").unwrap_or(done)
    } else {
        done + todo.len()
    };

    Ok(Some(Rebase {
        branch: head_name.and_then(|name| {
            name.strip_prefix("refs/heads/")
                .map(|short| short.to_string())
        }),
        onto: read(&state, "onto")
            .map(|sha| sha.chars().take(8).collect())
            .unwrap_or_default(),
        done,
        total,
        stopped: read(&state, "stopped-sha").and_then(|sha| crate::commit_of(repo, &sha).ok()),
        conflicts: conflicts(repo).unwrap_or_default(),
        todo,
    }))
}

/// Whether git stopped or finished, told from the repository rather than from an
/// exit code.
///
/// git exits non-zero both for "conflict, over to you" and for "I could not do
/// that at all", and the difference is whether a rebase is still sitting there.
/// So: state first, exit code only as the tiebreak when there is no rebase left.
fn outcome(repo: &Path, out: Result<Output>) -> Result<RebaseOutcome> {
    let message = match &out {
        Ok(out) => crate::last_line(out).unwrap_or_default(),
        Err(err) => err.to_string().lines().next().unwrap_or("").to_string(),
    };
    match rebase_state(repo).ok().flatten() {
        Some(rebase) => Ok(RebaseOutcome::Stopped { rebase, message }),
        None => {
            out?;
            Ok(RebaseOutcome::Done(message))
        }
    }
}

/// Paths the index holds more than one stage for.
fn conflicts(repo: &Path) -> Result<Vec<PathBuf>> {
    let out = crate::git(
        repo,
        ["diff", "--name-only", "--diff-filter=U", "--no-color", "-z"],
    )?;
    Ok(out
        .split(|&b| b == 0)
        .filter(|f| !f.is_empty())
        .map(crate::to_path)
        .collect())
}

fn read(dir: &Path, name: &str) -> Option<String> {
    let text = fs::read_to_string(dir.join(name)).ok()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn number(dir: &Path, name: &str) -> Option<usize> {
    read(dir, name)?.parse().ok()
}

fn steps(text: &str) -> usize {
    text.lines()
        .filter(|l| {
            let l = l.trim();
            !l.is_empty() && !l.starts_with('#')
        })
        .count()
}

// ----------------------------------------------------- one-commit surgery

/// **Rewrites history.** Replaces the last commit with a new one holding
/// whatever is in the index right now, under `message` or the old message when
/// that is `None`. The commit that was there is unreachable afterwards and only
/// the reflog remembers it. Never do this to a commit that has been pushed.
///
/// Note that this folds the index in, which is the point of Magit's amend: to
/// throw staged changes into the previous commit. Use [`reword`] to change only
/// the message.
pub fn amend(repo: &Path, message: Option<&str>) -> Result<String> {
    let mut args = vec!["commit".to_string(), "--amend".into()];
    match message {
        // `--message=` rather than `-m <msg>` so a message starting with a dash
        // is a message.
        Some(message) if !message.trim().is_empty() => args.push(format!("--message={message}")),
        Some(_) => bail!("empty commit message"),
        None => args.push("--no-edit".into()),
    }
    let out = run_with(repo, args, &[], None)?;
    Ok(crate::first_line(&out).unwrap_or_else(|| "amended".into()))
}

/// **Rewrites history.** Gives `commit` a new message. Its tree, and every
/// commit after it, is replayed unchanged, but they all get new hashes, so
/// anyone who has pulled them will have to deal with that.
///
/// Works by stopping the rebase at `commit` with an `edit` step and amending
/// there, because git's own `reword` would open an editor. Replaying `commit`
/// onto its own parent cannot conflict, so the message is always applied before
/// anything can go wrong; conflicts, if any, happen in the commits after it and
/// come back as [`RebaseOutcome::Stopped`] with the reword already done.
pub fn reword(repo: &Path, commit: &str, message: &str) -> Result<RebaseOutcome> {
    if message.trim().is_empty() {
        bail!("empty commit message");
    }
    if rev_parse(repo, commit)? == rev_parse(repo, "HEAD")? {
        return Ok(RebaseOutcome::Done(reword_head(repo, message)?));
    }
    let mut plan = plan_from(repo, commit)?;
    plan.todo[0].action = Action::Edit;
    match rebase_start(repo, &plan)? {
        RebaseOutcome::Stopped { .. } => {
            reword_head(repo, message)?;
            rebase_continue(repo)
        }
        // An `edit` first step always stops; if it did not, the rebase did
        // something else entirely and amending now would hit the wrong commit.
        RebaseOutcome::Done(message) => bail!("rebase did not stop to reword: {message}"),
    }
}

/// `--only` with no paths is what makes this a message change and not a commit:
/// the tree is taken from HEAD rather than from the index, so a user with
/// something staged does not find it folded into an old commit.
fn reword_head(repo: &Path, message: &str) -> Result<String> {
    let out = run_with(
        repo,
        [
            "commit".to_string(),
            "--amend".into(),
            "--only".into(),
            format!("--message={message}"),
        ],
        &[],
        None,
    )?;
    Ok(crate::first_line(&out).unwrap_or_else(|| "reworded".into()))
}

/// **Rewrites history.** Folds `commit` into the one before it, leaving one
/// commit where there were two. Both messages are kept, joined the way
/// `git rebase`'s `squash` joins them, so nothing written is lost — but the two
/// original commits are, and everything after them is replayed under new hashes.
pub fn squash(repo: &Path, commit: &str) -> Result<RebaseOutcome> {
    let commit = rev_parse(repo, commit)?;
    let parent = rev_parse(repo, &format!("{commit}^"))
        .map_err(|_| anyhow::anyhow!("{commit} is the first commit, nothing to squash into"))?;
    // Planned from the parent, so the parent is step 0 and `commit` is step 1 —
    // which is where the squash has to go.
    let mut plan = plan_from(repo, &parent)?;
    match plan.todo.get(1) {
        Some(second) if commit.starts_with(&second.hash) => {}
        _ => bail!("{commit} does not follow {parent} on the current branch"),
    }
    plan.todo[1].action = Action::Squash;
    rebase_start(repo, &plan)
}

/// **Destroys work.** Removes `commit` from the branch: its changes are undone
/// and its message is gone. Everything after it is replayed without it, which
/// is where conflicts come from — a later commit that touched the same lines
/// stops the rebase. The commit itself survives only in the reflog.
pub fn drop_commit(repo: &Path, commit: &str) -> Result<RebaseOutcome> {
    let mut plan = plan_from(repo, commit)?;
    if plan.todo.len() == 1 {
        // Dropping the only step leaves git with nothing to do; `reset` is the
        // command for unmaking the tip, and it is the caller's to choose.
        bail!("dropping the only commit in the range; reset instead");
    }
    plan.todo[0].action = Action::Drop;
    rebase_start(repo, &plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The round trip is the whole contract: a todo list read out of a rebase,
    /// reordered, and written back has to say the same things.
    #[test]
    fn a_todo_list_survives_parse_and_serialise_unchanged() {
        let text = "pick abc1234 first thing\n\
reword def5678 second thing\n\
squash 0011223 third\n\
fixup 4455667 fourth\n\
drop 8899aab fifth\n\
edit ccddeef sixth\n\
exec cargo test -p zemacs-git\n\
break\n";
        let items = parse_todo(text).unwrap();
        assert_eq!(items.len(), 8);
        assert_eq!(write_todo(&items), text);
        assert_eq!(parse_todo(&write_todo(&items)).unwrap(), items);

        // exec keeps its whole command line, hash-less.
        let exec = &items[6];
        assert_eq!(exec.action, Action::Exec);
        assert!(exec.hash.is_empty());
        assert_eq!(exec.subject, "cargo test -p zemacs-git");
    }

    #[test]
    fn comments_and_abbreviations_are_read_the_way_git_writes_them() {
        let items = parse_todo(
            "# This is a combination\n\
\n\
p abc1234 first\n\
   f def5678 second\n\
# Rebase aaa..bbb onto aaa (2 commands)\n",
        )
        .unwrap();
        assert_eq!(
            items,
            vec![
                TodoItem::pick("abc1234", "first"),
                TodoItem {
                    action: Action::Fixup,
                    hash: "def5678".into(),
                    subject: "second".into()
                },
            ]
        );
        // Expanded on the way out, so the file is unambiguous next time.
        assert_eq!(
            write_todo(&items),
            "pick abc1234 first\nfixup def5678 second\n"
        );
    }

    /// Skipping a line we do not understand would silently drop a commit.
    #[test]
    fn an_unknown_step_is_an_error_and_not_a_dropped_commit() {
        let err = parse_todo("pick abc1234 fine\nlabel onto\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("label onto"), "{err}");
    }

    /// The string git turns into a shell command has to be a constant: anything
    /// interpolated into it would be running as a command on someone's machine.
    #[test]
    fn the_sequence_editor_interpolates_nothing() {
        assert_eq!(SEQUENCE_EDITOR, "cp -- \"$ZEMACS_REBASE_TODO\"");
    }
}
