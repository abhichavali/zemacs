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

use std::collections::{HashMap, HashSet};
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
    /// Remote *writes* waiting to be sent, oldest first — the same ask as
    /// [`Dired::want_list`], for everything that is not a listing.
    ///
    /// A `Vec` rather than an `Option` because `x` over four flagged files is
    /// four `rm`s: a [`tramp::Op`] names one path. The worker is one thread and
    /// answers in the order it was asked, so a `want_list` drained behind these
    /// is the directory *after* the writes rather than a race against them.
    pub want_op: Vec<tramp::Op>,
    /// The delete in flight: which answers are still owed, and how it has gone
    /// so far.
    ///
    /// [`Self::remove_all`] used to count inside its own loop, because the loop
    /// and the round trips were the same thing. Over the worker the answers
    /// land a frame apart, so the count has to outlive it — and "deleted 3,
    /// failed 1" stays one sentence rather than becoming four.
    deleting: Option<Deleting>,
}

/// How a delete is going. One per [`Dired::remove_all`], local or remote.
#[derive(Default)]
struct Deleting {
    /// The files still out, by remote name. Empty means the sentence can be
    /// written — and always empty for a local delete, which is finished by the
    /// time its loop is.
    ///
    /// A set rather than a count, because it is also how an answer is
    /// recognised as *this batch's*: every one of dired's verbs comes back
    /// through [`Dired::did`], so a rename that happened to be queued beside
    /// the deletes must not be counted as one of them.
    owed: HashSet<String>,
    gone: usize,
    failed: Vec<String>,
}

impl Deleting {
    /// The one line the user reads. It names every failure rather than counting
    /// them, because "failed 1" without saying which file is a message you then
    /// have to go and check by hand.
    fn report(&self) -> String {
        match self.failed.as_slice() {
            [] => format!("deleted {}", self.gone),
            errs => format!(
                "deleted {}, failed {}: {}",
                self.gone,
                errs.len(),
                errs.join("; ")
            ),
        }
    }
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
        // The two-name verbs fork on the *names*, the two creating ones on the
        // listing — the same question, asked from the only end each of them
        // has. A rename can be given a destination anywhere, and [`pair`] is
        // what refuses one that is on another machine; `+` and `C-c n` create
        // in the directory on screen and nowhere else, which is [`one_name`].
        match pending {
            // A bare name stays in this directory; a path with a separator is
            // taken as given, so you can move a file elsewhere.
            Pending::Rename(from) => {
                let to = self.destination(&dir, answer);
                match pair("rename", &from, &to)? {
                    Some((from, to)) => self.want_op.push(tramp::Op::Rename { from, to }),
                    None => dired::rename(&from, &to)?,
                }
            }
            Pending::Copy(from) => {
                let to = self.destination(&dir, answer);
                match pair("copy", &from, &to)? {
                    Some((from, to)) => self.want_op.push(tramp::Op::Copy { from, to }),
                    None => dired::copy(&from, &to)?,
                }
            }
            Pending::Mkdir => match self.remote.clone() {
                Some(remote) => self
                    .want_op
                    .push(tramp::Op::Mkdir(remote.join(one_name(answer)?))),
                None => {
                    dired::create_dir(&dir, answer)?;
                }
            },
            Pending::CreateFile => match self.remote.clone() {
                Some(remote) => self
                    .want_op
                    .push(tramp::Op::CreateFile(remote.join(one_name(answer)?))),
                None => {
                    dired::create_file(&dir, answer)?;
                }
            },
        }
        self.reread();
        self.refresh(editor)
    }

    /// Ask for the directory again after a verb changed it.
    ///
    /// Nothing at all locally, where [`Self::refresh`] re-reads by itself. Over
    /// ssh the entries in hand have just become a lie, and `refresh`'s every
    /// other caller is a mark or a sort that must not pay for a round trip —
    /// see [`Dired::remote_entries`] — so the verbs that change the directory
    /// ask here, and only they.
    ///
    /// The same `want_list` a move to another directory sets, and it is queued
    /// *behind* the writes in [`Dired::want_op`] rather than racing them: one
    /// worker thread, answers in order, so what comes back is the directory as
    /// the writes left it. The listing on screen is the pre-verb one until then
    /// — a few frames of a stale row, against a minute of a frozen editor.
    fn reread(&mut self) {
        self.want_list = self.remote.clone();
    }

    /// Where a `rename`/`copy` answer points.
    ///
    /// [`resolve`] for a local listing. A bare name in a remote one has to go
    /// through [`tramp::RemotePath::join`] instead, because `Path::join` on
    /// `/ssh:host:` — which is the login's *home* — produces `/ssh:host:/x`,
    /// the remote root, and that is a different directory entirely.
    fn destination(&self, dir: &Path, answer: &str) -> PathBuf {
        match &self.remote {
            Some(remote) if bare(answer) => PathBuf::from(remote.join(answer).to_string()),
            _ => resolve(dir, answer),
        }
    }

    fn try_run(&mut self, editor: &mut Editor, verb: &str) -> anyhow::Result<()> {
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
                    // Over ssh the candidates are this machine's files, and
                    // `Prompt::value` prefers the highlighted one — so a remote
                    // rename would submit a local path and be refused for
                    // crossing machines. `bare` for the same reason `+` is:
                    // completion that cannot see the directory is worse than
                    // none. `docs/boundary.org` records the missing half.
                    p.bare = self.remote.is_some();
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
    ///
    /// A local delete finishes its count here, because the loop and the work
    /// are the same thing. A remote one only *queues*, and [`Self::did`] closes
    /// the same [`Deleting`] an answer at a time. Both write one sentence out of
    /// one tally, which is what keeps "deleted 3, failed 1" a property of the
    /// verb rather than of which machine the files happened to be on.
    fn remove_all(&mut self, editor: &mut Editor, doomed: &[PathBuf]) -> anyhow::Result<()> {
        // Merged into whatever is still in flight rather than replacing it: `D`
        // twice before the first answer lands is two batches and one sentence,
        // and a tally that had been overwritten would count the first batch's
        // answers against the second.
        let mut tally = self.deleting.take().unwrap_or_default();
        for path in doomed {
            // The fork, and it belongs *here* rather than in the two verbs:
            // `D` and `x` both arrive through this loop, so one `match` serves
            // both and a third door cannot grow its own quieter answer. It asks
            // the *path* rather than `self.remote` because the path is what the
            // deletion acts on — see [`machine`]. Recursive either way, which is
            // what `zemacs_dired::delete` is for a directory.
            match machine(path) {
                Some(path) => {
                    tally.owed.insert(path.to_string());
                    self.want_op.push(tramp::Op::Delete {
                        path,
                        recursive: true,
                    });
                }
                None => match dired::delete(path) {
                    Ok(()) => {
                        tally.gone += 1;
                        if let Some(name) = path.file_name() {
                            self.marks.remove(name);
                        }
                    }
                    Err(e) => tally.failed.push(format!("{}: {e}", path.display())),
                },
            }
        }
        self.reread();
        self.refresh(editor)?;
        match tally.owed.len() {
            // Nothing went over the network, so the sentence is finished here
            // exactly as it always was.
            0 => editor.apply(EditorCommand::Message(tally.report())),
            // ...and when it did, say so rather than saying nothing for a round
            // trip. `go` sets the same expectation when it moves directory.
            owed => {
                editor.apply(EditorCommand::Message(format!("deleting {owed}…")));
                self.deleting = Some(tally);
            }
        }
        Ok(())
    }

    /// One remote operation came back.
    ///
    /// Only a delete is *counted*, because it is the one verb that acts on more
    /// than one file and its answers land a frame apart — so the tally
    /// [`Self::remove_all`] started is closed here rather than there.
    /// Everything else is a single operation whose failure is its own sentence,
    /// in the same words `main.rs` gives a failed read.
    ///
    /// Which of the two an answer is turns on the *path* rather than on the
    /// order they land in: every verb dired queues comes back through here, and
    /// a rename that happened to be in flight beside a batch of deletes is not
    /// one of the files that batch is counting.
    pub fn did(
        &mut self,
        editor: &mut Editor,
        path: &tramp::RemotePath,
        reply: tramp::Result<tramp::Reply>,
    ) {
        let mine = self
            .deleting
            .as_mut()
            .is_some_and(|tally| tally.owed.remove(&path.to_string()));
        let Some(tally) = self.deleting.as_mut().filter(|_| mine) else {
            if let Err(e) = reply {
                editor.apply(EditorCommand::Message(format!("{path}: {e}")));
            }
            return;
        };
        match reply {
            Ok(_) => {
                tally.gone += 1;
                // Marks are keyed by *name* — see the field — and a remote
                // name's last component is the entry's, exactly as it is
                // locally. A file that is still there keeps its mark, so `D`
                // again retries the ones that stuck and only those.
                let full = path.to_string();
                if let Some(name) = Path::new(&full).file_name().map(OsString::from) {
                    self.marks.remove(&name);
                }
            }
            Err(e) => tally.failed.push(format!("{path}: {e}")),
        }
        // The last answer of the batch writes the sentence — however many round
        // trips it took, and whichever of them failed.
        if let Some(tally) = self.deleting.take_if(|tally| tally.owed.is_empty()) {
            editor.apply(EditorCommand::Message(tally.report()));
        }
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
            Face::Error => HlKind::Error,
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
    if bare(answer) {
        dir.join(answer)
    } else {
        PathBuf::from(answer)
    }
}

/// One component and no separator: a *name*, which stays in the directory on
/// screen, as opposed to a path, which says where to go.
fn bare(answer: &str) -> bool {
    !answer.starts_with('/') && Path::new(answer).components().count() <= 1
}

/// The machine a dired path names, or `None` for a file on this one.
///
/// Every verb asks this rather than `self.remote`, because a path is what an
/// operation acts on and the listing is only where the path came from: a rename
/// out of a remote directory can be given a local destination, and it has to be
/// told so rather than being run against the wrong filesystem.
fn machine(path: &Path) -> Option<tramp::RemotePath> {
    tramp::parse(&path.to_string_lossy())
}

/// Both ends of `rename`/`copy` as remote names, or `None` when both are here.
///
/// A mixed pair is refused rather than guessed at: moving a file between two
/// machines is a read plus a write plus a delete, which is a different feature
/// with its own half-finished state to report. Named by verb, so the message
/// says which key the user pressed.
fn pair(
    verb: &str,
    from: &Path,
    to: &Path,
) -> anyhow::Result<Option<(tramp::RemotePath, tramp::RemotePath)>> {
    match (machine(from), machine(to)) {
        (None, None) => Ok(None),
        (Some(from), Some(to)) => Ok(Some((from, to))),
        _ => anyhow::bail!(
            "cannot {verb} {} to {}: different machines",
            from.display(),
            to.display()
        ),
    }
}

/// A *name*, not a path: one component, so that a prompt answered
/// `../../.ssh/authorized_keys` cannot create anything outside the directory on
/// screen. `zemacs_dired`'s `child` says this for the local half and is private
/// to that crate, and the remote half hands `tramp` a whole path — so it has to
/// be said again here rather than inherited.
fn one_name(answer: &str) -> anyhow::Result<&str> {
    if answer == "."
        || answer == ".."
        || answer.contains('/')
        || answer.contains('\\')
        || answer.contains('\0')
    {
        anyhow::bail!("{answer:?} is not a file name");
    }
    Ok(answer)
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
        let dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
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
        onto_a_file(&dired, &mut editor);
        (dired, editor, dir)
    }

    /// Park the cursor on the first real entry — past `.` and `..`, which every
    /// verb refuses and which would otherwise make a fixture assert nothing.
    fn onto_a_file(dired: &Dired, editor: &mut Editor) {
        let entries = &dired.listing.as_ref().unwrap().entries;
        let line = dired
            .lines
            .iter()
            .position(|l| match l {
                dired::Line::Entry(i) => !entries[*i].is_dot(),
                _ => false,
            })
            .expect("the listing has a real entry");
        editor.buffer.cursor = editor.buffer.line_start(line);
    }

    /// The rule `D` lives or dies by. Deleting is not undoable here, so
    /// "which files" has to be exactly Emacs' answer and nothing looser.
    #[test]
    fn delete_takes_the_marks_or_the_line_and_asks_before_either() {
        let (mut dired, mut editor, dir) = listing_of_three("zemacs_dired_delete_one");

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

        // And the far side of `yes` really does delete, on this machine —
        // `remove_all` is the funnel a remote listing now shares, so this is
        // also the assertion that the fork left the local half alone.
        dired.run_confirmed(&mut editor, "delete");
        assert!(!dir.join("a.txt").exists() && !dir.join("c.txt").exists());
        assert!(dir.join("b.txt").exists(), "only the marked ones went");

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

    /// What the app would put on the wire this frame, in the order it would:
    /// the writes, then the re-read behind them. Exactly [`crate::Remote`]'s
    /// `take_from`, minus the worker — a real round trip needs a host, and what
    /// these tests are about is settled before ssh would ever be spawned:
    /// which machine a verb chose, and whether it asked the user first.
    fn sent(dired: &mut Dired) -> Vec<tramp::Op> {
        let mut ops: Vec<tramp::Op> = dired.want_op.drain(..).collect();
        ops.extend(dired.want_list.take().map(tramp::Op::List));
        ops
    }

    /// Answer everything that went out, the way an emptied directory would and
    /// in the order the worker would: one thread, so replies come back as
    /// asked. This is the half that used to happen inside the verb, on this
    /// thread, and now lands frames later.
    fn answered(dired: &mut Dired, editor: &mut Editor, ops: &[tramp::Op]) {
        for op in ops {
            match op {
                tramp::Op::List(dir) => dired.listed(editor, dir, Vec::new()),
                _ => dired.did(editor, &crate::acted_on(op), Ok(tramp::Reply::Done)),
            }
        }
    }

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
        onto_a_file(&dired, &mut editor);
        let _ = sent(&mut dired);
        (dired, editor)
    }

    /// One remote listing, one verb, one answer to the prompt it opened, and
    /// everything that went to the host. A verb per listing, because the
    /// re-read that follows one empties the listing it ran in.
    fn verb_answered(dir: &str, verb: &str, answer: &str) -> (Vec<tramp::Op>, Editor) {
        let (mut dired, mut editor) = remote_dired(dir, &[("a.conf", false)]);
        dired.run(&mut editor, verb);
        assert!(dired.awaiting_input(), "{verb} opened no prompt");
        dired.supply(&mut editor, answer);
        assert!(!dired.awaiting_input(), "{verb} is done");
        (sent(&mut dired), editor)
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

    /// `D` on a remote listing, and the half worth pinning down: it comes
    /// through the *same* funnel as the local one, so it is asked about before
    /// anything reaches the host. A remote delete that skipped the question
    /// because it took a different door is the data-loss shape this file is
    /// built to prevent, and it is one this tree has been bitten by before.
    #[test]
    fn a_remote_delete_asks_first_and_then_goes_to_the_host() {
        let (mut dired, mut editor) = remote_dired("/ssh:host:/etc", &[("a.conf", false)]);

        dired.run(&mut editor, "delete");
        assert!(sent(&mut dired).is_empty(), "it deleted before the user agreed");
        assert!(editor.pending_confirm.is_some(), "nothing was parked");
        let label = editor.prompt.as_ref().expect("a question").label.clone();
        assert!(label.contains("a.conf"), "asked by name: {label}");

        dired.run_confirmed(&mut editor, "delete");
        let ops = sent(&mut dired);
        match &ops[..] {
            [tramp::Op::Delete { path, recursive }, tramp::Op::List(dir)] => {
                assert_eq!(path.to_string(), "/ssh:host:/etc/a.conf");
                // `zemacs_dired::delete` is recursive locally, and a directory
                // left behind because `rm` had no `-r` is a worse surprise.
                assert!(*recursive);
                // ...and the entries in hand are a lie the moment it lands.
                assert_eq!(dir.to_string(), "/ssh:host:/etc");
            }
            other => panic!("{other:?}"),
        }
        // Nothing is *reported* until the host has answered, which is the whole
        // of the change: the editor stays alive across the round trip, so a
        // count claimed before it landed would be a guess.
        assert!(!editor.status.contains("deleted 1"), "{}", editor.status);
        answered(&mut dired, &mut editor, &ops);
        assert!(editor.status.contains("deleted 1"), "{}", editor.status);
    }

    /// `x` is the other door into `remove_all`, and it has to be guarded by the
    /// same check — which is the whole reason the check is in `run`.
    #[test]
    fn a_remote_expunge_asks_once_for_all_of_them() {
        let (mut dired, mut editor) =
            remote_dired("/ssh:host:/etc", &[("a.conf", false), ("b.conf", false)]);
        for name in ["a.conf", "b.conf"] {
            dired.marks.insert(name.into(), dired::MARK_DELETE);
        }

        dired.run(&mut editor, "execute");
        assert!(sent(&mut dired).is_empty(), "`x` deleted before asking");
        assert_eq!(
            dired.confirm_question(&editor, "execute").as_deref(),
            Some("Delete 2 files?"),
        );

        dired.run_confirmed(&mut editor, "execute");
        // Two deletes and one re-read: one round trip per file, which is the
        // ceiling `want_op` names — but all three are in flight at once now,
        // so it is the host's time rather than the editor's.
        let ops = sent(&mut dired);
        assert_eq!(ops.len(), 3, "{ops:?}");
        assert!(matches!(ops[2], tramp::Op::List(_)), "{ops:?}");

        // Two answers, one sentence: the count outlives the loop that queued
        // them, so `x` over marked files still reports once and not per file.
        answered(&mut dired, &mut editor, &ops[..1]);
        assert!(!editor.status.contains("deleted"), "reported half way: {}", editor.status);
        answered(&mut dired, &mut editor, &ops[1..]);
        assert!(editor.status.contains("deleted 2"), "{}", editor.status);
    }

    /// ...and the other half of that sentence: a host that refuses one of them
    /// is still one message naming which. The synchronous version got this out
    /// of a single `Result`; spread over frames it has to be counted, and a
    /// silent degrade to "some deletes failed" is what that must not become.
    #[test]
    fn a_delete_that_partly_fails_names_what_stayed() {
        let (mut dired, mut editor) = remote_dired(
            "/ssh:host:/etc",
            &[("a.conf", false), ("b.conf", false), ("c.conf", false)],
        );
        for name in ["a.conf", "b.conf", "c.conf"] {
            dired.marks.insert(name.into(), dired::MARK_SELECT);
        }

        dired.run_confirmed(&mut editor, "delete");
        let ops = sent(&mut dired);
        assert_eq!(ops.len(), 4, "three deletes and a re-read: {ops:?}");

        for (i, op) in ops.iter().enumerate() {
            match op {
                tramp::Op::List(dir) => dired.listed(&mut editor, dir, Vec::new()),
                // The middle one is refused, the way a directory you cannot
                // write to refuses one file and not the others.
                _ => dired.did(
                    &mut editor,
                    &crate::acted_on(op),
                    match i {
                        1 => Err(tramp::Error::Refused("Permission denied".into())),
                        _ => Ok(tramp::Reply::Done),
                    },
                ),
            }
        }

        assert!(editor.status.contains("deleted 2"), "{}", editor.status);
        assert!(editor.status.contains("failed 1"), "{}", editor.status);
        assert!(editor.status.contains("b.conf"), "which one stayed: {}", editor.status);
        // ...and the one that stayed keeps its mark, so `D` again retries it
        // and only it.
        assert_eq!(dired.marks.len(), 1, "{:?}", dired.marks);
        assert!(dired.marks.contains_key(&OsString::from("b.conf")));
    }

    /// The bug this whole route exists to close. Every write verb reaches dired
    /// on the far side of a confirmation, and the drain used to live under
    /// `EditorCommand::Dired` — the one arm none of them come through — so the
    /// delete sat in the field until some later, unrelated verb pushed it out.
    #[test]
    fn a_verb_confirmed_still_reaches_the_worker() {
        // A host nothing listens on, so the worker this spawns fails at
        // `connect(2)` rather than acting on anything — the same trick the
        // `Remote` tests in `main.rs` use.
        let (mut dired, mut editor) =
            remote_dired("/ssh:0.0.0.0#1:/nowhere", &[("a.conf", false)]);

        dired.run(&mut editor, "delete");
        assert!(dired.want_op.is_empty(), "sent before the user agreed");

        // The two halves of a frame in which a confirmation was answered: what
        // `EditorCommand::Confirmed(Dired(..))` does, then what `housekeep`
        // does afterwards — and the second one is unconditional, which is the
        // property being asserted.
        dired.run_confirmed(&mut editor, "delete");
        assert_eq!(dired.want_op.len(), 1, "the verb queued nothing");
        let mut remote = crate::Remote::default();
        remote.take_from(&mut dired);

        assert!(dired.want_op.is_empty(), "queued from `Confirmed` and never sent");
        assert!(dired.want_list.is_none(), "the re-read behind it was never sent");
        assert_eq!(remote.jobs.len(), 2, "the delete and the re-read");
        assert!(
            remote.jobs.values().any(|job| {
                matches!(job, crate::Job::Dired(p) if p.path == "/nowhere/a.conf")
            }),
            "the delete did not reach the worker: {:?}",
            remote.jobs.len(),
        );
    }

    /// The four verbs that ask for a name, each reaching the host the listing
    /// came from. Where the name *lands* matters as much as the operation:
    /// `Path::join` on `/ssh:host:` — the login's home — would have put it at
    /// the remote root instead, which is a different directory entirely.
    #[test]
    fn the_naming_verbs_reach_the_host_with_the_name_that_was_typed() {
        let (ops, _) = verb_answered("/ssh:host:", "rename", "b.conf");
        match &ops[..] {
            [tramp::Op::Rename { from, to }, tramp::Op::List(_)] => {
                assert_eq!(from.to_string(), "/ssh:host:~/a.conf");
                assert_eq!(to.to_string(), "/ssh:host:~/b.conf");
            }
            other => panic!("rename: {other:?}"),
        }

        // The login travels with the path, since it is half of which host it is.
        let (ops, _) = verb_answered("/ssh:user@host#22:/etc", "copy", "b.conf");
        match &ops[..] {
            [tramp::Op::Copy { from, to }, tramp::Op::List(_)] => {
                assert_eq!(from.to_string(), "/ssh:user@host#22:/etc/a.conf");
                assert_eq!(to.to_string(), "/ssh:user@host#22:/etc/b.conf");
            }
            other => panic!("copy: {other:?}"),
        }

        let (ops, _) = verb_answered("/ssh:host:", "mkdir", "sub");
        match &ops[..] {
            [tramp::Op::Mkdir(p), tramp::Op::List(_)] => {
                assert_eq!(p.to_string(), "/ssh:host:~/sub");
            }
            other => panic!("mkdir: {other:?}"),
        }

        let (ops, _) = verb_answered("/ssh:host:/etc", "create-file", "notes.txt");
        match &ops[..] {
            // `CreateFile`, never a `Write` of nothing: one would truncate a
            // file that is already there, which is what `C-c n` must not do.
            [tramp::Op::CreateFile(p), tramp::Op::List(_)] => {
                assert_eq!(p.to_string(), "/ssh:host:/etc/notes.txt");
            }
            other => panic!("create-file: {other:?}"),
        }
    }

    /// The two answers a remote prompt has to refuse: one that would act on
    /// this machine instead of that one, and one that would leave the directory
    /// on screen. Neither may reach the host at all.
    #[test]
    fn a_remote_prompt_refuses_the_answers_that_leave_the_directory() {
        let (mut dired, mut editor) = remote_dired("/ssh:host:/etc", &[("a.conf", false)]);

        dired.run(&mut editor, "rename");
        dired.supply(&mut editor, "/tmp/here.conf");
        assert!(
            editor.status.contains("different machines"),
            "{}",
            editor.status
        );
        assert!(sent(&mut dired).is_empty(), "a rename crossed machines");

        dired.run(&mut editor, "mkdir");
        dired.supply(&mut editor, "../evil");
        assert!(
            editor.status.contains("not a file name"),
            "{}",
            editor.status
        );
        assert!(
            sent(&mut dired).is_empty(),
            "a name with a separator reached the host"
        );
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
