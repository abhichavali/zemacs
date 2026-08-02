//! The terminal half that needs a process, mirroring [`crate::magit`] and
//! [`crate::dired`].
//!
//! `zemacs-term` knows PTYs and escape sequences; core knows the buffer kind and
//! the mode. This is the seam: it forks the child, feeds it keystrokes, and
//! flattens its grid back into buffer text so that the buffer switcher, the
//! modeline and `buffer-string` all work on a terminal without a special case.
//! The *colour* does not survive that flattening, so the renderer reads the
//! grid directly — see [`Term::screen`].
//!
//! # Many sessions, not one
//!
//! This used to hold a single [`Terminal`], the way magit holds a single status
//! buffer, and that was fine while the only thing in a terminal was a shell you
//! popped open and closed. It is not fine for a coding agent: an agent session
//! is a *conversation* that runs for an hour, and opening a second one must not
//! replace the first any more than opening a second file does.
//!
//! So there is a [`Session`] per child, each with its own buffer, and the buffer
//! id is the key to everything — which grid a keystroke reaches, which pane
//! sizes which child, which buffer gets refreshed. "Acts like any other buffer"
//! falls straight out of that: `(buffer-list)` sees them, `switch-to-buffer`
//! reaches them, and killing one leaves the others running.

use std::path::PathBuf;

use zemacs_core::{BufferId, BufferKind, Editor, EditorCommand, Key, Mode};
use zemacs_term::{Command, Input, Mouse, Screen, Terminal};

/// Rows and columns to start with, before the renderer has measured a pane.
/// Replaced on the first frame; the child only sees the real size.
const INITIAL: (usize, usize) = (80, 24);

/// One live child and the buffer showing it.
struct Session {
    /// The handle everything routes by. A buffer that has been killed is how a
    /// session learns it is over — see [`Term::reap`].
    buffer: BufferId,
    /// The buffer's name, and the key `show_named` matches on. Unique across
    /// live sessions, which is what [`Term::unique_name`] is for.
    name: String,
    /// Where the child was started. Kept for `restart`: an agent that came back
    /// up in a different repository than it went down in would be reading the
    /// wrong tree, and `current_dir` is whatever the *editor* was launched from.
    cwd: Option<PathBuf>,
    inner: Terminal,
    /// The editor has the keyboard and the buffer holds the scrollback, so the
    /// child must not rewrite it out from under the cursor. Per session rather
    /// than per app: stepping out of one agent to yank a path must not stop the
    /// other one from printing.
    frozen: bool,
}

#[derive(Default)]
pub struct Term {
    sessions: Vec<Session>,
}

impl Term {
    /// True while any session is alive, so the app knows to pump them.
    pub fn is_live(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// Every session's buffer, so the app can measure the pane each one is
    /// shown in. Order is spawn order, which is also the order `next`/`prev`
    /// walk.
    pub fn buffers(&self) -> Vec<BufferId> {
        self.sessions.iter().map(|s| s.buffer).collect()
    }

    /// The session the keyboard belongs to: the one whose buffer is live.
    /// `None` whenever the user is looking at anything else, which is what
    /// makes every key and mouse route below a no-op outside a terminal.
    fn current(&self, editor: &Editor) -> Option<usize> {
        let id = editor.buffer.id;
        self.sessions.iter().position(|s| s.buffer == id)
    }

    /// Verbs arrive as strings from `EditorCommand::Term`, so a new one is a
    /// Lisp line rather than a Rust variant — see `runtime/modes/ai.lisp`,
    /// which is where the harness list actually lives.
    ///
    /// `run:NAME:PROGRAM ARG…` is the one that carries an argument. Colons
    /// separate the three fields and the command line is whatever is left, so a
    /// program path with a colon in it is unsupported and a flag with one is
    /// fine.
    pub fn run(&mut self, editor: &mut Editor, verb: &str) {
        match verb {
            // The old spelling, and still what `SPC o t` means: show the shell
            // session if there is one, start it if there is not.
            "open" => self.open(editor),
            // Always a fresh one, however many are already running.
            "new" => self.spawn(editor, "*terminal*", None, true),
            "close" => self.close(editor),
            "restart" => self.restart(editor),
            "next" => self.cycle(editor, 1),
            "prev" => self.cycle(editor, -1),
            // Leaving and re-entering the child. `terminal-normal` in core sets
            // the mode; these are what put the right *text* in the buffer for
            // it, which is the half core cannot do.
            "normal" => self.freeze(editor),
            "insert" => self.thaw(editor),
            // `run:` starts a session; `rerun:` replaces the one of that name.
            //
            // Two verbs because there are two intentions and they are opposites.
            // A harness is a *thing you start* — two agents side by side is the
            // point, so `run:claude` twice is two sessions. A program is a thing
            // you run *again*: edit, run, read, edit, and a session per press
            // would pile up dead children in the switcher within a minute, all
            // but the last finished and none named distinguishably.
            other => match (other.strip_prefix("run:"), other.strip_prefix("rerun:")) {
                (Some(rest), _) => self.run_harness(editor, rest, false),
                (_, Some(rest)) => self.run_harness(editor, rest, true),
                _ => editor.apply(EditorCommand::Message(format!(
                    "unknown terminal verb: {other}"
                ))),
            },
        }
    }

    /// `run:NAME:PROGRAM ARG…`.
    ///
    /// Splitting the command line on whitespace is deliberate and is the
    /// ceiling: resume flags are `-r`, `--resume`, `--continue`, and a session
    /// id is a UUID. ponytail: an argument with a space in it needs the verb to
    /// carry a *list* rather than a string, which means an `EditorCommand`
    /// shaped like `Term { verb, args }` — worth doing the first time a harness
    /// wants `--prompt "do the thing"`, and not before.
    fn run_harness(&mut self, editor: &mut Editor, rest: &str, reuse: bool) {
        let Some((name, line)) = rest.split_once(':') else {
            editor.apply(EditorCommand::Message(format!(
                "terminal: run needs NAME:COMMAND, got {rest:?}"
            )));
            return;
        };
        let mut words = line.split_whitespace().map(str::to_string);
        let Some(program) = words.next() else {
            editor.apply(EditorCommand::Message(
                "terminal: run needs a command to run".into(),
            ));
            return;
        };
        let command = Command::new(program, words.collect());
        let buffer = format!("*{name}*");

        // The command is rewritten before restarting rather than reusing the
        // stored one, because this is a *run*, not the resume `restart` was
        // written for: the script it points at may have been regenerated, and
        // re-running the previous command would silently execute the old one.
        if reuse {
            if let Some(i) = self.sessions.iter().position(|s| s.name == buffer) {
                self.show(editor, i);
                self.sessions[i].inner.set_command(command);
                self.restart(editor);
                return;
            }
        }
        self.spawn(editor, &buffer, Some(command), true);
    }

    fn open(&mut self, editor: &mut Editor) {
        // An existing shell session is shown rather than duplicated, which is
        // what `SPC o t` has always meant. `new` is how you ask for a second.
        if let Some(i) = self.sessions.iter().position(|s| s.inner.command().is_none()) {
            self.show(editor, i);
            return;
        }
        self.spawn(editor, "*terminal*", None, true);
    }

    /// Start a child, give it a buffer, and hand it the keyboard.
    ///
    /// `focus` is always true today; it is an argument because a background
    /// session is the obvious next thing to want and this is the one line that
    /// would change.
    fn spawn(&mut self, editor: &mut Editor, name: &str, command: Option<Command>, focus: bool) {
        // Start where the current file is, the way `M-x shell` does — a
        // terminal that opens in the wrong directory is a terminal you
        // immediately have to `cd` in, and an *agent* in the wrong directory
        // is one that reads the wrong repository.
        let cwd = editor
            .buffer
            .path
            .as_deref()
            .and_then(|p| if p.is_dir() { Some(p) } else { p.parent() })
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok());
        let inner = match Terminal::spawn_command(INITIAL.0, INITIAL.1, cwd.clone(), command) {
            Ok(term) => term,
            Err(e) => {
                // The message a missing harness produces. Loud and specific,
                // because the alternative is a buffer that appears and vanishes.
                editor.apply(EditorCommand::Message(format!("terminal: {e:#}")));
                return;
            }
        };
        let name = self.unique_name(name);
        let buffer = editor.show_named(BufferKind::Terminal, Some(&name), "");
        self.sessions.push(Session {
            buffer,
            name,
            cwd,
            inner,
            frozen: false,
        });
        if focus {
            editor.apply(EditorCommand::SetMode(Mode::Terminal));
        }
    }

    /// `*claude*`, then `*claude*<2>` — Emacs' own disambiguation, and the
    /// reason two sessions of the same harness are tellable apart in the
    /// switcher. Counted rather than remembered, so closing `<2>` frees the
    /// name again.
    fn unique_name(&self, base: &str) -> String {
        if !self.sessions.iter().any(|s| s.name == base) {
            return base.to_string();
        }
        (2..).map(|n| format!("{base}<{n}>")).find(|candidate| {
            !self.sessions.iter().any(|s| &s.name == candidate)
        })
        .unwrap_or_else(|| base.to_string())
    }

    /// Put session `i` on screen and give it the keyboard.
    fn show(&mut self, editor: &mut Editor, i: usize) {
        let Some(session) = self.sessions.get_mut(i) else {
            return;
        };
        // Whether the session is new or was parked in Normal mode, showing it
        // means the child has the keyboard again — without this the buffer
        // keeps showing a frozen scrollback and nothing typed ever appears.
        session.frozen = false;
        let (kind, name) = (BufferKind::Terminal, session.name.clone());
        // Empty text: `sync` puts the real grid in on the very next frame, and
        // writing the stale flattening here would flash the previous screenful.
        editor.show_named(kind, Some(&name), "");
        editor.apply(EditorCommand::SetMode(Mode::Terminal));
    }

    /// Walk to the next or previous session. Wraps, and with one session is a
    /// no-op rather than a re-entry that would unfreeze it.
    fn cycle(&mut self, editor: &mut Editor, by: isize) {
        if self.sessions.len() < 2 {
            return;
        }
        let n = self.sessions.len() as isize;
        // From outside any session, `next` means "the first one" — which is how
        // you get back into a terminal from a file without the switcher.
        let from = match self.current(editor) {
            Some(i) => i as isize,
            None => {
                self.show(editor, 0);
                return;
            }
        };
        self.show(editor, ((from + by).rem_euclid(n)) as usize);
    }

    /// Kill the live session: hang up on the child and take its buffer with it.
    ///
    /// Deliberately different from a child that exits on its own (see [`sync`]):
    /// closing is something you asked for, so the buffer goes, while a harness
    /// that died has usually just printed why and the buffer stays so you can
    /// read it.
    fn close(&mut self, editor: &mut Editor) {
        let Some(i) = self.current(editor) else {
            return;
        };
        // Dropping it is what shuts the reader thread down and hangs up on the
        // child; see `Terminal::drop`.
        let gone = self.sessions.remove(i);
        // The live buffer is index 0 in the switcher's order, and this session
        // is live by definition — `current` matched on `editor.buffer.id`.
        editor.apply(EditorCommand::SetMode(Mode::Normal));
        editor.kill_buffer(0);
        editor.apply(EditorCommand::Message(format!("closed {}", gone.name)));
    }

    /// Hang up and start the same command again in the same buffer.
    ///
    /// The buffer is reused rather than remade so window layout, position in
    /// the switcher and anything pointing at it all survive — restarting an
    /// agent that wedged should feel like reloading a file, not like closing
    /// and reopening one.
    fn restart(&mut self, editor: &mut Editor) {
        let Some(i) = self.current(editor) else {
            return;
        };
        let (name, cwd, command) = {
            let session = &self.sessions[i];
            (
                session.name.clone(),
                session.cwd.clone(),
                session.inner.command().cloned(),
            )
        };
        let (cols, rows) = self.sessions[i].inner.size();
        match Terminal::spawn_command(cols, rows, cwd, command) {
            Ok(fresh) => {
                self.sessions[i].inner = fresh;
                self.sessions[i].frozen = false;
                editor.show_named(BufferKind::Terminal, Some(&name), "");
                editor.apply(EditorCommand::SetMode(Mode::Terminal));
                editor.apply(EditorCommand::Message(format!("restarted {name}")));
            }
            // The old child is already gone at this point only if the spawn
            // succeeded, so a failure leaves the session exactly as it was.
            Err(e) => editor.apply(EditorCommand::Message(format!("terminal: {e:#}"))),
        }
    }

    /// Type something at the live session. Used to run a project's build
    /// command, which is then a command in a terminal like any other rather
    /// than a second process-spawning path whose output nobody can see.
    pub fn send(&self, editor: &Editor, bytes: Vec<u8>) {
        if let Some(i) = self.current(editor) {
            self.sessions[i].inner.send(bytes);
        }
    }

    /// Send a keystroke to the child. Returns false when the key is not one a
    /// terminal can carry, which is how the splits keep working inside it.
    pub fn key(&mut self, editor: &Editor, key: Key) -> bool {
        let Some(i) = self.current(editor) else {
            return false;
        };
        let input = match key {
            Key::Char(c) => Input::Char(c),
            Key::Ctrl(c) => Input::Ctrl(c),
            Key::Meta(c) => Input::Alt(c),
            Key::Enter => Input::Enter,
            Key::Tab => Input::Tab,
            Key::Backspace => Input::Backspace,
            Key::Esc => Input::Esc,
            Key::Up => Input::Up,
            Key::Down => Input::Down,
            Key::Left => Input::Left,
            Key::Right => Input::Right,
            // `M-<bs>` is `ESC DEL`, which readline reads as backward-kill-word
            // — so `⌘⌫` deletes the last word in the shell exactly as it does
            // in a buffer.
            Key::MetaBackspace => Input::Alt('\u{7f}'),
            // `C-M-x` and the two split keys belong to the editor. Leaving them
            // unhandled is what lets `C-<ret>` still split a window while a
            // child has the keyboard.
            Key::CtrlMeta(_) | Key::CtrlEnter | Key::CtrlMetaEnter => return false,
        };
        self.sessions[i].inner.input(input);
        true
    }

    /// A wheel notch over the terminal. `lines` is positive downward, the way
    /// the editor counts; the grid counts up its history, hence the negation.
    pub fn wheel(&self, editor: &Editor, lines: i32, col: usize, row: usize) {
        if let Some(i) = self.current(editor) {
            self.sessions[i].inner.wheel(-lines, col, row);
        }
    }

    /// A click or drag over the terminal, in cell coordinates. False when the
    /// child does not want mouse events, so the caller can fall back to
    /// whatever the editor would have done.
    pub fn mouse(&self, editor: &Editor, mouse: Mouse) -> bool {
        self.current(editor)
            .is_some_and(|i| self.sessions[i].inner.mouse(mouse))
    }

    /// Hand the keyboard back to the editor, with the whole scrollback in the
    /// buffer so the motions have something to move through.
    ///
    /// Live updates stop for *this* session while that is up: the child would
    /// otherwise rewrite the buffer under the cursor sixty times a second,
    /// which is what made every vim key look broken here. The other sessions
    /// carry on — an agent does not stop working because you looked away.
    pub fn freeze(&mut self, editor: &mut Editor) {
        let Some(i) = self.current(editor) else { return };
        self.sessions[i].frozen = true;
        let text = self.sessions[i].inner.history_text();
        let name = self.sessions[i].name.clone();
        editor.show_named(BufferKind::Terminal, Some(&name), &text);
        editor.apply(EditorCommand::SetMode(Mode::Normal));
        // Land at the bottom, where the prompt is — that is what was on screen
        // a moment ago, and starting at line 1 of a 10,000-line scrollback is
        // never what was meant.
        editor.buffer.move_to_line_col(editor.buffer.len_lines(), 0);
    }

    /// Give the keyboard back to the child.
    pub fn thaw(&mut self, editor: &mut Editor) {
        let Some(i) = self.current(editor) else { return };
        self.sessions[i].frozen = false;
        editor.apply(EditorCommand::SetMode(Mode::Terminal));
    }

    /// Resize every session to the pane it is shown in, drain each child's
    /// requests, and refresh the live one's buffer text.
    ///
    /// Called every frame. `poll` is the part that must not be skipped, and it
    /// must not be skipped *per session*: a program asking the terminal how big
    /// it is blocks until the answer is written back, so a background agent
    /// that nobody polls is a background agent that hangs.
    ///
    /// `sizes` is `(buffer, cols, rows)` per session, measured by the app —
    /// core owns no geometry and this crate owns no renderer.
    pub fn sync(&mut self, editor: &mut Editor, sizes: &[(BufferId, usize, usize)]) {
        self.reap(editor);

        let mut exited: Vec<(String, Option<i32>)> = Vec::new();
        self.sessions.retain_mut(|session| {
            if let Some((_, cols, rows)) = sizes.iter().find(|(id, ..)| *id == session.buffer) {
                session.inner.resize(*cols, *rows);
            }
            session.inner.poll();
            if session.inner.exited() {
                exited.push((session.name.clone(), session.inner.exit_status()));
                return false;
            }
            true
        });

        // Report and freeze *after* the retain, so the borrow of `self` is over
        // before the editor is touched.
        for (name, status) in exited {
            self.retire(editor, &name, status);
        }

        // Only the live session's buffer is refreshed. A parked one keeps the
        // last screenful it had, and catches up on the frame after you switch
        // to it — which is a frame, not a delay anyone can see, and it keeps
        // `show_named`'s revision bump off every background agent's output.
        let Some(i) = self.current(editor) else { return };
        if self.sessions[i].frozen {
            return;
        }
        let text = self.sessions[i]
            .inner
            .screen(fg(editor), bg(editor))
            .to_text();
        // Only when it changed: `show_named` bumps the revision, and doing that
        // every frame would have the syntax thread reparsing a terminal sixty
        // times a second.
        if editor.buffer.text != text {
            let name = self.sessions[i].name.clone();
            editor.show_named(BufferKind::Terminal, Some(&name), &text);
        }
        // A session running a harness is in `ai-mode`, a plain shell in
        // `terminal-mode` — the axis `(major-mode)` answers on, which is what a
        // config tests to tell "there is an agent in here" from "there is a
        // shell in here".
        //
        // Guarded on a change rather than assigned every frame, which is what
        // makes the hook fire exactly *once* per buffer entering the mode.
        // Core fires `<mode>-hook` from `SetMajorMode` and from opening a file;
        // neither happens here, because a generated buffer is never loaded and
        // routing this through `apply` would put a status line under every
        // frame of the agent's output.
        let want = if self.sessions[i].inner.command().is_some() {
            "ai-mode"
        } else {
            "terminal-mode"
        };
        if editor.buffer.major_mode != want {
            editor.buffer.major_mode = want.into();
            editor.pending_hooks.push(format!("{want}-hook"));
        }
    }

    /// A child that exited on its own. Its buffer keeps the last screenful —
    /// which is usually the error — and becomes an ordinary read-only buffer
    /// you can search and yank from until you kill it.
    fn retire(&mut self, editor: &mut Editor, name: &str, status: Option<i32>) {
        let how = match status {
            Some(0) | None => String::new(),
            // 127 is the shell's "command not found", and the one exit code
            // worth spelling out — it is what a harness that is not installed
            // produces when it is launched through a shell rather than directly.
            Some(127) => " — command not found".into(),
            Some(code) => format!(" — exit {code}"),
        };
        editor.apply(EditorCommand::Message(format!("{name} exited{how}")));
        if editor.buffer.given_name.as_deref() == Some(name) {
            editor.apply(EditorCommand::SetMode(Mode::Normal));
        }
    }

    /// Drop sessions whose buffer has been killed.
    ///
    /// `kill-buffer` is the switcher's, the Lisp API's and `SPC b k`'s way of
    /// getting rid of a buffer, and none of them knows a process is behind this
    /// one. Noticing here is what keeps "acts like any other buffer" true in
    /// the direction that matters: killing the buffer hangs up on the child
    /// rather than leaving an agent running with nobody reading it.
    fn reap(&mut self, editor: &Editor) {
        self.sessions
            .retain(|s| editor.buffer_by_id(s.buffer).is_some());
    }

    /// The live grid, for the renderer to draw in colour.
    ///
    /// `None` while frozen or outside a session, which is what makes the
    /// renderer fall back to the ordinary text path — and so the selection, the
    /// cursor and the line numbers all appear, which is the whole point of
    /// stepping out of the child.
    ///
    /// ponytail: *one* screen, so two terminal buffers visible in two panes at
    /// once both draw the live session's grid. The upgrade is the renderer
    /// taking a grid per buffer id instead of a single `Option<&Screen>`, which
    /// is a signature change in `zemacs-render`; until then a background
    /// session is correct in the switcher, in `buffer-string` and in its own
    /// pane the moment you focus it, and wrong only in a split showing two
    /// terminals simultaneously.
    pub fn screen(&self, editor: &Editor) -> Option<Screen> {
        let i = self.current(editor)?;
        let session = &self.sessions[i];
        (!session.frozen).then(|| session.inner.screen(fg(editor), bg(editor)))
    }

    /// What is under the pointer at `(col, row)`: the OSC 8 link the child put
    /// on that cell if there is one, otherwise the row as plain text.
    ///
    /// Two answers because there are two kinds of link. `cargo` and `ls
    /// --hyperlink` attach a URL to text that does not look like one — an error
    /// code, a filename — and that link is the whole point of the escape. Every
    /// other program just prints the URL, and then the text *is* the link.
    ///
    /// The row comes from the grid rather than from the buffer, whose copy has
    /// had its trailing blanks trimmed and whose rows are lines — so a column
    /// index means something here and nothing there.
    /// `(row text, the OSC 8 link on that cell)`. Both, because Lisp decides
    /// between them and there is no third call to make.
    pub fn click_context(
        &self,
        editor: &Editor,
        col: usize,
        row: usize,
    ) -> Option<(String, Option<String>)> {
        let screen = self.screen(editor)?;
        let i = self.current(editor)?;
        (row < screen.rows).then(|| {
            let text = (0..screen.cols)
                .filter_map(|col| screen.cell(row, col).map(|cell| cell.c))
                .collect();
            (text, self.sessions[i].inner.link_at(col, row))
        })
    }
}

/// The editor's colours, as the terminal wants them. Settings are `0.0..1.0`
/// floats and a terminal is bytes.
fn fg(editor: &Editor) -> [u8; 3] {
    to_bytes(editor.settings.foreground)
}

fn bg(editor: &Editor) -> [u8; 3] {
    to_bytes(editor.settings.background)
}

fn to_bytes(c: [f32; 3]) -> [u8; 3] {
    c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything below runs a real PTY, so it runs a program that is on every
    /// machine this builds on. None of the three harnesses is a build
    /// dependency and none of them is ever spawned by the suite.
    fn cat() -> Command {
        Command::new("cat", vec![])
    }

    #[test]
    fn editor_colors_convert_to_bytes() {
        assert_eq!(to_bytes([0.0, 0.5, 1.0]), [0, 128, 255]);
        // out-of-range values clamp rather than wrapping around
        assert_eq!(to_bytes([-1.0, 2.0, 0.0]), [0, 255, 0]);
    }

    /// The window splits have to keep working while a child has the keyboard,
    /// so they are the keys a terminal deliberately does not take.
    #[test]
    fn the_editors_own_keys_are_not_sent_to_the_shell() {
        let mut term = Term::default();
        let ed = Editor::new();
        // With no session running nothing is claimed at all.
        assert!(!term.key(&ed, Key::Char('x')));
        assert!(!term.key(&ed, Key::CtrlEnter));
        assert!(!term.key(&ed, Key::CtrlMetaEnter));
        assert!(!term.key(&ed, Key::CtrlMeta('j')));
    }

    /// The feature, in one test: two sessions, two buffers, two children, and
    /// neither one replaced by the other.
    #[test]
    fn two_sessions_are_two_buffers() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        term.run(&mut ed, "run:one:cat");
        term.run(&mut ed, "run:two:cat");

        assert_eq!(term.sessions.len(), 2);
        let names = ed.buffer_names();
        assert!(names.iter().any(|n| n == "*one*"), "{names:?}");
        assert!(names.iter().any(|n| n == "*two*"), "{names:?}");
        // The second is live, the first is parked and still running.
        assert_eq!(ed.buffer.name(), "*two*");
        assert_eq!(ed.mode, Mode::Terminal);
    }

    /// Two of the same harness are told apart the way Emacs tells two `mod.rs`
    /// apart, rather than one silently becoming the other.
    #[test]
    fn a_second_session_of_the_same_harness_gets_its_own_name() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        term.run(&mut ed, "run:claude:cat");
        term.run(&mut ed, "run:claude:cat");
        let names: Vec<&str> = term.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["*claude*", "*claude*<2>"]);
    }

    /// ...and `rerun:` is the opposite intention, which is why it is a second
    /// verb rather than a flag on the first.
    ///
    /// A harness is a thing you *start* — two agents side by side is the point.
    /// A program is a thing you run *again*: edit, run, read, edit. Spawning per
    /// press piled dead children into the switcher, all but the last finished
    /// and none of them named distinguishably, which is what made running a
    /// curriculum's code feel like it was leaking.
    #[test]
    fn rerunning_replaces_the_session_of_that_name_rather_than_stacking_one() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        term.run(&mut ed, "rerun:gaussian:cat");
        let first = term.sessions[0].buffer;
        term.run(&mut ed, "rerun:gaussian:cat");

        let names: Vec<&str> = term.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["*gaussian*"], "one session, not two");
        // The *buffer* survives, which is the point of restarting in place: the
        // window showing it, its position in the switcher and anything pointing
        // at it all still mean what they did.
        assert_eq!(term.sessions[0].buffer, first);

        // A different program is still its own session — reuse is by name, not
        // a global "one runner".
        term.run(&mut ed, "rerun:independence:cat");
        let names: Vec<&str> = term.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["*gaussian*", "*independence*"]);

        // And `run:` is untouched by any of it.
        term.run(&mut ed, "run:gaussian:cat");
        let names: Vec<&str> = term.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["*gaussian*", "*independence*", "*gaussian*<2>"]);
    }

    /// Keys, freezing and closing all address the session whose buffer is live
    /// — not "the terminal", of which there is no longer one.
    #[test]
    fn the_keyboard_follows_the_live_buffer() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        term.run(&mut ed, "run:one:cat");
        let first = ed.buffer.id;
        term.run(&mut ed, "run:two:cat");

        assert!(term.key(&ed, Key::Char('x')), "the live session takes it");
        // Park the live one; the other is untouched.
        term.run(&mut ed, "normal");
        assert!(term.sessions[1].frozen);
        assert!(!term.sessions[0].frozen);
        assert_eq!(ed.mode, Mode::Normal);

        // Switching back to the first session's buffer moves the keyboard with
        // it, and its own frozen flag is what decides — not the other's.
        let i = ed.buffer_names().len() - 1;
        let _ = i;
        let pos = ed
            .others
            .iter()
            .position(|b| b.id == first)
            .expect("still open");
        ed.switch_buffer(pos + 1);
        assert!(term.screen(&ed).is_some(), "the first session still draws");
    }

    /// Closing one session must not disturb the other, and must take its buffer
    /// with it — a dead terminal left in the switcher is litter.
    #[test]
    fn closing_one_session_leaves_the_rest_running() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        term.run(&mut ed, "run:one:cat");
        term.run(&mut ed, "run:two:cat");
        term.run(&mut ed, "close");

        assert_eq!(term.sessions.len(), 1);
        assert_eq!(term.sessions[0].name, "*one*");
        let names = ed.buffer_names();
        assert!(!names.iter().any(|n| n == "*two*"), "{names:?}");
        assert!(names.iter().any(|n| n == "*one*"), "{names:?}");
    }

    /// The other direction: `kill-buffer` knows nothing about PTYs, so the
    /// session has to notice its buffer went and hang up on the child.
    #[test]
    fn killing_the_buffer_ends_the_session() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        term.run(&mut ed, "run:one:cat");
        term.run(&mut ed, "run:two:cat");
        ed.kill_buffer(0); // the live one, `*two*`

        term.sync(&mut ed, &[]);
        assert_eq!(term.sessions.len(), 1);
        assert_eq!(term.sessions[0].name, "*one*");
    }

    /// Wrapping both ways, and no-oping with one session rather than
    /// re-entering it.
    #[test]
    fn sessions_cycle() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        term.run(&mut ed, "run:one:cat");
        term.run(&mut ed, "run:two:cat");
        term.run(&mut ed, "run:three:cat");
        assert_eq!(ed.buffer.name(), "*three*");

        term.run(&mut ed, "next");
        assert_eq!(ed.buffer.name(), "*one*", "wraps past the end");
        term.run(&mut ed, "prev");
        assert_eq!(ed.buffer.name(), "*three*", "and back past the start");
        term.run(&mut ed, "prev");
        assert_eq!(ed.buffer.name(), "*two*");
    }

    /// `SPC o t` shows the shell you already have; `new` is how you ask for a
    /// second one.
    #[test]
    fn open_reuses_the_shell_and_new_does_not() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        term.run(&mut ed, "open");
        term.run(&mut ed, "open");
        assert_eq!(term.sessions.len(), 1, "one shell, shown twice");
        term.run(&mut ed, "new");
        assert_eq!(term.sessions.len(), 2);
        // ...and an agent session is never mistaken for the shell.
        term.run(&mut ed, "run:claude:cat");
        term.run(&mut ed, "open");
        assert_eq!(term.sessions.len(), 3);
        assert_eq!(ed.buffer.name(), "*terminal*");
    }

    /// A harness that is not installed is a message, not a buffer that appears
    /// and vanishes — and nothing else in the editor moves.
    #[test]
    fn a_missing_harness_reports_instead_of_opening_a_buffer() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        let before = ed.buffer_names().len();
        term.run(&mut ed, "run:nope:zemacs-no-such-harness-9f3c --resume");

        assert!(term.sessions.is_empty());
        assert_eq!(ed.buffer_names().len(), before);
        assert!(ed.status.contains("not installed"), "{}", ed.status);
        assert_ne!(ed.mode, Mode::Terminal);
    }

    #[test]
    fn a_malformed_run_verb_reports() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        term.run(&mut ed, "run:justaname");
        assert!(ed.status.contains("NAME:COMMAND"), "{}", ed.status);
        term.run(&mut ed, "wobble");
        assert!(ed.status.contains("unknown terminal verb"), "{}", ed.status);
    }

    /// Restarting keeps the buffer, so window layout and switcher position
    /// survive — and it keeps the command, which is the whole point.
    #[test]
    fn restart_reuses_the_buffer_and_the_command() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        term.run(&mut ed, "run:one:cat");
        let buffer = ed.buffer.id;
        term.run(&mut ed, "restart");

        assert_eq!(term.sessions.len(), 1);
        assert_eq!(ed.buffer.id, buffer, "same buffer");
        assert_eq!(term.sessions[0].inner.command(), Some(&cat()));
        assert!(ed.status.contains("restarted"), "{}", ed.status);
    }
}
