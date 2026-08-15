//! The Magit half that needs the filesystem.
//!
//! `zemacs-core` knows the verbs and the buffer kinds and nothing about git;
//! `zemacs-git` knows git and nothing about buffers. This is the seam: it runs
//! the verb, then hands the rendered status back to the editor as buffer text,
//! the same shape as opening a file.
//!
//! The one piece of state worth keeping is the line map. `render` returns the
//! status text *and* one `Line` per line of it, so "stage the thing under the
//! cursor" is a lookup by cursor row rather than re-parsing what we drew.

use std::path::{Path, PathBuf};

use zemacs_core::{BufferKind, Editor, EditorCommand, HlKind, Mode};
use zemacs_git as git;

/// Comment prefix in the commit message buffer, as in `COMMIT_EDITMSG`.
const COMMENT: &str = "#";

/// How far back `l` opens the log up to. Magit's `l l` is a buffer of its own
/// and unbounded; this is the same gesture in the section that is already
/// there, so it is bounded by what is reasonable to draw at once.
const LOG_MORE: usize = 100;

/// The question to ask before `verb`, or `None` if it cannot lose work.
///
/// The list is `zemacs-git`'s own — the module doc there names every function
/// that can lose work, and this is that list read back as verbs.
///
/// Deliberately *not* everything git can undo. `stash` is recoverable with `z
/// p`, `amend` and `unstage-all` leave the work in the tree or the reflog, and
/// `push` is checked by the remote — asking about those is how a confirmation
/// prompt becomes something you dismiss without reading, which would cost the
/// one below its whole value.
fn confirm_question(verb: &str) -> Option<&'static str> {
    match verb {
        "rebase-abort" => Some("Abort the rebase and throw away its work?"),
        "abort" => Some("Abort what is in progress and throw away its work?"),
        "reset-hard" => Some("Reset --hard, discarding all uncommitted changes?"),
        "discard" => Some("Discard the changes to this file?"),
        "discard-untracked" => Some("Delete this file? It is untracked, so there is no copy."),
        "discard-hunk" => Some("Discard this hunk?"),
        // `k` over a region throws away less than the hunk, and has to say so:
        // a question about the hunk answered `yes` would be agreement to a
        // bigger loss than the one about to happen.
        "discard-lines" => Some("Discard these lines?"),
        "stash-drop" => Some("Drop this stash for good?"),
        "branch-delete-force" => Some("Delete this branch even if unmerged?"),
        "drop-commit" => Some("Drop this commit?"),
        "resolve-ours" | "resolve-theirs" => {
            Some("Take that side whole, discarding every edit to this file?")
        }
        "push-force" => Some("Force-push, overwriting the branch on the remote?"),
        _ => None,
    }
}

/// What the commit message buffer on screen is going to do when `C-c` finishes
/// it. Kept here rather than read off the buffer because the buffer holds a
/// message and nothing else: `c c`, `c a` and `c w` all open the same one.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum Writing {
    #[default]
    New,
    /// Fold the index into the last commit, under a new message.
    Amend,
    /// Give one commit a different message and leave its tree alone.
    Reword(String),
}

#[derive(Default)]
pub struct Magit {
    /// Repository the status buffer is showing.
    repo: Option<PathBuf>,
    /// One entry per line of the status buffer, parallel to its text.
    lines: Vec<git::Line>,
    /// The status buffer's own state — which sections are folded and which
    /// files have their diff open. Held across refreshes, because a refresh
    /// that silently closed everything you had opened would be useless.
    view: Option<git::View>,
    /// What the message being written is for. Reset only on success, so a
    /// commit hook that rejected an amend is retried as an amend.
    writing: Writing,
    /// `RET` on a file. The app opens buffers; this layer asks, exactly as
    /// dired does.
    pub open_file: Option<PathBuf>,
}

impl Magit {
    /// Run a `magit-*` verb, asking first if it can lose work.
    ///
    /// The guard is here rather than in the arms of `try_run` because this is
    /// the funnel: a keybinding, `M-x`, and `(git "...")` from Lisp all arrive
    /// as one `EditorCommand::Git`, so one check in front of them covers all
    /// three and cannot be forgotten by whoever adds the fourth.
    ///
    /// A verb may carry one argument after a space — `checkout main` — which is
    /// how a question whose answer is *data* stays in Lisp, where `read-string`
    /// is: the whole string is parked for the confirmation, so a guarded verb
    /// keeps its argument across the `yes`.
    pub fn run(&mut self, editor: &mut Editor, verb: &str) {
        // `k` on a hunk is a different question from `k` on a file, and the
        // question has to be the right one *before* the guard is spent — so the
        // line under the cursor picks which verb is being asked about, the same
        // way `try_run`'s arm picks what to discard.
        let asked = match (split(verb).0, self.line_at_cursor(editor)) {
            // ...and `k` over a region, or on one `+`/`-` line of a hunk's
            // body, is a fourth question again: it throws away those lines and
            // leaves the rest of the hunk alone. `picked` is asked here rather
            // than the line map alone because a selection can start on the `@@`
            // header, where the map says "hunk" and the answer is still
            // "some of it".
            ("discard", Some(git::Line::Hunk { .. })) => match self.picked(editor) {
                Ok(Some(_)) => "discard-lines",
                _ => "discard-hunk",
            },
            // ...and `k` on an untracked file is a third question again. It
            // *deletes* the file rather than throwing away edits to one, and
            // there is no copy of it anywhere — not in the index, not in a
            // commit, not in a stash. Asking "discard the changes?" over that
            // is the one wording that could lose work nobody agreed to lose.
            (
                "discard",
                Some(git::Line::File {
                    section: git::Section::Untracked,
                    ..
                }),
            ) => "discard-untracked",
            (verb, _) => verb,
        };
        if let Some(question) = confirm_question(asked) {
            editor.confirm(
                question,
                EditorCommand::Confirmed(Box::new(EditorCommand::Git(verb.to_string()))),
            );
            return;
        }
        self.run_confirmed(editor, verb)
    }

    /// The same verb with the guard already spent — reached only by answering
    /// `yes`, or by being a verb that never needed asking about.
    pub fn run_confirmed(&mut self, editor: &mut Editor, verb: &str) {
        if let Err(e) = self.try_run(editor, verb) {
            // `{e:#}` so anyhow's context chain (which carries git's stderr)
            // is shown rather than just the outermost message.
            editor.apply(EditorCommand::Message(format!("git: {e:#}")));
        }
    }

    fn try_run(&mut self, editor: &mut Editor, verb: &str) -> anyhow::Result<()> {
        let (verb, arg) = split(verb);
        match verb {
            "status" | "refresh" => {
                self.repo = Some(self.locate(editor)?);
                self.refresh(editor)
            }
            // Fold a section, or open a file's diff under it. The one key
            // that makes the buffer navigable rather than a list.
            "toggle" => {
                let repo = self.repo()?.to_path_buf();
                let line = self.line_at_cursor(editor).cloned();
                let view = self.view_mut()?;
                match line {
                    Some(git::Line::Section(section)) => view.toggle_section(section),
                    Some(git::Line::File { path, section })
                    | Some(git::Line::Hunk { path, section, .. }) => {
                        view.toggle_file(&repo, section, &path)?
                    }
                    Some(git::Line::Commit { hash }) => view.toggle_commit(&repo, &hash)?,
                    _ => {}
                }
                self.render(editor)
            }
            // `RET`: the file under the cursor, in a buffer you can edit. On
            // anything else it is `TAB`, which is what there is to do there.
            "visit" => match self.line(editor, verb)? {
                git::Line::File { path, .. } | git::Line::Hunk { path, .. } => {
                    self.open_file = Some(self.repo()?.join(path));
                    Ok(())
                }
                _ => self.try_run(editor, "toggle"),
            },
            // Staging is line-sensitive, at three resolutions: the region or the
            // one changed line under the cursor, else the hunk, else the file.
            "stage" | "unstage" => {
                let repo = self.repo()?.to_path_buf();
                if let Some(p) = self.picked(editor)? {
                    let view = self.view_mut()?;
                    let diff = view
                        .diff_of(p.section, &p.path)
                        .ok_or_else(|| anyhow::anyhow!("no diff open for {}", p.path.display()))?;
                    if verb == "stage" {
                        git::stage_lines(&repo, diff, p.index, &p.lines)?
                    } else {
                        git::unstage_lines(&repo, diff, p.index, &p.lines)?
                    }
                    return self.after_region(editor);
                }
                let line = self
                    .line_at_cursor(editor)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("nothing to {verb} on this line"))?;
                match line {
                    git::Line::Hunk {
                        path,
                        section,
                        index,
                        ..
                    } => {
                        let view = self.view_mut()?;
                        let diff = view
                            .diff_of(section, &path)
                            .ok_or_else(|| anyhow::anyhow!("no diff open for {}", path.display()))?;
                        if verb == "stage" {
                            git::stage_hunk(&repo, diff, index)?
                        } else {
                            git::unstage_hunk(&repo, diff, index)?
                        }
                    }
                    git::Line::File { path, .. } => {
                        if verb == "stage" {
                            git::stage(&repo, &path)?
                        } else {
                            git::unstage(&repo, &path)?
                        }
                    }
                    _ => anyhow::bail!("nothing to {verb} on this line"),
                }
                self.refresh(editor)
            }
            // `k`, and the reason every arm of it went through `confirm_question`
            // first. Which of the three destructive functions runs is the line's
            // to say, exactly as `stage`'s is.
            "discard" | "discard-hunk" | "discard-lines" => {
                let repo = self.repo()?.to_path_buf();
                let picked = self.picked(editor)?;
                // The question named the lines, so the lines are all this is
                // allowed to throw away: falling back to the whole hunk here
                // would destroy more than the `yes` agreed to.
                if verb == "discard-lines" && picked.is_none() {
                    anyhow::bail!("nothing selected to discard");
                }
                if let Some(p) = picked {
                    let view = self.view_mut()?;
                    let diff = view
                        .diff_of(p.section, &p.path)
                        .ok_or_else(|| anyhow::anyhow!("no diff open for {}", p.path.display()))?;
                    git::discard_lines(&repo, diff, p.index, &p.lines)?;
                    return self.after_region(editor);
                }
                match self.line(editor, verb)? {
                    git::Line::Hunk {
                        path,
                        section,
                        index,
                        ..
                    } => {
                        let view = self.view_mut()?;
                        let diff = view.diff_of(section, &path).ok_or_else(|| {
                            anyhow::anyhow!("no diff open for {}", path.display())
                        })?;
                        git::discard_hunk(&repo, diff, index)?
                    }
                    git::Line::File { path, section } => match section {
                        git::Section::Untracked => git::discard_untracked(&repo, &path)?,
                        // ponytail: a staged change has to be unstaged before it
                        // can be thrown away, where Magit's `k` does both at
                        // once. Doing both means reverse-applying the staged
                        // diff to the tree *and* resetting the index, and
                        // failing halfway through that leaves a mess this
                        // refuses to be able to make.
                        git::Section::Staged => {
                            anyhow::bail!("unstage it first, then discard")
                        }
                        _ => git::discard(&repo, &path)?,
                    },
                    _ => anyhow::bail!("nothing to discard on this line"),
                }
                self.refresh(editor)
            }
            // Conflicts. Staging a conflicted file is how you say "I have fixed
            // it by hand"; these two are how you say "take that side whole".
            "resolve-ours" | "resolve-theirs" => {
                let repo = self.repo()?.to_path_buf();
                let (path, _) = self.file(editor, verb)?;
                git::resolve(&repo, &path, verb == "resolve-ours")?;
                self.refresh(editor)
            }
            "amend" => {
                let repo = self.repo()?.to_path_buf();
                let out = git::amend(&repo, None)?;
                self.refresh(editor)?;
                editor.apply(EditorCommand::Message(summarize(&out, "amend")));
                Ok(())
            }
            "fetch" => {
                let repo = self.repo()?.to_path_buf();
                let out = git::fetch(&repo)?;
                self.refresh(editor)?;
                editor.apply(EditorCommand::Message(summarize(&out, "fetch")));
                Ok(())
            }
            // The stack, and the entry under the cursor when there is one — a
            // stash line is what `z a` and `z k` are usually pressed on, and
            // `stash@{0}` is what they mean anywhere else.
            "stash" | "stash-pop" | "stash-apply" | "stash-drop" => {
                let repo = self.repo()?.to_path_buf();
                let name = self.stash_at_cursor(editor);
                let out = match verb {
                    "stash" => git::stash_push(&repo, arg)?,
                    "stash-pop" => git::stash_pop(&repo, &name)?,
                    "stash-apply" => git::stash_apply(&repo, &name)?,
                    _ => git::stash_drop(&repo, &name)?,
                };
                self.refresh(editor)?;
                editor.apply(EditorCommand::Message(summarize(&out, verb)));
                Ok(())
            }
            // Branches. The name is always the argument, because it is a
            // question whose answer is data and those are asked in Lisp.
            "checkout" | "checkout-new" | "branch-create" | "branch-delete"
            | "branch-delete-force" => {
                let repo = self.repo()?.to_path_buf();
                let name = self.revision(editor, arg)?;
                let out = match verb {
                    "checkout" => git::checkout(&repo, &name)?,
                    "checkout-new" => git::checkout_new(&repo, &name, None)?,
                    "branch-create" => {
                        git::branch_create(&repo, &name, None)?;
                        format!("created {name}")
                    }
                    "branch-delete" => git::branch_delete(&repo, &name)?,
                    _ => git::branch_delete_force(&repo, &name)?,
                };
                self.refresh(editor)?;
                editor.apply(EditorCommand::Message(summarize(&out, verb)));
                Ok(())
            }
            // Reset. The revision defaults to the commit under the cursor, so
            // `X h` in the log is "put the branch back to here".
            "reset-soft" | "reset-mixed" | "reset-hard" => {
                let repo = self.repo()?.to_path_buf();
                let target = self.revision(editor, arg)?;
                match verb {
                    "reset-soft" => git::reset_soft(&repo, &target)?,
                    "reset-mixed" => git::reset_mixed(&repo, &target)?,
                    _ => git::reset_hard(&repo, &target)?,
                }
                self.refresh(editor)?;
                editor.apply(EditorCommand::Message(format!("{verb} {target}")));
                Ok(())
            }
            // Everything that replays a commit somewhere else. All three can
            // stop on a conflict, which is a state to look at rather than a
            // failure — hence `attempt`, which redraws either way.
            "cherry-pick" | "revert" | "merge" => {
                let repo = self.repo()?.to_path_buf();
                let target = self.revision(editor, arg)?;
                let out = match verb {
                    "cherry-pick" => git::cherry_pick(&repo, &target),
                    "revert" => git::revert(&repo, &target),
                    _ => git::merge(&repo, &target),
                };
                self.attempt(editor, verb, out)
            }
            // The other half of the four operations above, and of a rebase:
            // git spells `--continue` and `--abort` the same way for all of
            // them, and the repository already says which one is running.
            "continue" | "abort" => {
                let repo = self.repo()?.to_path_buf();
                let out = if verb == "continue" {
                    git::sequence_continue(&repo)
                } else {
                    git::sequence_abort(&repo)
                };
                self.attempt(editor, verb, out)
            }
            // Replay this branch onto another one. The todo list is worked out
            // here and handed to git whole, so no sequence editor is spawned —
            // see `zemacs_git::rebase`.
            "rebase" => {
                let repo = self.repo()?.to_path_buf();
                let base = self.revision(editor, arg)?;
                let plan = git::plan_onto(&repo, &base)?;
                let outcome = git::rebase_start(&repo, &plan)?;
                self.refresh(editor)?;
                editor.apply(EditorCommand::Message(report(outcome, "rebase")));
                Ok(())
            }
            // History surgery on the commit under the cursor. Each is a rebase
            // underneath and so can stop on a conflict.
            "squash" | "drop-commit" => {
                let repo = self.repo()?.to_path_buf();
                let target = self.revision(editor, arg)?;
                let outcome = if verb == "squash" {
                    git::squash(&repo, &target)?
                } else {
                    git::drop_commit(&repo, &target)?
                };
                self.refresh(editor)?;
                editor.apply(EditorCommand::Message(report(outcome, verb)));
                Ok(())
            }
            // A rebase in flight. `continue` and `skip` can stop again on the
            // next conflict, which is ordinary progress, not failure — so both
            // outcomes are reported the same way and the buffer redraws either
            // way.
            "rebase-continue" | "rebase-skip" => {
                let repo = self.repo()?.to_path_buf();
                let outcome = if verb == "rebase-continue" {
                    git::rebase_continue(&repo)?
                } else {
                    git::rebase_skip(&repo)?
                };
                self.refresh(editor)?;
                editor.apply(EditorCommand::Message(report(outcome, "rebase")));
                Ok(())
            }
            // Throws away everything the rebase has done and puts the branch
            // back. Destructive, hence its own verb and its own key.
            "rebase-abort" => {
                let repo = self.repo()?.to_path_buf();
                git::rebase_abort(&repo)?;
                self.refresh(editor)?;
                editor.apply(EditorCommand::Message("rebase aborted".into()));
                Ok(())
            }
            "stage-all" => {
                git::stage_all(self.repo()?)?;
                self.refresh(editor)
            }
            "unstage-all" => {
                git::unstage_all(self.repo()?)?;
                self.refresh(editor)
            }
            "push" | "pull" | "push-upstream" | "push-force" => {
                let repo = self.repo()?.to_path_buf();
                let out = match verb {
                    "push" => git::push(&repo)?,
                    "pull" => git::pull(&repo)?,
                    // ponytail: `origin` unless the caller names a remote. The
                    // upgrade is a `remotes` reader and a picker in front of it;
                    // a repository with one remote — which is nearly all of them
                    // — never notices.
                    "push-upstream" => {
                        git::push_upstream(&repo, if arg.is_empty() { "origin" } else { arg })?
                    }
                    _ => git::push_force(&repo)?,
                };
                self.refresh(editor)?;
                editor.apply(EditorCommand::Message(summarize(&out, verb)));
                Ok(())
            }
            // `l`: more log, in the buffer that is already showing some. A
            // separate log buffer would need its own line map and its own copy
            // of every verb that acts on a commit; this way `TAB`, `c w`, `x`
            // and `A` already work on every line of it.
            "log" => {
                let repo = self.repo()?.to_path_buf();
                let view = self.view_mut()?;
                let shown = view.log_limit;
                view.log_limit = match arg.parse::<usize>() {
                    Ok(n) if n > 0 => n,
                    // No argument toggles: the long log, or back to the short
                    // one it started as.
                    _ if shown == 0 => LOG_MORE,
                    _ => 0,
                };
                view.refresh(&repo)?;
                self.render(editor)
            }
            // The three ways into the message buffer. They differ only in what
            // `commit-finish` does with what you write, which is what `writing`
            // remembers.
            "commit" | "commit-amend" | "reword" => {
                let repo = self.repo()?.to_path_buf();
                let status = git::status(&repo)?;
                let (writing, old) = match verb {
                    "commit" if status.staged.is_empty() => {
                        anyhow::bail!("nothing staged to commit")
                    }
                    "commit" => (Writing::New, String::new()),
                    "commit-amend" => (Writing::Amend, git::message_of(&repo, "HEAD")?),
                    _ => {
                        let target = self.revision(editor, arg)?;
                        let old = git::message_of(&repo, &target)?;
                        (Writing::Reword(target), old)
                    }
                };
                editor.show_special(
                    BufferKind::CommitMessage,
                    &commit_template(&status, &writing, &old),
                );
                self.writing = writing;
                editor.apply(EditorCommand::Message(
                    "write a message, then C-c to commit".into(),
                ));
                Ok(())
            }
            "commit-finish" => {
                let repo = self.repo()?.to_path_buf();
                let message = strip_comments(&editor.buffer.text.to_string());
                if message.is_empty() {
                    anyhow::bail!("aborting commit due to empty message");
                }
                // Cloned rather than taken: a hook that rejected the commit
                // leaves the buffer up, and the retry has to be the same verb.
                let out = match self.writing.clone() {
                    Writing::New => git::commit(&repo, &message)?,
                    Writing::Amend => git::amend(&repo, Some(&message))?,
                    Writing::Reword(target) => {
                        report(git::reword(&repo, &target, &message)?, "reword")
                    }
                };
                self.writing = Writing::New;
                self.refresh(editor)?;
                editor.apply(EditorCommand::Message(summarize(&out, "commit")));
                Ok(())
            }
            other => anyhow::bail!("unknown git verb: {other}"),
        }
    }

    /// Run something that can stop half-way — a merge, a cherry-pick, a revert,
    /// a `--continue` — and redraw either way.
    ///
    /// A conflict comes back from git as a failure, and it is not one: it is a
    /// state, and the status buffer is the only place to see it. Letting the
    /// error out through `run_confirmed` would report it and leave the buffer
    /// showing the repository as it was before.
    fn attempt(
        &mut self,
        editor: &mut Editor,
        verb: &str,
        out: anyhow::Result<String>,
    ) -> anyhow::Result<()> {
        self.refresh(editor)?;
        editor.apply(EditorCommand::Message(match out {
            Ok(message) => summarize(&message, verb),
            Err(e) => format!("{verb}: {e:#}"),
        }));
        Ok(())
    }

    /// The `+`/`-` lines the user has picked out of one hunk: the Visual
    /// selection where there is one, else the single line under the cursor.
    ///
    /// `Ok(None)` means "nothing partial here" — the cursor is on a file row, on
    /// a `@@` header, or on nothing but context — and every caller falls back to
    /// the hunk for it, which is what keeps `s` on a hunk heading meaning what
    /// it has always meant.
    ///
    /// A selection reaching across two hunks is an error rather than a guess:
    /// each hunk is its own patch with its own `@@` counts, and picking one of
    /// the two would stage half of what the region covers without saying so.
    fn picked(&self, editor: &Editor) -> anyhow::Result<Option<Picked>> {
        // Same guard as `line_at_cursor`, and for the same reason: the map
        // describes the status buffer and a row read out of any other one names
        // whatever happened to be drawn there last.
        if editor.buffer.kind != BufferKind::Magit {
            return Ok(None);
        }
        let (first, last) = match editor.selection() {
            // The end is exclusive and a selection is never empty, so the last
            // row is the one the character before it sits on.
            Some((start, end)) => (
                row_of(editor, start),
                row_of(editor, end.saturating_sub(1).max(start)),
            ),
            None => {
                let (row, _) = editor.buffer.cursor_line_col();
                (row, row)
            }
        };

        let mut at: Option<(PathBuf, git::Section, usize)> = None;
        let mut lines = Vec::new();
        for row in first..=last {
            let Some(git::Line::Hunk {
                path,
                section,
                index,
                line,
            }) = self.lines.get(row)
            else {
                continue;
            };
            // Line 0 is the `@@` header. A region dragged from it still means
            // the body underneath, so it is passed over rather than refused.
            if *line == 0 {
                continue;
            }
            let here = (path.clone(), *section, *index);
            match &at {
                Some(open) if *open != here => {
                    anyhow::bail!("that selection covers more than one hunk")
                }
                Some(_) => {}
                None => at = Some(here),
            }
            // Only a change can be staged; a context line swept up by the
            // region is along for the ride and is not one.
            if matches!(self.mark(section, path, *index, *line), Some('+' | '-')) {
                lines.push(*line);
            }
        }
        let Some((path, section, index)) = at else {
            return Ok(None);
        };
        if lines.is_empty() {
            return Ok(None);
        }
        Ok(Some(Picked {
            path,
            section,
            index,
            lines,
        }))
    }

    /// The first character of one line of an open hunk's body — `+`, `-`, ` ` or
    /// `\`. Read from the diff rather than off the screen, so it is the same
    /// bytes the patch rewriter will see.
    fn mark(
        &self,
        section: &git::Section,
        path: &Path,
        index: usize,
        line: usize,
    ) -> Option<char> {
        let hunk = self.view.as_ref()?.diff_of(*section, path)?.hunks.get(index)?;
        hunk.body.split_inclusive('\n').nth(line)?.chars().next()
    }

    /// Redraw after acting on a region, and drop the region on the way out.
    ///
    /// Magit ends the selection for the same reason: the buffer under it has
    /// just been re-rendered from a repository that changed, so those rows now
    /// hold different lines — and a second `s` on a stale selection would stage
    /// something nobody looked at.
    fn after_region(&mut self, editor: &mut Editor) -> anyhow::Result<()> {
        if editor.mode.is_visual() {
            editor.apply(EditorCommand::SetMode(Mode::Normal));
        }
        self.refresh(editor)
    }

    /// What the cursor is on, or an error naming the verb that wanted it.
    fn line(&self, editor: &Editor, verb: &str) -> anyhow::Result<git::Line> {
        self.line_at_cursor(editor)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("nothing to {verb} on this line"))
    }

    fn file(&self, editor: &Editor, verb: &str) -> anyhow::Result<(PathBuf, git::Section)> {
        match self.line(editor, verb)? {
            git::Line::File { path, section } | git::Line::Hunk { path, section, .. } => {
                Ok((path, section))
            }
            _ => anyhow::bail!("no file on this line"),
        }
    }

    /// The revision a verb is about: what Lisp asked for, else the commit under
    /// the cursor. The second half is what makes the log section useful —
    /// `X h`, `A` and `x` on a commit line mean *that* commit, with nothing to
    /// type.
    fn revision(&self, editor: &Editor, arg: &str) -> anyhow::Result<String> {
        if !arg.is_empty() {
            return Ok(arg.to_string());
        }
        match self.line_at_cursor(editor) {
            Some(git::Line::Commit { hash }) => Ok(hash.clone()),
            _ => anyhow::bail!("no commit on this line, and none was named"),
        }
    }

    /// The stash under the cursor, or the top of the stack.
    fn stash_at_cursor(&self, editor: &Editor) -> String {
        match self.line_at_cursor(editor) {
            Some(git::Line::Stash { name }) => name.clone(),
            _ => "stash@{0}".to_string(),
        }
    }

    fn view_mut(&mut self) -> anyhow::Result<&mut git::View> {
        self.view
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no status buffer — run magit-status first"))
    }

    /// Re-read the repository, then redraw.
    fn refresh(&mut self, editor: &mut Editor) -> anyhow::Result<()> {
        let repo = self.repo()?.to_path_buf();
        match self.view.as_mut() {
            Some(view) => view.refresh(&repo)?,
            None => self.view = Some(git::View::load(&repo)?),
        }
        self.render(editor)
    }

    /// Redraw from the view as it stands. Folding changed no git state, so
    /// re-reading the repository for it would be a needless `git status` on
    /// every `TAB`.
    fn render(&mut self, editor: &mut Editor) -> anyhow::Result<()> {
        let view = self
            .view
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no status buffer — run magit-status first"))?;
        let (text, lines, spans) = git::render(view);
        self.lines = lines;
        editor.show_special(BufferKind::Magit, &text);
        // A *minor mode* on top of the `"magit"` state keymap, and the two are
        // not the same reach. The state keymap layers over Normal only, so once
        // `V` puts the editor in Visual the whole of magit stops answering —
        // which is exactly when you are selecting the lines you mean to stage.
        // A minor-mode binding is consulted in every editing mode, so `s` and
        // `u` over a region work; see `runtime/modes/magit.lisp`, where the
        // short list of keys that belong in both is written down and argued for.
        //
        // Through `SetMinorMode` rather than pushing the name: `show_special`
        // does *not* empty the list, and a render happens on every `TAB` and
        // every refresh, so a bare push grows it without bound. The command
        // removes before it adds and is the only idempotent door.
        editor.apply(EditorCommand::SetMinorMode("magit-mode".into(), true));
        // Generated text has no language for the syntax thread, so the status
        // buffer carries its own spans — in the same faces everything else uses,
        // so a theme colours magit without knowing magit exists.
        editor.buffer.highlights = spans.into_iter().map(face_span).collect();
        Ok(())
    }

    fn repo(&self) -> anyhow::Result<&Path> {
        self.repo
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no repository — run magit-status first"))
    }

    /// The repository to show: the one holding the current file, else the one
    /// holding the working directory.
    fn locate(&self, editor: &Editor) -> anyhow::Result<PathBuf> {
        let from = editor
            .buffer
            .path
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        git::repo_root(&from).ok_or_else(|| anyhow::anyhow!("{} is not a git repository", from.display()))
    }

    /// What the cursor is on. `None` on a blank, which is why acting there is a
    /// message rather than a mistake.
    ///
    /// `None` too when the live buffer is not the status buffer at all: the map
    /// describes text that is on screen somewhere else, and a cursor row read
    /// out of a *source* file would name whatever happened to be on that row of
    /// the last status — which is how `M-x magit-cherry-pick` from a file
    /// would pick a commit nobody chose.
    fn line_at_cursor(&self, editor: &Editor) -> Option<&git::Line> {
        if editor.buffer.kind != BufferKind::Magit {
            return None;
        }
        let (line, _) = editor.buffer.cursor_line_col();
        self.lines.get(line)
    }
}

/// One hunk, and which of its `+`/`-` lines a gesture is about.
#[derive(Debug)]
struct Picked {
    path: PathBuf,
    section: git::Section,
    /// Index into the open [`git::FileDiff`]'s hunks.
    index: usize,
    /// Body lines, numbered as [`git::Line::Hunk`]'s `line` numbers them.
    lines: Vec<usize>,
}

/// The row a char offset sits on.
///
/// ponytail: a binary search over `line_start`, because core's own `line_of` is
/// crate-private and `crates/core/src/lib.rs` is not this change's to widen. The
/// upgrade is one `pub` over there; ten comparisons on a status buffer is not a
/// reason to ask for it.
fn row_of(editor: &Editor, at: usize) -> usize {
    let buf = &editor.buffer;
    let (mut low, mut high) = (0usize, buf.len_lines().saturating_sub(1));
    while low < high {
        let mid = low.midpoint(high + 1);
        if buf.line_start(mid) <= at {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

/// A verb and its argument: `"checkout main"` is `("checkout", "main")`.
///
/// One space, because every argument here is a single revision, path or branch
/// name and none of them can hold one. A verb with no argument is the whole
/// string and an empty second, which is what every existing verb is.
fn split(verb: &str) -> (&str, &str) {
    match verb.split_once(' ') {
        Some((verb, arg)) => (verb, arg.trim()),
        None => (verb, ""),
    }
}

/// A rebase's two outcomes, as one line for the echo area. Stopping is ordinary
/// progress — a conflict to fix, or an `edit` step waiting — and not a failure,
/// so both come out of here.
fn report(outcome: git::RebaseOutcome, verb: &str) -> String {
    match outcome {
        git::RebaseOutcome::Done(message) => summarize(&message, verb),
        git::RebaseOutcome::Stopped { rebase, .. } => {
            if rebase.is_conflicted() {
                format!("{verb} stopped: {} conflicted", rebase.conflicts.len())
            } else {
                format!("{verb} stopped at {}/{}", rebase.done, rebase.total)
            }
        }
    }
}

/// `git commit` prints several lines; the status line wants one.
fn summarize(out: &str, verb: &str) -> String {
    match out.lines().find(|l| !l.trim().is_empty()) {
        Some(first) => first.trim().to_string(),
        None => format!("{verb}: done"),
    }
}

/// The `COMMIT_EDITMSG` template: the message being edited — empty for a new
/// commit, the old one for an amend or a reword — then the state as comments.
///
/// A reword takes its tree from HEAD and never looks at the index, so the
/// staged files are left out of its template: listing them under "Changes to be
/// committed" would be a lie about what is going to happen.
fn commit_template(status: &git::Status, writing: &Writing, old: &str) -> String {
    let mut s = format!("{old}\n");
    s.push_str(&format!("{COMMENT} Please enter the commit message for your changes.\n"));
    s.push_str(&format!("{COMMENT} Lines starting with '{COMMENT}' are ignored.\n"));
    s.push_str(&format!("{COMMENT}\n"));
    match writing {
        Writing::New => {}
        Writing::Amend => s.push_str(&format!("{COMMENT} Amending the last commit.\n")),
        Writing::Reword(hash) => s.push_str(&format!("{COMMENT} Rewording {hash}.\n")),
    }
    if let Some(b) = &status.branch {
        s.push_str(&format!("{COMMENT} On branch {b}\n"));
    }
    if !matches!(writing, Writing::Reword(_)) {
        s.push_str(&format!("{COMMENT} Changes to be committed:\n"));
        for c in &status.staged {
            s.push_str(&format!(
                "{COMMENT}\t{}  {}\n",
                c.status.letter(),
                c.path.display()
            ));
        }
    }
    s
}

/// Drop comment lines and surrounding blank lines, as git does.
fn strip_comments(text: &str) -> String {
    let body: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with(COMMENT))
        .collect();
    body.join("\n").trim().to_string()
}

/// git names its faces, core owns the enum. Mechanical, and the only place the
/// two vocabularies meet.
fn face_span(span: git::Span) -> zemacs_core::Span {
    use git::Face;
    zemacs_core::Span {
        start: span.start,
        end: span.end,
        kind: match span.kind {
            Face::Heading1 => HlKind::Heading1,
            Face::String => HlKind::String,
            Face::Keyword => HlKind::Keyword,
            Face::Link => HlKind::Link,
            Face::Number => HlKind::Number,
            Face::Comment => HlKind::Comment,
            Face::Constant => HlKind::Constant,
            Face::Bold => HlKind::Bold,
            Face::Punctuation => HlKind::Punctuation,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_padding_are_stripped_from_a_message() {
        let msg = "\nAdd the thing\n\nWith a body.\n\n# On branch main\n#\tM  a.rs\n";
        assert_eq!(strip_comments(msg), "Add the thing\n\nWith a body.");
    }

    #[test]
    fn a_message_of_only_comments_is_empty() {
        assert_eq!(strip_comments("\n# nothing\n#\n"), "");
        assert_eq!(strip_comments(""), "");
    }

    #[test]
    fn a_hash_inside_a_line_is_not_a_comment() {
        // Only a *leading* hash comments a line out; `#42` in prose must survive.
        assert_eq!(strip_comments("fix #42 properly\n"), "fix #42 properly");
    }

    fn staged() -> git::Status {
        git::Status {
            branch: Some("main".into()),
            staged: vec![git::FileChange {
                path: "a.rs".into(),
                status: git::ChangeKind::Modified,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn the_template_lists_what_is_staged_and_starts_empty() {
        let t = commit_template(&staged(), &Writing::New, "");
        assert!(t.starts_with('\n'), "cursor lands on an empty first line");
        assert!(t.contains("On branch main"));
        assert!(t.contains("M  a.rs"));
        // and the template alone is not a message
        assert_eq!(strip_comments(&t), "");
    }

    /// `c a` and `c w` open the same buffer with the old message already in it,
    /// which is the whole difference between rewriting one and retyping it.
    #[test]
    fn amending_and_rewording_start_from_the_old_message() {
        let amend = commit_template(&staged(), &Writing::Amend, "the old subject");
        assert_eq!(strip_comments(&amend), "the old subject");
        assert!(amend.contains("Amending the last commit"), "{amend}");
        assert!(amend.contains("M  a.rs"), "an amend does fold the index in");

        let reword = commit_template(&staged(), &Writing::Reword("abc1234".into()), "old");
        assert!(reword.contains("Rewording abc1234"), "{reword}");
        // A reword takes its tree from HEAD, so saying what is staged would be
        // a lie about what is going to happen.
        assert!(!reword.contains("a.rs"), "{reword}");
    }

    /// The one line that lets a question whose answer is *data* stay in Lisp.
    #[test]
    fn a_verb_carries_one_argument_after_a_space() {
        assert_eq!(split("checkout"), ("checkout", ""));
        assert_eq!(split("checkout main"), ("checkout", "main"));
        // A stash message is the rest of the string, spaces and all.
        assert_eq!(split("stash work in progress"), ("stash", "work in progress"));
    }

    /// Stopping is progress, not failure, so both outcomes read as a state.
    #[test]
    fn a_stopped_rebase_reports_where_it_stopped() {
        assert_eq!(
            report(git::RebaseOutcome::Done(" done \n".into()), "rebase"),
            "done"
        );
        let stopped = |conflicts: Vec<PathBuf>| git::RebaseOutcome::Stopped {
            rebase: git::Rebase {
                done: 2,
                total: 5,
                conflicts,
                ..Default::default()
            },
            message: String::new(),
        };
        assert_eq!(report(stopped(vec![]), "rebase"), "rebase stopped at 2/5");
        assert_eq!(
            report(stopped(vec!["a.rs".into()]), "squash"),
            "squash stopped: 1 conflicted"
        );
    }

    /// A status buffer and an editor showing it, with the cursor on the first
    /// line the map answers `true` for.
    fn showing(view: git::View, on: impl Fn(&git::Line) -> bool) -> (Magit, Editor) {
        let (text, lines, _) = git::render(&view);
        let mut editor = Editor::new();
        editor.show_special(BufferKind::Magit, &text);
        let row = lines.iter().position(on).expect("no such line");
        editor.buffer.cursor = editor.buffer.line_start(row);
        let magit = Magit {
            repo: Some(PathBuf::from("/nowhere")),
            lines,
            view: Some(view),
            ..Magit::default()
        };
        (magit, editor)
    }

    /// The status buffer carries `magit-mode`, which is what lets `s` and `u`
    /// answer over a *selection*.
    ///
    /// The `"magit"` keymap is a state keymap and layers over Normal only, so
    /// pressing `V` to pick the lines to stage left magit unreachable — the one
    /// moment the region verbs exist for. `show_special` clears minor modes, so
    /// this has to be re-pushed on every render rather than set once.
    #[test]
    fn the_status_buffer_carries_the_minor_mode_a_selection_needs() {
        let (mut magit, mut editor) =
            showing(unstaged_with_a_hunk(), |l| matches!(l, git::Line::File { .. }));
        magit.render(&mut editor).unwrap();
        assert!(
            editor.buffer.minor_modes.iter().any(|m| m == "magit-mode"),
            "{:?}",
            editor.buffer.minor_modes
        );
        // Twice, because a refresh re-renders and `show_special` empties the
        // list each time — a mode pushed once would survive exactly one `TAB`.
        magit.render(&mut editor).unwrap();
        assert_eq!(
            editor.buffer.minor_modes.iter().filter(|m| *m == "magit-mode").count(),
            1,
            "re-pushed, and not stacked up: {:?}",
            editor.buffer.minor_modes
        );
    }

    fn unstaged_with_a_hunk() -> git::View {
        let status = git::Status {
            branch: Some("main".into()),
            unstaged: vec![git::FileChange {
                path: "a.rs".into(),
                status: git::ChangeKind::Modified,
            }],
            ..Default::default()
        };
        let mut view = git::View::of(status);
        view.diffs.push(git::FileDiff {
            path: "a.rs".into(),
            section: git::Section::Unstaged,
            preamble: "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n".into(),
            hunks: vec![git::Hunk {
                header: "@@ -1 +1 @@".into(),
                body: "@@ -1 +1 @@\n-one\n+ONE\n".into(),
            }],
        });
        view
    }

    /// A file with two hunks open under it, so a region can be dragged inside
    /// one of them and across both.
    fn unstaged_with_two_hunks() -> git::View {
        let mut view = unstaged_with_a_hunk();
        view.diffs[0].hunks = vec![
            git::Hunk {
                header: "@@ -1,3 +1,3 @@".into(),
                body: "@@ -1,3 +1,3 @@\n-one\n+ONE\n two\n".into(),
            },
            git::Hunk {
                header: "@@ -9,2 +9,2 @@".into(),
                body: "@@ -9,2 +9,2 @@\n-nine\n+NINE\n".into(),
            },
        ];
        view
    }

    /// The row of the status buffer whose text is exactly `text`.
    fn row(editor: &Editor, text: &str) -> usize {
        editor
            .buffer
            .text
            .to_string()
            .lines()
            .position(|line| line == text)
            .unwrap_or_else(|| panic!("no {text:?} on screen"))
    }

    /// Drag a Visual-line selection from one row to another, the way `V` and
    /// then `j` would: the anchor lands where the mode is entered.
    fn select(editor: &mut Editor, from: usize, to: usize) {
        // Out first: entering a visual mode from another one deliberately keeps
        // the old anchor, so `v` then `C-v` reshapes one selection.
        editor.apply(EditorCommand::SetMode(Mode::Normal));
        editor.buffer.cursor = editor.buffer.line_start(from);
        editor.apply(EditorCommand::SetMode(Mode::VisualLine));
        editor.buffer.cursor = editor.buffer.line_start(to);
    }

    /// The gesture Magit is used for. A region picks out exactly the `+`/`-`
    /// lines it covers; the `@@` header and the context lines it drags over are
    /// not changes and are passed through.
    #[test]
    fn a_region_picks_out_the_changed_lines_it_covers() {
        let (magit, mut editor) = showing(unstaged_with_two_hunks(), |l| {
            matches!(l, git::Line::File { .. })
        });

        // No region, on a `+` line: that one line, in the hunk it belongs to.
        editor.buffer.cursor = editor.buffer.line_start(row(&editor, "+ONE"));
        let p = magit.picked(&editor).unwrap().expect("a + line is a change");
        assert_eq!((p.index, p.lines), (0, vec![2]));
        assert_eq!(p.section, git::Section::Unstaged);
        assert_eq!(p.path, PathBuf::from("a.rs"));

        // No region, on the `@@` header: nothing partial here, so `s` still
        // means the whole hunk. That is the gesture this must not take away.
        editor.buffer.cursor = editor.buffer.line_start(row(&editor, "@@ -1,3 +1,3 @@"));
        assert!(magit.picked(&editor).unwrap().is_none());

        // ...and on a context line, for the same reason: it is not a change.
        editor.buffer.cursor = editor.buffer.line_start(row(&editor, " two"));
        assert!(magit.picked(&editor).unwrap().is_none());

        // A region from the header down over the whole hunk picks the two
        // changed lines and neither the header nor the context.
        let (top, bottom) = (row(&editor, "@@ -1,3 +1,3 @@"), row(&editor, " two"));
        select(&mut editor, top, bottom);
        let p = magit.picked(&editor).unwrap().unwrap();
        assert_eq!((p.index, p.lines), (0, vec![1, 2]));

        // Dragged the other way it is the same region, because the anchor is an
        // end and not a start.
        select(&mut editor, bottom, top);
        assert_eq!(magit.picked(&editor).unwrap().unwrap().lines, vec![1, 2]);

        // And the second hunk numbers its own body from its own `@@`.
        editor.apply(EditorCommand::SetMode(Mode::Normal));
        editor.buffer.cursor = editor.buffer.line_start(row(&editor, "-nine"));
        let p = magit.picked(&editor).unwrap().unwrap();
        assert_eq!((p.index, p.lines), (1, vec![1]));
    }

    /// Two hunks are two patches with two sets of `@@` counts. Staging one of
    /// them and calling that the region would move half of what the region
    /// covers and say nothing about the other half.
    #[test]
    fn a_region_across_two_hunks_is_refused_rather_than_halved() {
        let (magit, mut editor) = showing(unstaged_with_two_hunks(), |l| {
            matches!(l, git::Line::File { .. })
        });
        let (top, bottom) = (row(&editor, "+ONE"), row(&editor, "+NINE"));
        select(&mut editor, top, bottom);
        let err = magit.picked(&editor).unwrap_err();
        assert!(format!("{err:#}").contains("more than one hunk"), "{err:#}");

        // The map describes the status buffer and nothing else, so a selection
        // in a *source* file picks nothing rather than whatever happens to be
        // on those rows of the last status.
        editor.show_special(BufferKind::Scratch, "a source file\nwith lines\n");
        select(&mut editor, 0, 1);
        assert!(magit.picked(&editor).unwrap().is_none());
    }

    /// `k` over a region throws away less than the hunk, so it has to ask about
    /// less — and the region has to still be there when the answer comes back,
    /// or the `yes` would be spent on the line under the cursor alone.
    #[test]
    fn discarding_a_region_asks_about_the_lines_and_keeps_them() {
        let (mut magit, mut editor) = showing(unstaged_with_two_hunks(), |l| {
            matches!(l, git::Line::File { .. })
        });
        let (top, bottom) = (row(&editor, "-one"), row(&editor, "+ONE"));
        select(&mut editor, top, bottom);

        magit.run(&mut editor, "discard");
        let label = editor
            .prompt
            .as_ref()
            .expect("a confirmation is open")
            .label
            .clone();
        assert!(label.contains("these lines"), "{label}");
        assert!(!label.contains("hunk"), "{label}");
        assert_eq!(
            editor.pending_confirm.as_deref(),
            Some(&EditorCommand::Confirmed(Box::new(EditorCommand::Git(
                "discard".into()
            ))))
        );
        // The prompt takes the keyboard, not the selection: what the question
        // was about is what the `yes` will act on.
        assert_eq!(
            magit.picked(&editor).unwrap().unwrap().lines,
            vec![1, 2],
            "the region has to survive the confirmation"
        );
    }

    /// `k` on a file and `k` on a hunk throw away different things, so they have
    /// to ask different questions — and the question is chosen before the guard
    /// is spent, which means the *line* picks it.
    #[test]
    fn discarding_asks_about_the_thing_under_the_cursor() {
        let (mut magit, mut editor) =
            showing(unstaged_with_a_hunk(), |l| matches!(l, git::Line::File { .. }));
        magit.run(&mut editor, "discard");
        let label = editor.prompt.as_ref().expect("a confirmation is open").label.clone();
        assert!(label.contains("changes to this file"), "{label}");

        let (mut magit, mut editor) =
            showing(unstaged_with_a_hunk(), |l| matches!(l, git::Line::Hunk { .. }));
        magit.run(&mut editor, "discard");
        let label = editor.prompt.as_ref().expect("a confirmation is open").label.clone();
        assert!(label.contains("this hunk"), "{label}");

        // And a third question for an untracked file, which is the one `k` that
        // *deletes* rather than reverts: nothing in the repository holds a copy,
        // so "discard the changes?" would be asking about the wrong loss.
        let untracked = git::View::of(git::Status {
            branch: Some("main".into()),
            untracked: vec![PathBuf::from("new.rs")],
            ..Default::default()
        });
        let (mut magit, mut editor) = showing(untracked, |l| matches!(l, git::Line::File { .. }));
        magit.run(&mut editor, "discard");
        let label = editor.prompt.as_ref().expect("a confirmation is open").label.clone();
        assert!(label.contains("Delete this file"), "{label}");
        // Either way the repository is untouched until the answer comes back.
        assert_eq!(
            editor.pending_confirm.as_deref(),
            Some(&EditorCommand::Confirmed(Box::new(EditorCommand::Git(
                "discard".into()
            ))))
        );
    }

    /// The map describes the status buffer and nothing else. Read it from a
    /// source file and `A` cherry-picks whatever happened to be on that row.
    #[test]
    fn the_line_map_is_never_read_from_another_buffer() {
        let status = git::Status {
            recent: vec![git::Commit {
                hash: "abc1234".into(),
                subject: "first".into(),
            }],
            ..Default::default()
        };
        let (magit, mut editor) =
            showing(git::View::of(status), |l| matches!(l, git::Line::Commit { .. }));
        assert_eq!(magit.revision(&editor, "").unwrap(), "abc1234");
        // An argument always wins: that is Lisp having asked.
        assert_eq!(magit.revision(&editor, "main").unwrap(), "main");

        editor.show_special(BufferKind::Scratch, "a source file\nwith lines\n");
        assert!(magit.line_at_cursor(&editor).is_none());
        assert!(magit.revision(&editor, "").is_err());
        // ...and the stash falls back to the top of the stack rather than to
        // whatever row the cursor happens to be on.
        assert_eq!(magit.stash_at_cursor(&editor), "stash@{0}");
    }

    /// `r a` was one keystroke away from discarding a whole rebase. It has to
    /// stop at a question, and it has to stop *before* touching the repository —
    /// which is why the guard is in `run` and not in `try_run`'s arm.
    #[test]
    fn a_destructive_verb_asks_before_it_runs() {
        let mut magit = Magit::default();
        let mut ed = Editor::new();
        // No repository located, so reaching git at all would fail loudly with
        // "no repository" — the absence of that message is the proof it stopped.
        magit.run(&mut ed, "rebase-abort");

        let prompt = ed.prompt.as_ref().expect("a confirmation is open");
        assert_eq!(prompt.kind, zemacs_core::PromptKind::Confirm);
        assert!(prompt.label.contains("throw away"), "{}", prompt.label);
        assert!(!ed.status.contains("no repository"), "{}", ed.status);
        assert_eq!(
            ed.pending_confirm.as_deref(),
            Some(&EditorCommand::Confirmed(Box::new(EditorCommand::Git(
                "rebase-abort".into()
            ))))
        );
    }

    /// And the ordinary ones must not grow a prompt: a confirmation in front of
    /// `stage` is how people learn to answer without reading.
    #[test]
    fn an_ordinary_verb_is_not_guarded() {
        assert!(confirm_question("stage").is_none());
        assert!(confirm_question("stash").is_none());
        assert!(confirm_question("commit").is_none());
        assert!(confirm_question("push").is_none());
        assert!(confirm_question("checkout").is_none());
        assert!(confirm_question("visit").is_none());
        assert!(confirm_question("log").is_none());
        assert!(confirm_question("rebase-abort").is_some());
    }

    /// Every verb that can lose work, checked against the list in
    /// `zemacs-git`'s module doc rather than against what happens to be bound:
    /// a verb reachable from `M-x` or from `(magit "…")` is reachable whether
    /// or not it has a key.
    #[test]
    fn everything_that_can_lose_work_stops_to_ask() {
        for verb in [
            "discard",
            "discard-hunk",
            "discard-lines",
            "reset-hard",
            "stash-drop",
            "branch-delete-force",
            "drop-commit",
            "rebase-abort",
            "abort",
            "resolve-ours",
            "resolve-theirs",
            "push-force",
        ] {
            assert!(confirm_question(verb).is_some(), "{verb} does not ask");
        }
    }

    #[test]
    fn summarize_takes_the_first_meaningful_line() {
        assert_eq!(summarize("\n [main abc1234] hi\n 1 file\n", "commit"), "[main abc1234] hi");
        assert_eq!(summarize("   \n", "push"), "push: done");
    }
}
