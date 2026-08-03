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

use zemacs_core::{BufferKind, Editor, EditorCommand, HlKind};
use zemacs_git as git;

/// Comment prefix in the commit message buffer, as in `COMMIT_EDITMSG`.
const COMMENT: &str = "#";

/// The question to ask before `verb`, or `None` if it cannot lose work.
///
/// The list is `zemacs-git`'s own — the module doc there names every function
/// that can lose work, and this is that list read back as verbs. Most are not
/// wired to a verb yet; they are here anyway, because the moment somebody adds
/// `"discard" =>` to `try_run` the guard should already be in front of it
/// rather than be a second thing to remember.
///
/// Deliberately *not* everything git can undo. `stash` is recoverable with `z
/// p`, `amend` and `unstage-all` leave the work in the tree or the reflog, and
/// `push` is checked by the remote — asking about those is how a confirmation
/// prompt becomes something you dismiss without reading, which would cost the
/// one below its whole value.
fn confirm_question(verb: &str) -> Option<&'static str> {
    match verb {
        "rebase-abort" => Some("Abort the rebase and throw away its work?"),
        "reset-hard" => Some("Reset --hard, discarding all uncommitted changes?"),
        "discard" => Some("Discard the changes to this file?"),
        "discard-hunk" => Some("Discard this hunk?"),
        "stash-drop" => Some("Drop this stash for good?"),
        "branch-delete-force" => Some("Delete this branch even if unmerged?"),
        "drop-commit" => Some("Drop this commit?"),
        _ => None,
    }
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
}

impl Magit {
    /// Run a `magit-*` verb, asking first if it can lose work.
    ///
    /// The guard is here rather than in the arms of `try_run` because this is
    /// the funnel: a keybinding, `M-x`, and `(git "...")` from Lisp all arrive
    /// as one `EditorCommand::Git`, so one check in front of them covers all
    /// three and cannot be forgotten by whoever adds the fourth.
    pub fn run(&mut self, editor: &mut Editor, verb: &str) {
        if let Some(question) = confirm_question(verb) {
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
                    _ => {}
                }
                self.render(editor)
            }
            // Staging is line-sensitive: on a hunk it stages *that hunk*, which
            // is the thing Magit is used for more than any other.
            "stage" | "unstage" => {
                let repo = self.repo()?.to_path_buf();
                let line = self
                    .line_at_cursor(editor)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("nothing to {verb} on this line"))?;
                match line {
                    git::Line::Hunk {
                        path,
                        section,
                        index,
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
            "stash" | "stash-pop" => {
                let repo = self.repo()?.to_path_buf();
                let out = match verb {
                    "stash" => git::stash_push(&repo, "")?,
                    _ => git::stash_pop(&repo, "stash@{0}")?,
                };
                self.refresh(editor)?;
                editor.apply(EditorCommand::Message(summarize(&out, verb)));
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
                editor.apply(EditorCommand::Message(match outcome {
                    git::RebaseOutcome::Done(msg) => summarize(&msg, "rebase"),
                    git::RebaseOutcome::Stopped { rebase, .. } => {
                        if rebase.is_conflicted() {
                            format!("rebase stopped: {} conflicted", rebase.conflicts.len())
                        } else {
                            format!("rebase stopped at {}/{}", rebase.done, rebase.total)
                        }
                    }
                }));
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
            "push" | "pull" => {
                let repo = self.repo()?.to_path_buf();
                let out = if verb == "push" {
                    git::push(&repo)?
                } else {
                    git::pull(&repo)?
                };
                self.refresh(editor)?;
                editor.apply(EditorCommand::Message(summarize(&out, verb)));
                Ok(())
            }
            "commit" => {
                let repo = self.repo()?.to_path_buf();
                let status = git::status(&repo)?;
                if status.staged.is_empty() {
                    anyhow::bail!("nothing staged to commit");
                }
                editor.show_special(BufferKind::CommitMessage, &commit_template(&status));
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
                let out = git::commit(&repo, &message)?;
                self.refresh(editor)?;
                editor.apply(EditorCommand::Message(summarize(&out, "commit")));
                Ok(())
            }
            other => anyhow::bail!("unknown git verb: {other}"),
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
    fn line_at_cursor(&self, editor: &Editor) -> Option<&git::Line> {
        let (line, _) = editor.buffer.cursor_line_col();
        self.lines.get(line)
    }
}

/// `git commit` prints several lines; the status line wants one.
fn summarize(out: &str, verb: &str) -> String {
    match out.lines().find(|l| !l.trim().is_empty()) {
        Some(first) => first.trim().to_string(),
        None => format!("{verb}: done"),
    }
}

/// The `COMMIT_EDITMSG` template: an empty first line to type into, then the
/// staged files as comments so you can see what you are committing.
fn commit_template(status: &git::Status) -> String {
    let mut s = String::from("\n");
    s.push_str(&format!("{COMMENT} Please enter the commit message for your changes.\n"));
    s.push_str(&format!("{COMMENT} Lines starting with '{COMMENT}' are ignored.\n"));
    s.push_str(&format!("{COMMENT}\n"));
    if let Some(b) = &status.branch {
        s.push_str(&format!("{COMMENT} On branch {b}\n"));
    }
    s.push_str(&format!("{COMMENT} Changes to be committed:\n"));
    for c in &status.staged {
        s.push_str(&format!(
            "{COMMENT}\t{}  {}\n",
            c.status.letter(),
            c.path.display()
        ));
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

    #[test]
    fn the_template_lists_what_is_staged_and_starts_empty() {
        let status = git::Status {
            branch: Some("main".into()),
            staged: vec![git::FileChange {
                path: "a.rs".into(),
                status: git::ChangeKind::Modified,
            }],
            ..Default::default()
        };
        let t = commit_template(&status);
        assert!(t.starts_with('\n'), "cursor lands on an empty first line");
        assert!(t.contains("On branch main"));
        assert!(t.contains("M  a.rs"));
        // and the template alone is not a message
        assert_eq!(strip_comments(&t), "");
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
        assert!(confirm_question("rebase-abort").is_some());
    }

    #[test]
    fn summarize_takes_the_first_meaningful_line() {
        assert_eq!(summarize("\n [main abc1234] hi\n 1 file\n", "commit"), "[main abc1234] hi");
        assert_eq!(summarize("   \n", "push"), "push: done");
    }
}
