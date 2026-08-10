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
use zemacs_tramp as tramp;

#[derive(Default)]
pub struct Dired {
    dir: Option<PathBuf>,
    /// Set exactly when `dir` names a machine that is not this one. The two are
    /// kept in step by [`Dired::go`] and nowhere else, so every read of `dir`
    /// below is still "the directory on screen" and only the *listing* forks.
    remote: Option<tramp::RemotePath>,
    /// The last listing that came back over ssh, unfiltered and unsorted.
    ///
    /// Kept because marking a file, inverting the marks, toggling dotfiles and
    /// changing the sort all go through `refresh`, and over a network each of
    /// those must not be a round trip. A *fetch* is a separate, explicit step —
    /// `open`, `enter`, `up` and the `refresh` verb — and everything else
    /// re-renders from here.
    remote_entries: Vec<tramp::RemoteEntry>,
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
    /// Set when a remote directory needs fetching. Same shape as `open_file`
    /// and for the same reason: dired owns no ssh worker, so it asks.
    pub want_list: Option<tramp::RemotePath>,
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

    /// Run a `dired-*` verb — or list a directory — asking first if it
    /// destroys files.
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
        // ponytail: a remote listing is read-only. `zemacs-tramp` has `rename`,
        // `delete` and `mkdir` and they are tested, so the upgrade is three more
        // turns of the `want_list`/reply crank plus a `copy` script it does not
        // have yet. Refused here rather than in each arm because the arms below
        // hand `zemacs_dired` a `PathBuf` spelled `/ssh:host:/etc/x`, which is a
        // perfectly good *local* name for a file that is not there — so the
        // failure without this guard is "No such file", about the wrong machine.
        if self.remote.is_some()
            && matches!(
                verb,
                "rename" | "copy" | "delete" | "execute" | "mkdir" | "create-file"
            )
        {
            anyhow::bail!("{verb} is not available on a remote directory");
        }
        match verb {
            "open" => {
                let dir = self.locate(editor);
                self.go(editor, dir)
            }
            // The one verb that re-reads. Everything else that calls `refresh`
            // internally is a mark or a sort, and those re-render from the
            // entries already in hand — see `remote_entries`.
            "refresh" => {
                if let Some(remote) = self.remote.clone() {
                    self.want_list = Some(remote);
                }
                self.refresh(editor)
            }
            "toggle-hidden" => {
                self.show_hidden = !self.show_hidden;
                self.refresh(editor)
            }
            "up" => {
                // Two spellings of "the directory above" because they disagree:
                // `Path::parent` of `/ssh:host:/etc` is `/ssh:host:`, which
                // `tramp` reads as the remote *home* rather than as the root.
                let up = match &self.remote {
                    Some(remote) => remote.parent().map(|p| PathBuf::from(p.to_string())),
                    None => self.dir()?.parent().map(Path::to_path_buf),
                };
                match up {
                    Some(parent) => self.go(editor, parent),
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
                    // A remote name is already exact — it was built by
                    // `RemotePath::join`/`parent` — and `canonicalize` would
                    // only ask this machine about a file on another one.
                    let target = match self.remote {
                        Some(_) => entry.path.clone(),
                        None => entry.path.canonicalize().unwrap_or(entry.path.clone()),
                    };
                    self.go(editor, target)
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
            //
            // A *name*, not a path — these two always create in the directory
            // on screen, and [`dired::create_dir`]/[`dired::create_file`] refuse
            // a separator outright. So the prompt is marked `bare`: no listing
            // behind it, and Enter answers with what was typed. Filesystem
            // completion here was the bug both keys had, since `Prompt::value`
            // prefers the highlighted candidate over the text — `+` typed
            // `notes` and submitted a path `create_dir` then rejected. `rename`
            // and `copy` above still complete, because a path is the point
            // there: it is how you move a file somewhere else.
            "mkdir" | "create-file" => {
                let (pending, label) = match verb {
                    "mkdir" => (Pending::Mkdir, "New directory: "),
                    _ => (Pending::CreateFile, "New file: "),
                };
                self.pending = Some(pending);
                editor.open_prompt(PromptKind::File);
                if let Some(p) = editor.prompt.as_mut() {
                    p.label = label.into();
                    p.bare = true;
                }
                Ok(())
            }
            // Not a verb: a directory to list, which is the whole of `(dired
            // "/tmp")` from Lisp — the command carries one string, and there was
            // no other spelling that could name a directory. Told apart by
            // falling through rather than by shape, because the verbs above are
            // a closed set matched first: a directory called `open` cannot
            // shadow the verb, and a new verb cannot start meaning a path.
            other => {
                let dir = PathBuf::from(crate::expand_tilde(other));
                // A remote name cannot be checked without asking the host, and
                // asking is the very thing that must not happen on this thread.
                // So it is taken on trust and the listing reports if it was
                // wrong — which is also what happens to a local directory that
                // is deleted between this line and the next.
                if tramp::parse(other).is_some() {
                    return self.go(editor, dir);
                }
                if !dir.is_dir() {
                    anyhow::bail!("no such verb or directory: {other}");
                }
                // Canonical for the reason `enter` is: a relative path or a `..`
                // here would make `up` walk somewhere that is not the parent.
                self.go(editor, dir.canonicalize().unwrap_or(dir))
            }
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

    /// Show `dir`, wherever it is. The single place `dir` and `remote` are set,
    /// so the two can never disagree about which machine the listing is from.
    ///
    /// A remote directory renders nothing yet: there is nothing to render until
    /// the reply lands, and the alternative is an ssh round trip on the main
    /// thread. `want_list` is the ask; [`Dired::listed`] is the answer.
    fn go(&mut self, editor: &mut Editor, dir: PathBuf) -> anyhow::Result<()> {
        self.remote = tramp::parse(&dir.to_string_lossy());
        self.dir = Some(dir);
        self.marks.clear();
        if let Some(remote) = self.remote.clone() {
            // Not kept across the move: the entries describe the directory we
            // are leaving, and rendering them under the new name would show one
            // directory's files with another's heading.
            self.remote_entries.clear();
            editor.apply(EditorCommand::Message(format!("listing {remote}…")));
            self.want_list = Some(remote);
            return Ok(());
        }
        self.refresh(editor)
    }

    /// A remote listing came back. Renders it, unless the user has walked
    /// somewhere else in the meantime — replies are not cancelled, so a slow
    /// one for a directory nobody is looking at any more is dropped here.
    pub fn listed(
        &mut self,
        editor: &mut Editor,
        dir: &tramp::RemotePath,
        entries: Vec<tramp::RemoteEntry>,
    ) {
        if self.remote.as_ref() != Some(dir) {
            return;
        }
        self.remote_entries = entries;
        if let Err(e) = self.refresh(editor) {
            editor.apply(EditorCommand::Message(format!("dired: {e:#}")));
        }
    }

    /// Point dired at a remote directory somebody else decided on — `find-file`
    /// on a name that turned out to be a directory. The fetch is the caller's,
    /// since it is already holding the worker that would do it.
    pub fn adopt_remote(&mut self, dir: &tramp::RemotePath) {
        self.remote = Some(dir.clone());
        self.dir = Some(PathBuf::from(dir.to_string()));
        self.remote_entries.clear();
        self.marks.clear();
    }

    fn refresh(&mut self, editor: &mut Editor) -> anyhow::Result<()> {
        self.refresh_keeping_line(editor, 0)
    }

    /// Re-list and redraw, optionally advancing the cursor — marking a file
    /// moves down to the next one, the way dired does.
    fn refresh_keeping_line(&mut self, editor: &mut Editor, advance: usize) -> anyhow::Result<()> {
        let dir = self.dir()?.to_path_buf();
        // The only fork in this file. Everything below — marks, rendering, the
        // cursor — works on a `dired::Listing` and does not care which machine
        // built it, which is the whole reason the remote half is a converter
        // rather than a second dired.
        let listing = match self.remote.clone() {
            Some(remote) => self.remote_listing(&remote),
            None => dired::list(&dir, self.show_hidden, self.sort)?,
        };
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

    /// The remote entries in hand, as the listing `dired::render` wants.
    ///
    /// Deliberately the same shape as [`dired::list`]: `..` always, `.` only
    /// with hidden files, dotfiles filtered here rather than by the remote (the
    /// listing already crossed the network, so filtering it again costs
    /// nothing and toggling `.` costs no round trip), and the same
    /// [`dired::sort_entries`] so a directory does not read differently
    /// depending on which machine it is on.
    fn remote_listing(&self, remote: &tramp::RemotePath) -> dired::Listing {
        let mut entries = vec![dot_entry(remote, "..")];
        if self.show_hidden {
            entries.push(dot_entry(remote, "."));
        }
        entries.extend(
            self.remote_entries
                .iter()
                .filter(|e| self.show_hidden || !e.name.starts_with('.'))
                .map(|e| remote_entry(remote, e)),
        );
        dired::sort_entries(&mut entries, self.sort);
        dired::Listing {
            dir: PathBuf::from(remote.to_string()),
            entries,
            show_hidden: self.show_hidden,
            sort: self.sort,
        }
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

/// One remote entry as a dired line.
///
/// The path is the *tramp name* — `/ssh:host:/etc/nginx.conf` — because that is
/// what every consumer of it wants: `RET` hands it to `find-file`, which parses
/// it straight back into the [`tramp::RemotePath`] it came from, and the
/// modeline shows the machine it is on. A `PathBuf` here is a string with a
/// separator in it and nothing more; nothing ever hands it to `std::fs`.
fn remote_entry(dir: &tramp::RemotePath, e: &tramp::RemoteEntry) -> dired::Entry {
    dired::Entry {
        name: OsString::from(&e.name),
        path: PathBuf::from(dir.join(&e.name).to_string()),
        is_dir: e.is_dir,
        is_symlink: e.is_symlink,
        // ponytail: the remote `ls` prints one record per entry with no room
        // for a link target, so the `-> target` column is blank over ssh. The
        // upgrade is a `readlink` in `list_script`, which costs a field.
        link_target: None,
        len: e.len,
        modified: e.modified,
        // The owner's write bit, which is what the local side's `readonly`
        // means too — `Permissions::readonly` is `mode & 0o222 == 0`, and the
        // login we are connected as is nearly always the owner.
        readonly: e.mode & 0o200 == 0,
        mode: e.mode,
    }
}

/// `..` (and `.`), which the remote listing never sends: [`tramp::list`] drops
/// them, exactly as `read_dir` does, and dired synthesises both.
fn dot_entry(dir: &tramp::RemotePath, name: &str) -> dired::Entry {
    let at = match name {
        ".." => dir.parent().unwrap_or_else(|| dir.clone()),
        _ => dir.clone(),
    };
    dired::Entry {
        name: OsString::from(name),
        path: PathBuf::from(at.to_string()),
        is_dir: true,
        is_symlink: false,
        link_target: None,
        len: 0,
        // Nothing was stat'd, and a round trip for two rows that render as
        // navigation is not worth it. `dired::render` prints a blank date.
        modified: None,
        readonly: false,
        mode: 0,
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

    /// `+` and `C-c n` ask for a name and create it *here*. Both used to open a
    /// completing file prompt, which is why neither worked: the candidates were
    /// paths — listed from the process's directory, not the one on screen — and
    /// `Prompt::value` prefers the highlighted candidate to the text, so Enter
    /// submitted a path that `zemacs_dired::child` refuses for having a
    /// separator in it.
    #[test]
    fn creating_asks_for_a_name_and_puts_it_in_the_listing_directory() {
        let (mut dired, mut editor, dir) = listing_of_three("zemacs_dired_create");

        for (verb, name) in [("mkdir", "sub"), ("create-file", "notes.txt")] {
            dired.run_confirmed(&mut editor, verb);
            let p = editor.prompt.as_ref().expect("{verb} opens a prompt");
            assert!(p.bare, "{verb} asks for a name, so nothing is completed");
            assert!(dired.awaiting_input(), "{verb}'s answer belongs to dired");
            // No candidates, so what Enter submits is what was typed — the
            // whole of the fix, and the thing `value()` used to override.
            assert_eq!(p.value(), "");

            dired.supply(&mut editor, name);
            assert!(dir.join(name).exists(), "{verb} creates {name} in {dir:?}");
            assert!(!dired.awaiting_input(), "{verb} is done");
        }
        assert!(dir.join("sub").is_dir(), "+ makes a directory, not a file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `(dired "/some/dir")`, which used to report "unknown dired verb" — the
    /// command carries one string and every verb acts on the directory dired is
    /// already in, so this is the only way Lisp can name one.
    #[test]
    fn a_directory_where_a_verb_goes_lists_that_directory() {
        let (mut dired, mut editor, dir) = listing_of_three("zemacs_dired_open_path");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let real = |p: &Path| p.canonicalize().unwrap();

        dired.run(&mut editor, sub.to_str().unwrap());
        assert_eq!(dired.dir().unwrap(), real(&sub), "{}", editor.status);
        // The buffer only gets a path from a re-list, so this is "it listed it".
        assert_eq!(editor.buffer.path.as_deref(), Some(real(&sub).as_path()));

        // ...and a verb is still a verb, from the same directory it was before.
        dired.run(&mut editor, "up");
        assert_eq!(dired.dir().unwrap(), real(&dir));

        // A string that is neither says so, rather than blaming the verb list
        // for a path that is simply not there.
        dired.run(&mut editor, "/nope/not/here");
        assert!(editor.status.contains("no such verb or directory"), "{}", editor.status);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- remote listings --------------------------------------------------

    fn remote_dired(dir: &str, names: &[(&str, bool)]) -> (Dired, Editor) {
        let remote = tramp::parse(dir).expect("a remote name");
        let mut dired = Dired {
            remote: Some(remote.clone()),
            dir: Some(PathBuf::from(remote.to_string())),
            remote_entries: names
                .iter()
                .map(|(name, is_dir)| tramp::RemoteEntry {
                    name: (*name).into(),
                    is_dir: *is_dir,
                    is_symlink: false,
                    len: 12,
                    mode: 0o644,
                    modified: None,
                })
                .collect(),
            ..Default::default()
        };
        let mut editor = Editor::default();
        dired.refresh(&mut editor).unwrap();
        (dired, editor)
    }

    /// A listing that arrived over ssh renders like any other, and — the part
    /// that matters — nothing in it is ever handed to this machine's
    /// filesystem. Every path it produces is a tramp name that parses straight
    /// back into the host it came from, which is what makes `RET` work.
    #[test]
    fn a_remote_listing_renders_and_its_paths_stay_remote() {
        let (dired, editor) = remote_dired(
            "/ssh:user@host#22:/etc",
            &[("nginx.conf", false), (".hidden", false), ("ssl", true)],
        );

        let text = editor.buffer.text.to_string();
        assert!(text.contains("nginx.conf"), "{text}");
        assert!(text.contains("ssl"), "{text}");
        assert!(!text.contains(".hidden"), "dotfiles are hidden by default:\n{text}");

        let listing = dired.listing().unwrap();
        // `..` is synthesised here exactly as it is locally, and it is the
        // *remote* parent — `Path::parent` would have said `/ssh:user@host#22:`,
        // which tramp reads as the login's home directory.
        assert_eq!(listing.entries[0].name, OsString::from(".."));
        assert_eq!(
            listing.entries[0].path,
            Path::new("/ssh:user@host#22:/")
        );
        // Directories before files, then by name — `dired::sort_entries`, the
        // same one `dired::list` uses.
        let order: Vec<String> = listing
            .entries
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(order, ["..", "ssl", "nginx.conf"]);

        for entry in &listing.entries {
            let name = entry.path.to_string_lossy().into_owned();
            let back = tramp::parse(&name).unwrap_or_else(|| panic!("{name} went local"));
            assert_eq!(back.host, "host");
            assert_eq!(back.user.as_deref(), Some("user"));
            assert_eq!(back.port, Some(22));
        }
        assert_eq!(
            listing.entries[2].path,
            Path::new("/ssh:user@host#22:/etc/nginx.conf"),
            "what RET hands to find-file"
        );
    }

    /// Toggling dotfiles, marking and sorting must all be free over a network:
    /// the entries are already here, so none of them may ask for them again.
    #[test]
    fn re_rendering_a_remote_listing_costs_no_round_trip() {
        let (mut dired, mut editor) =
            remote_dired("/ssh:host:/etc", &[("a.conf", false), (".hidden", false)]);
        dired.want_list = None;

        dired.run(&mut editor, "toggle-hidden");
        assert!(editor.buffer.text.to_string().contains(".hidden"));
        dired.run(&mut editor, "mark");
        assert!(dired.want_list.is_none(), "a mark asked the host to list again");

        // `g` is the one verb that does re-read, because that is what it is for.
        dired.run(&mut editor, "refresh");
        assert_eq!(dired.want_list.take().map(|p| p.to_string()).as_deref(), Some("/ssh:host:/etc"));
    }

    /// Walking the remote tree, and the two places `Path` would get it wrong.
    #[test]
    fn entering_and_leaving_a_remote_directory_asks_for_the_right_one() {
        let (mut dired, mut editor) = remote_dired("/ssh:host:/etc", &[("ssl", true)]);

        // `..` from `/etc` is `/`, not the login's home directory — which is
        // what `Path::parent` on `/ssh:host:/etc` would have produced.
        dired.run(&mut editor, "up");
        assert_eq!(dired.want_list.take().map(|p| p.to_string()).as_deref(), Some("/ssh:host:/"));
        assert_eq!(dired.dir().unwrap(), Path::new("/ssh:host:/"));
        // ...and the entries of the directory we left do not render under it.
        assert!(dired.remote_entries.is_empty());

        // A local dired is untouched by any of this.
        let here = std::env::temp_dir();
        dired.run(&mut editor, here.to_str().unwrap());
        assert!(dired.remote.is_none(), "a local directory went remote");
        assert!(dired.want_list.is_none(), "a local directory asked ssh to list it");
        assert!(
            editor.buffer.text.to_string().contains("entries"),
            "a local listing was rendered on the spot:\n{}",
            editor.buffer.text
        );
    }

    /// Everything that destroys or creates is refused, loudly and by name.
    /// Without the guard these reach `zemacs_dired` with a `PathBuf` spelled
    /// `/ssh:host:/etc/x`, which is a perfectly good local name for a file that
    /// is not there — so the error would be "No such file", about this machine.
    #[test]
    fn a_remote_listing_refuses_the_verbs_that_would_act_on_the_wrong_machine() {
        let (mut dired, mut editor) = remote_dired("/ssh:host:/etc", &[("a.conf", false)]);
        for verb in ["rename", "copy", "delete", "execute", "mkdir", "create-file"] {
            dired.run(&mut editor, verb);
            assert!(
                editor.status.contains(verb) && editor.status.contains("remote"),
                "{verb}: {}",
                editor.status
            );
            assert!(!dired.awaiting_input(), "{verb} left a prompt armed");
        }
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
