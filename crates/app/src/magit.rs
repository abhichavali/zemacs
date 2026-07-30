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

use zemacs_core::{BufferKind, Editor, EditorCommand};
use zemacs_git as git;

/// Comment prefix in the commit message buffer, as in `COMMIT_EDITMSG`.
const COMMENT: &str = "#";

#[derive(Default)]
pub struct Magit {
    /// Repository the status buffer is showing.
    repo: Option<PathBuf>,
    /// One entry per line of the status buffer, parallel to its text.
    lines: Vec<git::Line>,
}

impl Magit {
    /// Run a `magit-*` verb. Errors are reported in the status line and are
    /// never fatal: a failed push is ordinary news, not a crash.
    pub fn run(&mut self, editor: &mut Editor, verb: &str) {
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
            "stage" | "unstage" => {
                let repo = self.repo()?.to_path_buf();
                let path = self
                    .path_at_cursor(editor)
                    .ok_or_else(|| anyhow::anyhow!("nothing to {verb} on this line"))?;
                match verb {
                    "stage" => git::stage(&repo, &path)?,
                    _ => git::unstage(&repo, &path)?,
                }
                self.refresh(editor)
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

    /// Re-render the status buffer, keeping it the one `*magit*` buffer.
    fn refresh(&mut self, editor: &mut Editor) -> anyhow::Result<()> {
        let repo = self.repo()?.to_path_buf();
        let status = git::status(&repo)?;
        let (text, lines) = git::render(&status);
        self.lines = lines;
        editor.show_special(BufferKind::Magit, &text);
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

    /// The file named by the line the cursor is on. `None` on a heading or a
    /// blank, which is why staging there is a message rather than a mistake.
    fn path_at_cursor(&self, editor: &Editor) -> Option<PathBuf> {
        let (line, _) = editor.buffer.cursor_line_col();
        match self.lines.get(line)? {
            git::Line::File { path, .. } => Some(path.clone()),
            git::Line::Hunk { path, .. } => Some(path.clone()),
            _ => None,
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

    #[test]
    fn summarize_takes_the_first_meaningful_line() {
        assert_eq!(summarize("\n [main abc1234] hi\n 1 file\n", "commit"), "[main abc1234] hi");
        assert_eq!(summarize("   \n", "push"), "push: done");
    }
}
