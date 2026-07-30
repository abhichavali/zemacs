//! The dired half that needs the filesystem, mirroring [`crate::magit`].
//!
//! `zemacs-core` knows the verbs and the buffer kind; `zemacs-dired` knows
//! `std::fs`. This is the seam: it runs the verb, then hands the rendered
//! listing back to the editor as buffer text.
//!
//! Marks live here rather than in the listing, because they are UI state: they
//! survive a refresh (a re-`list` after staging a rename should not forget what
//! you had selected), and they are keyed by *name* rather than by index for the
//! same reason — an index means something different after a sort or a delete.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use zemacs_dired as dired;
use zemacs_core::{BufferKind, Editor, EditorCommand, PromptKind};

#[derive(Default)]
pub struct Dired {
    dir: Option<PathBuf>,
    listing: Option<dired::Listing>,
    lines: Vec<dired::Line>,
    /// Marks by file name, so they survive a re-list.
    marks: HashMap<OsString, char>,
    show_hidden: bool,
    sort: dired::Sort,
    /// A rename/copy/mkdir waiting on the prompt the user is typing into.
    pending: Option<Pending>,
    /// Set when `RET` lands on a file; the app drains it and opens the file,
    /// since opening a buffer is its job rather than dired's.
    pub open_file: Option<PathBuf>,
}

/// An operation that needs a name before it can run.
enum Pending {
    Rename(PathBuf),
    Copy(PathBuf),
    Mkdir,
}

impl Dired {
    /// True while a prompt belongs to dired, so the app routes its answer here.
    pub fn awaiting_input(&self) -> bool {
        self.pending.is_some()
    }

    pub fn run(&mut self, editor: &mut Editor, verb: &str) {
        if let Err(e) = self.try_run(editor, verb) {
            editor.apply(EditorCommand::Message(format!("dired: {e:#}")));
        }
    }

    /// Answer to the prompt a `rename`/`copy`/`mkdir` opened.
    pub fn supply(&mut self, editor: &mut Editor, answer: &str) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if let Err(e) = self.apply_pending(editor, pending, answer) {
            editor.apply(EditorCommand::Message(format!("dired: {e:#}")));
        }
    }

    fn apply_pending(
        &mut self,
        editor: &mut Editor,
        pending: Pending,
        answer: &str,
    ) -> anyhow::Result<()> {
        let answer = answer.trim();
        if answer.is_empty() {
            anyhow::bail!("cancelled");
        }
        let dir = self.dir()?.to_path_buf();
        match pending {
            // A bare name stays in this directory; a path with a separator is
            // taken as given, so you can move a file elsewhere.
            Pending::Rename(from) => dired::rename(&from, &resolve(&dir, answer))?,
            Pending::Copy(from) => dired::copy(&from, &resolve(&dir, answer))?,
            Pending::Mkdir => {
                dired::create_dir(&dir, answer)?;
            }
        }
        self.refresh(editor)
    }

    fn try_run(&mut self, editor: &mut Editor, verb: &str) -> anyhow::Result<()> {
        match verb {
            "open" => {
                self.dir = Some(self.locate(editor));
                self.marks.clear();
                self.refresh(editor)
            }
            "refresh" => self.refresh(editor),
            "toggle-hidden" => {
                self.show_hidden = !self.show_hidden;
                self.refresh(editor)
            }
            "up" => {
                let dir = self.dir()?.to_path_buf();
                match dir.parent() {
                    Some(parent) => self.enter_dir(editor, parent.to_path_buf()),
                    None => Ok(()), // already at the root
                }
            }
            "enter" => {
                let Some(entry) = self.entry_at_cursor(editor) else {
                    return Ok(());
                };
                if entry.is_dir {
                    // `..` and `.` resolve through the path rather than the
                    // name, so `..` from `/a/b` is `/a` and not `/a/b/..`.
                    let target = entry.path.canonicalize().unwrap_or(entry.path.clone());
                    self.enter_dir(editor, target)
                } else {
                    // A file leaves dired entirely — the app opens it.
                    editor.apply(EditorCommand::Message(format!(
                        "opening {}",
                        entry.path.display()
                    )));
                    self.open_file = Some(entry.path.clone());
                    Ok(())
                }
            }
            "mark" | "flag-delete" => {
                let mark = if verb == "mark" {
                    dired::MARK_SELECT
                } else {
                    dired::MARK_DELETE
                };
                if let Some(entry) = self.entry_at_cursor(editor) {
                    if !entry.is_dot() {
                        self.marks.insert(entry.name.clone(), mark);
                    }
                }
                self.refresh_keeping_line(editor, 1)
            }
            "unmark" => {
                let name = self.entry_at_cursor(editor).map(|e| e.name.clone());
                if let Some(name) = name {
                    self.marks.remove(&name);
                }
                self.refresh_keeping_line(editor, 1)
            }
            "toggle-marks" => {
                let names: Vec<OsString> = self
                    .listing()?
                    .entries
                    .iter()
                    .filter(|e| !e.is_dot())
                    .map(|e| e.name.clone())
                    .collect();
                for name in names {
                    match self.marks.remove(&name) {
                        Some(_) => {}
                        None => {
                            self.marks.insert(name, dired::MARK_SELECT);
                        }
                    }
                }
                self.refresh(editor)
            }
            "execute" => self.execute(editor),
            "rename" | "copy" => {
                let Some(entry) = self.entry_at_cursor(editor) else {
                    anyhow::bail!("no file on this line");
                };
                if entry.is_dot() {
                    anyhow::bail!("cannot {verb} {}", entry.name.to_string_lossy());
                }
                let name = entry.name.to_string_lossy().into_owned();
                self.pending = Some(match verb {
                    "rename" => Pending::Rename(entry.path.clone()),
                    _ => Pending::Copy(entry.path.clone()),
                });
                editor.open_prompt(PromptKind::File);
                if let Some(p) = editor.prompt.as_mut() {
                    p.label = format!("{verb} to: ");
                    p.text = name;
                    p.refilter();
                }
                Ok(())
            }
            "mkdir" => {
                self.pending = Some(Pending::Mkdir);
                editor.open_prompt(PromptKind::File);
                if let Some(p) = editor.prompt.as_mut() {
                    p.label = "New directory: ".into();
                }
                Ok(())
            }
            other => anyhow::bail!("unknown dired verb: {other}"),
        }
    }

    /// Delete everything flagged `D`. Nothing else acts on the delete flags, so
    /// this is the only place data is destroyed — and it reports a count rather
    /// than going quiet.
    fn execute(&mut self, editor: &mut Editor) -> anyhow::Result<()> {
        let doomed: Vec<PathBuf> = self
            .listing()?
            .entries
            .iter()
            .filter(|e| self.marks.get(&e.name) == Some(&dired::MARK_DELETE))
            .map(|e| e.path.clone())
            .collect();
        if doomed.is_empty() {
            editor.apply(EditorCommand::Message("nothing flagged for deletion".into()));
            return Ok(());
        }
        let mut gone = 0usize;
        let mut failed = Vec::new();
        for path in &doomed {
            match dired::delete(path) {
                Ok(()) => {
                    gone += 1;
                    if let Some(name) = path.file_name() {
                        self.marks.remove(name);
                    }
                }
                Err(e) => failed.push(format!("{}: {e}", path.display())),
            }
        }
        self.refresh(editor)?;
        let msg = match failed.as_slice() {
            [] => format!("deleted {gone}"),
            errs => format!("deleted {gone}, failed {}: {}", errs.len(), errs.join("; ")),
        };
        editor.apply(EditorCommand::Message(msg));
        Ok(())
    }

    fn enter_dir(&mut self, editor: &mut Editor, dir: PathBuf) -> anyhow::Result<()> {
        self.dir = Some(dir);
        self.marks.clear();
        self.refresh(editor)
    }

    fn refresh(&mut self, editor: &mut Editor) -> anyhow::Result<()> {
        self.refresh_keeping_line(editor, 0)
    }

    /// Re-list and redraw, optionally advancing the cursor — marking a file
    /// moves down to the next one, the way dired does.
    fn refresh_keeping_line(&mut self, editor: &mut Editor, advance: usize) -> anyhow::Result<()> {
        let dir = self.dir()?.to_path_buf();
        let listing = dired::list(&dir, self.show_hidden, self.sort)?;
        let marks: Vec<Option<char>> = listing
            .entries
            .iter()
            .map(|e| self.marks.get(&e.name).copied())
            .collect();
        let (text, lines) = dired::render(&listing, &marks);
        let line = editor.buffer.cursor_line_col().0;
        self.listing = Some(listing);
        self.lines = lines;
        editor.show_special(BufferKind::Dired, &text);
        editor.buffer.path = Some(dir);
        let target = (line + advance).min(self.lines.len().saturating_sub(1));
        editor.buffer.move_to_line_col(target, 0);
        Ok(())
    }

    fn dir(&self) -> anyhow::Result<&Path> {
        self.dir
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no directory — run dired first"))
    }

    fn listing(&self) -> anyhow::Result<&dired::Listing> {
        self.listing
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no listing — run dired first"))
    }

    fn entry_at_cursor(&self, editor: &Editor) -> Option<&dired::Entry> {
        let (line, _) = editor.buffer.cursor_line_col();
        match self.lines.get(line)? {
            dired::Line::Entry(i) => self.listing.as_ref()?.entries.get(*i),
            _ => None,
        }
    }

    /// The directory to show: the one holding the current file, else the
    /// working directory.
    fn locate(&self, editor: &Editor) -> PathBuf {
        editor
            .buffer
            .path
            .as_deref()
            .and_then(|p| if p.is_dir() { Some(p.to_path_buf()) } else { p.parent().map(Path::to_path_buf) })
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

}

/// A bare name stays in `dir`; anything with a separator is taken as written.
fn resolve(dir: &Path, answer: &str) -> PathBuf {
    let p = Path::new(answer);
    if p.components().count() > 1 || answer.starts_with('/') {
        p.to_path_buf()
    } else {
        dir.join(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_name_stays_in_the_directory() {
        let dir = Path::new("/tmp/things");
        assert_eq!(resolve(dir, "new.txt"), Path::new("/tmp/things/new.txt"));
    }

    #[test]
    fn a_path_with_separators_is_taken_as_written() {
        let dir = Path::new("/tmp/things");
        assert_eq!(resolve(dir, "/etc/hosts"), Path::new("/etc/hosts"));
        assert_eq!(resolve(dir, "sub/new.txt"), Path::new("sub/new.txt"));
        // ...including going up, which is how you move a file out
        assert_eq!(resolve(dir, "../new.txt"), Path::new("../new.txt"));
    }

    #[test]
    fn names_with_spaces_are_not_split() {
        let dir = Path::new("/tmp");
        assert_eq!(resolve(dir, "two words.md"), Path::new("/tmp/two words.md"));
    }
}
