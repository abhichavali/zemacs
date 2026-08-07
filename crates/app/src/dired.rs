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
use zemacs_core::{BufferKind, Editor, EditorCommand, HlKind, PromptKind};

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
    CreateFile,
}

impl Dired {
    /// True while a prompt belongs to dired, so the app routes its answer here.
    pub fn awaiting_input(&self) -> bool {
        self.pending.is_some()
    }

    /// Run a `dired-*` verb, asking first if it destroys files.
    ///
    /// The guard is here rather than in the arms of [`Self::try_run`] because
    /// this is the funnel: a keybinding, `M-x` and `(dired "…")` from Lisp all
    /// arrive as one `EditorCommand::Dired`, so one check in front of them
    /// covers all three and cannot be forgotten by whoever adds the fourth.
    /// `crates/app/src/magit.rs` guards the same way for the same reason.
    pub fn run(&mut self, editor: &mut Editor, verb: &str) {
        if let Some(question) = self.confirm_question(editor, verb) {
            editor.confirm(
                &question,
                EditorCommand::Confirmed(Box::new(EditorCommand::Dired(verb.to_string()))),
            );
            return;
        }
        self.run_confirmed(editor, verb)
    }

    /// The far side of a `yes`, and the entry point for every verb that never
    /// had to ask.
    pub fn run_confirmed(&mut self, editor: &mut Editor, verb: &str) {
        if let Err(e) = self.try_run(editor, verb) {
            editor.apply(EditorCommand::Message(format!("dired: {e:#}")));
        }
    }

    /// The question to ask before `verb`, or `None` if it cannot lose work.
    ///
    /// Both arms name *what* is about to go rather than asking "are you sure":
    /// a prompt you can answer without reading is worth nothing, and the whole
    /// value of this one is that `D` on the wrong line is survivable.
    ///
    /// `None` when there is nothing to destroy, so the verb runs and reports
    /// "no file on this line" itself — a confirmation for a no-op teaches the
    /// habit of dismissing confirmations.
    fn confirm_question(&self, editor: &Editor, verb: &str) -> Option<String> {
        let doomed = match verb {
            "delete" => self.selected(editor).ok()?,
            "execute" => self.flagged().ok()?,
            _ => return None,
        };
        match doomed.as_slice() {
            [] => None,
            [one] => Some(format!(
                "Delete {}?",
                one.file_name().unwrap_or(one.as_os_str()).to_string_lossy()
            )),
            many => Some(format!("Delete {} files?", many.len())),
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
            Pending::CreateFile => {
                dired::create_file(&dir, answer)?;
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
            // Emacs' `D`: delete now, rather than flagging and expunging. The
            // guard is in `run`, so nothing reaches here unasked.
            "delete" => {
                let doomed = self.selected(editor)?;
                if doomed.is_empty() {
                    anyhow::bail!("no file on this line");
                }
                self.remove_all(editor, &doomed)
            }
            // `u` clears one mark and `t` inverts them all; neither is the
            // "I have lost track of what is marked" gesture, which is this.
            "unmark-all" => {
                self.marks.clear();
                self.refresh(editor)
            }
            // Emacs' `w`. The *name*, not the path, because that is what Emacs
            // copies and what you want when the next thing you type is a shell
            // command in this directory.
            "copy-filename" => {
                let name = self
                    .entry_at_cursor(editor)
                    .filter(|e| !e.is_dot())
                    .map(|e| e.name.to_string_lossy().into_owned());
                let Some(name) = name else {
                    anyhow::bail!("no file on this line");
                };
                editor.apply(EditorCommand::SetRegister {
                    text: name.clone(),
                    linewise: false,
                });
                editor.apply(EditorCommand::Message(name));
                Ok(())
            }
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
            // `create-file` is Emacs' `dired-create-empty-file`: it makes the
            // file and leaves you in the listing, rather than opening it. `RET`
            // is one key, and a new file you did not want open is the more
            // annoying half of the two.
            "mkdir" | "create-file" => {
                let (pending, label) = match verb {
                    "mkdir" => (Pending::Mkdir, "New directory: "),
                    _ => (Pending::CreateFile, "New file: "),
                };
                self.pending = Some(pending);
                editor.open_prompt(PromptKind::File);
                if let Some(p) = editor.prompt.as_mut() {
                    p.label = label.into();
                }
                Ok(())
            }
            other => anyhow::bail!("unknown dired verb: {other}"),
        }
    }

    /// What `x` acts on: everything flagged `D`.
    fn flagged(&self) -> anyhow::Result<Vec<PathBuf>> {
        Ok(self
            .listing()?
            .entries
            .iter()
            .filter(|e| self.marks.get(&e.name) == Some(&dired::MARK_DELETE))
            .map(|e| e.path.clone())
            .collect())
    }

    /// What `D` acts on: everything marked `*`, or the entry under the cursor
    /// when nothing is marked.
    ///
    /// Emacs' rule, and the reason `D` is worth having next to `d`+`x`: the
    /// common case is deleting the one file you are looking at, and making that
    /// a two-key ceremony is how people stop using the marks for the case that
    /// needs them. `.` and `..` are never it.
    fn selected(&self, editor: &Editor) -> anyhow::Result<Vec<PathBuf>> {
        let marked: Vec<PathBuf> = self
            .listing()?
            .entries
            .iter()
            .filter(|e| self.marks.get(&e.name) == Some(&dired::MARK_SELECT))
            .map(|e| e.path.clone())
            .collect();
        if !marked.is_empty() {
            return Ok(marked);
        }
        Ok(self
            .entry_at_cursor(editor)
            .filter(|e| !e.is_dot())
            .map(|e| vec![e.path.clone()])
            .unwrap_or_default())
    }

    /// Delete `doomed`, and say how it went.
    ///
    /// Both `D` and `x` come through here, so this is the only place data is
    /// destroyed — one routine to audit, one count to trust, and no way for a
    /// second delete path to grow its own quieter reporting.
    fn remove_all(&mut self, editor: &mut Editor, doomed: &[PathBuf]) -> anyhow::Result<()> {
        let mut gone = 0usize;
        let mut failed = Vec::new();
        for path in doomed {
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

    /// Delete everything flagged `D`. Guarded by [`Self::confirm_question`], so
    /// by the time this runs the count has been shown and agreed to.
    fn execute(&mut self, editor: &mut Editor) -> anyhow::Result<()> {
        let doomed = self.flagged()?;
        if doomed.is_empty() {
            editor.apply(EditorCommand::Message("nothing flagged for deletion".into()));
            return Ok(());
        }
        self.remove_all(editor, &doomed)
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
        let (text, lines, spans) = dired::render(&listing, &marks);
        let line = editor.buffer.cursor_line_col().0;
        self.listing = Some(listing);
        self.lines = lines;
        editor.show_special(BufferKind::Dired, &text);
        // A listing has no language for the syntax thread to parse, so it hands
        // over its own spans. Same faces as everything else, so a theme reaches
        // dired without knowing dired exists.
        editor.buffer.highlights = spans.into_iter().map(face_span).collect();
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

/// dired names its faces, core owns the enum. Mechanical, and the only place
/// the two vocabularies meet.
fn face_span(span: dired::Span) -> zemacs_core::Span {
    use dired::Face;
    zemacs_core::Span {
        start: span.start,
        end: span.end,
        kind: match span.kind {
            Face::Heading1 => HlKind::Heading1,
            Face::Type => HlKind::Type,
            Face::Function => HlKind::Function,
            Face::Link => HlKind::Link,
            Face::Number => HlKind::Number,
            Face::Comment => HlKind::Comment,
            Face::Constant => HlKind::Constant,
            Face::Punctuation => HlKind::Punctuation,
        },
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

    /// A listing of three files, with the cursor parked on a real entry.
    fn listing_of_three(name: &str) -> (Dired, Editor, PathBuf) {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let mut editor = Editor::default();
        let mut dired = Dired {
            dir: Some(dir.clone()),
            ..Default::default()
        };
        dired.refresh(&mut editor).unwrap();
        // Onto the first real entry — past `.` and `..`, which `selected`
        // refuses and which would otherwise make this fixture assert nothing.
        let entries = &dired.listing.as_ref().unwrap().entries;
        let line = dired
            .lines
            .iter()
            .position(|l| match l {
                dired::Line::Entry(i) => !entries[*i].is_dot(),
                _ => false,
            })
            .expect("a listing of three files has a real entry");
        editor.buffer.cursor = editor.buffer.line_start(line);
        (dired, editor, dir)
    }

    /// The rule `D` lives or dies by. Deleting is not undoable here, so
    /// "which files" has to be exactly Emacs' answer and nothing looser.
    #[test]
    fn delete_takes_the_marks_or_the_line_and_asks_before_either() {
        let (mut dired, editor, dir) = listing_of_three("zemacs_dired_delete_one");

        // Nothing marked: the entry under the cursor, alone.
        let one = dired.selected(&editor).unwrap();
        assert_eq!(one.len(), 1, "unmarked D takes only the current line");
        assert!(one[0].starts_with(&dir));

        // ...and it asks by *name*, not by count, so a wrong line is visible.
        let q = dired.confirm_question(&editor, "delete").unwrap();
        assert!(q.starts_with("Delete ") && q.ends_with('?'), "{q}");
        assert!(q.contains(".txt"), "one file is named in the question: {q}");

        // Marks win over the cursor, and take *all* of them.
        for f in ["a.txt", "c.txt"] {
            dired.marks.insert(f.into(), dired::MARK_SELECT);
        }
        assert_eq!(dired.selected(&editor).unwrap().len(), 2);
        assert_eq!(
            dired.confirm_question(&editor, "delete").as_deref(),
            Some("Delete 2 files?"),
        );

        // `x` is a different set: delete *flags*, not selection marks. Mixing
        // the two would make `D` expunge what `d` flagged, or the reverse.
        assert!(dired.flagged().unwrap().is_empty());
        assert_eq!(dired.confirm_question(&editor, "execute"), None);

        // Verbs that cannot lose work are never guarded — a confirmation for a
        // no-op is how people learn to dismiss them unread.
        assert_eq!(dired.confirm_question(&editor, "refresh"), None);
        assert_eq!(dired.confirm_question(&editor, "mark"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `U` clears every mark, where `u` clears one. Asserted because the
    /// difference between them is the whole reason `U` exists.
    #[test]
    fn unmark_all_clears_both_kinds_of_mark() {
        let (mut dired, mut editor, dir) = listing_of_three("zemacs_dired_unmark_all");
        dired.marks.insert("a.txt".into(), dired::MARK_SELECT);
        dired.marks.insert("b.txt".into(), dired::MARK_DELETE);

        dired.run_confirmed(&mut editor, "unmark-all");

        assert!(dired.marks.is_empty());
        assert!(dired.selected(&editor).unwrap().len() <= 1, "back to the line");
        assert!(dired.flagged().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
