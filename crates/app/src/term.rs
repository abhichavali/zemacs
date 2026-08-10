//! The terminal half that needs a process, mirroring [`crate::magit`] and
//! [`crate::dired`].
//!
//! `zemacs-term` knows PTYs and escape sequences; core knows the buffer kind and
//! the mode. This is the seam: it forks the child, feeds it keystrokes, and
//! flattens its grid back into buffer text so that the buffer switcher, the
//! modeline and `buffer-string` all work on a terminal without a special case.
//! The *colour* does not survive that flattening, so the renderer reads the
//! grid directly — see [`Term::screens`], one per terminal buffer on screen.
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

/// How a `run:`-family verb starts a child, which is the whole of what the
/// three spellings differ by.
#[derive(Clone, Copy, PartialEq)]
enum Start {
    /// A fresh session every press. Two agents side by side is the point.
    New,
    /// Replace the session of that name. A program is a thing you run *again*.
    Reuse,
    /// One command's output, in a pane, with the editor keeping the keyboard.
    Output,
}

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
    /// A compilation pane rather than a session you talk to: the output of one
    /// command, which the editor never hands the keyboard to and therefore
    /// never freezes. `q` closes the window over it — see `terminal-output-mode`
    /// in `runtime/init.lisp`, which is the keymap this flag exists to select.
    output: bool,
}

#[derive(Default)]
pub struct Term {
    sessions: Vec<Session>,
    /// The `:!` commands still running, each waiting to deliver its one line.
    /// Not a [`Session`]: a session is a PTY and a buffer, and the whole point
    /// of the bang is that it leaves neither behind — see [`Term::shell`].
    bangs: Vec<std::sync::mpsc::Receiver<String>>,
}

impl Term {
    /// True while any session is alive, so the app knows to pump them.
    ///
    /// A running bang counts, and that is the only reason `housekeep` needs no
    /// line of its own: it already polls on this answer, so a bang with no
    /// session beside it still gets `sync` called until it has reported.
    pub fn is_live(&self) -> bool {
        !self.sessions.is_empty() || !self.bangs.is_empty()
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
            "new" => self.spawn(editor, "*terminal*", None, Start::New),
            "close" => self.close(editor),
            "restart" => self.restart(editor),
            "next" => self.cycle(editor, 1),
            "prev" => self.cycle(editor, -1),
            // Leaving and re-entering the child. `terminal-normal` in core sets
            // the mode; these are what put the right *text* in the buffer for
            // it, which is the half core cannot do.
            "normal" => self.freeze(editor),
            "insert" => self.thaw(editor),
            // The unnamed register — which *is* the system clipboard, see
            // `Clipboard` in main.rs — typed into the child. The other
            // direction needs nothing: `C-M-t` then `v … y` yanks out of the
            // scrollback, and the register mirrors out to the window system on
            // its own.
            "paste" => self.paste(editor),
            // `run:` starts a session; `rerun:` replaces the one of that name.
            //
            // Two verbs because there are two intentions and they are opposites.
            // A harness is a *thing you start* — two agents side by side is the
            // point, so `run:claude` twice is two sessions. A program is a thing
            // you run *again*: edit, run, read, edit, and a session per press
            // would pile up dead children in the switcher within a minute, all
            // but the last finished and none named distinguishably.
            // `output:` is `rerun:` with the keyboard kept: a build is
            // something you *read*, not something you type at, so the pane
            // opens beside what you were working on and `q` dismisses it.
            other => match (
                other.strip_prefix("run:"),
                other.strip_prefix("rerun:"),
                other.strip_prefix("output:"),
                other.strip_prefix("shell:"),
            ) {
                (Some(rest), ..) => self.run_harness(editor, rest, Start::New),
                (_, Some(rest), ..) => self.run_harness(editor, rest, Start::Reuse),
                (_, _, Some(rest), _) => self.run_harness(editor, rest, Start::Output),
                (.., Some(line)) => self.shell(editor, line),
                _ => editor.apply(EditorCommand::Message(format!(
                    "unknown terminal verb: {other}"
                ))),
            },
        }
    }

    /// `run:NAME:PROGRAM ARG…`.
    ///
    /// The command line is split the way a shell splits one — see [`words`] —
    /// which is the ponytail note that used to be here coming due: `claude -p
    /// "fix the failing test"` is a harness wanting an argument with spaces in
    /// it, and quoting is a fifteen-line function against a new `EditorCommand`
    /// variant and a new envelope. Nothing is *executed* by a shell; the quotes
    /// only decide where one argument ends and the next begins.
    fn run_harness(&mut self, editor: &mut Editor, rest: &str, start: Start) {
        let Some((name, line)) = rest.split_once(':') else {
            editor.apply(EditorCommand::Message(format!(
                "terminal: run needs NAME:COMMAND, got {rest:?}"
            )));
            return;
        };
        let mut words = words(line).into_iter();
        let Some(program) = words.next() else {
            editor.apply(EditorCommand::Message(
                "terminal: run needs a command to run".into(),
            ));
            return;
        };
        let command = Command::new(program, words.collect());
        let buffer = format!("*{name}*");

        let existing = self.sessions.iter().position(|s| s.name == buffer);

        // The pane is the whole point of `output:`: a build you have to switch
        // buffers to read is a build you do not read. Split only when this
        // output is not already on screen — pressing the key twice must not
        // keep halving the frame, and a pane dismissed with `q` has to come
        // back in one of its own rather than taking over whatever you moved on
        // to reading.
        if start == Start::Output {
            let onscreen = existing.is_some_and(|i| {
                let id = self.sessions[i].buffer;
                (editor.frames.iter()).any(|f| f.windows.iter().any(|w| w.buffer == id))
            });
            if !onscreen {
                editor.apply(EditorCommand::SplitWindow(
                    zemacs_core::frame::Split::Columns,
                ));
            }
        }

        // The command is rewritten before restarting rather than reusing the
        // stored one, because this is a *run*, not the resume `restart` was
        // written for: the script it points at may have been regenerated, and
        // re-running the previous command would silently execute the old one.
        if start != Start::New {
            if let Some(i) = existing {
                self.show(editor, i);
                self.sessions[i].inner.set_command(command);
                self.restart(editor);
                return;
            }
        }
        self.spawn(editor, &buffer, Some(command), start);
    }

    /// `shell:COMMAND` — what evil's `:!cmd` becomes.
    ///
    /// Run and *reported*, not opened. `:!` is a thing you want done — `git add
    /// -A`, `make`, `chmod +x` — and leaving a terminal buffer behind for each
    /// one is how the buffer list becomes something you stop reading. The shell
    /// is still one keystroke away when the command is a thing you want to sit
    /// in front of; this is for the other kind.
    ///
    /// Through `$SHELL -c` rather than [`Command`], which is word-split by
    /// [`words`] and never runs a pipeline: `:!sed s/a/b/ < in | wc -l` is a
    /// *shell line*, and the whole point of the bang is that it is.
    ///
    /// A thread and a channel rather than a `try_wait` loop over the child,
    /// because the output is wanted: an unread pipe fills at about 64k and the
    /// command then blocks forever waiting for a reader that is busy drawing
    /// frames. `wait_with_output` already reads both pipes correctly, so the
    /// laziest way to have it is to let it block somewhere that is not here.
    ///
    /// Fired again while one is running, both run — `:!make` and then `:!git
    /// status` is a thing to want, and a bang that refused would be a bang you
    /// have to wait out. The echo area holds one line, so the second report to
    /// land is the one on screen; both are in `*Messages*`, which is what that
    /// log is for.
    fn shell(&mut self, editor: &mut Editor, line: &str) {
        let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let (tx, rx) = std::sync::mpsc::channel();
        // Said before the command has said anything, because the frame comes
        // back instantly now and a `:!make` that answered with nothing at all
        // would look like a bang that did not fire.
        editor.apply(EditorCommand::Message(format!("running {line}…")));
        let line = line.to_string();
        std::thread::spawn(move || {
            let msg = match std::process::Command::new(sh).arg("-c").arg(&line).output() {
                Err(e) => format!("{line}: {e}"),
                Ok(out) => {
                    // stderr only when there is no stdout: a command that
                    // printed both is being read for what it produced, and the
                    // echo area has room for one of them.
                    let text = match out.stdout.is_empty() {
                        true => String::from_utf8_lossy(&out.stderr).into_owned(),
                        false => String::from_utf8_lossy(&out.stdout).into_owned(),
                    };
                    echo_line(text.trim(), out.status)
                }
            };
            // The receiver is gone only if the editor is, and then there is
            // nobody left to tell.
            let _ = tx.send(msg);
        });
        // ponytail: nothing hangs up on the child when the editor quits, so
        // `:!sleep 300` outlives it — the same thing a shell's `&` does. Keeping
        // the `Child` to kill on drop is the upgrade, and it costs the thread
        // the `wait_with_output` that makes this ten lines.
        self.bangs.push(rx);
    }

    /// Report every `:!` that has finished since the last frame.
    ///
    /// Called from [`Term::sync`] rather than from its own hook in `housekeep`,
    /// because `is_live` already keeps that call coming for as long as a bang
    /// is outstanding.
    fn reap_bangs(&mut self, editor: &mut Editor) {
        let mut done = Vec::new();
        self.bangs.retain(|rx| match rx.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Ok(msg) => {
                done.push(msg);
                false
            }
            // A sender dropped without sending is a panicked thread: there is
            // nothing to report and nothing left to wait for.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
        });
        for msg in done {
            editor.apply(EditorCommand::Message(msg));
        }
    }

    fn open(&mut self, editor: &mut Editor) {
        // An existing shell session is shown rather than duplicated, which is
        // what `SPC o t` has always meant. `new` is how you ask for a second.
        if let Some(i) = self.sessions.iter().position(|s| s.inner.command().is_none()) {
            self.show(editor, i);
            return;
        }
        self.spawn(editor, "*terminal*", None, Start::New);
    }

    /// Start a child, give it a buffer, and hand over the keyboard — to the
    /// child, or, for an output pane, to nobody: the editor keeps it, which is
    /// what makes `q` and every other Normal-mode key reach the editor while a
    /// build is still printing.
    fn spawn(&mut self, editor: &mut Editor, name: &str, command: Option<Command>, start: Start) {
        // Start where the current file is, the way `M-x shell` does — a
        // terminal that opens in the wrong directory is a terminal you
        // immediately have to `cd` in, and an *agent* in the wrong directory
        // is one that reads the wrong repository.
        //
        // A terminal buffer has no path, so a second session opened from inside
        // one used to land in whatever directory the *editor* was launched from
        // — rarely the one you were just working in. It inherits its
        // neighbour's instead.
        //
        // ponytail: the neighbour's *starting* directory, not wherever it has
        // since `cd`-ed to. Reading a child's live cwd needs its pid plumbed out
        // of `zemacs-term` and a per-platform lookup (`proc_pidinfo` on macOS,
        // `/proc/PID/cwd` on Linux); worth it the first time following `cd` is
        // what someone actually asks for.
        let cwd = self
            .current(editor)
            .and_then(|i| self.sessions[i].cwd.clone())
            .or_else(|| {
                editor
                    .buffer
                    .path
                    .as_deref()
                    .and_then(|p| if p.is_dir() { Some(p) } else { p.parent() })
                    .map(PathBuf::from)
            })
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
        let output = start == Start::Output;
        self.sessions.push(Session {
            buffer,
            name,
            cwd,
            inner,
            frozen: false,
            output,
        });
        // Explicitly, both ways: `show_named` takes the mode from the buffer
        // *kind*, and a `Terminal` buffer means `Mode::Terminal`. An output
        // pane has to undo that or the first `q` would be typed at make.
        editor.apply(EditorCommand::SetMode(match output {
            true => Mode::Normal,
            false => Mode::Terminal,
        }));
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
        let output = session.output;
        // Empty text: `sync` puts the real grid in on the very next frame, and
        // writing the stale flattening here would flash the previous screenful.
        editor.show_named(kind, Some(&name), "");
        editor.apply(EditorCommand::SetMode(match output {
            true => Mode::Normal,
            false => Mode::Terminal,
        }));
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
        let (name, cwd, command, output) = {
            let session = &self.sessions[i];
            (
                session.name.clone(),
                session.cwd.clone(),
                session.inner.command().cloned(),
                session.output,
            )
        };
        let (cols, rows) = self.sessions[i].inner.size();
        match Terminal::spawn_command(cols, rows, cwd, command) {
            Ok(fresh) => {
                self.sessions[i].inner = fresh;
                self.sessions[i].frozen = false;
                editor.show_named(BufferKind::Terminal, Some(&name), "");
                editor.apply(EditorCommand::SetMode(match output {
                    true => Mode::Normal,
                    false => Mode::Terminal,
                }));
                editor.apply(EditorCommand::Message(format!("restarted {name}")));
            }
            // The old child is already gone at this point only if the spawn
            // succeeded, so a failure leaves the session exactly as it was.
            Err(e) => editor.apply(EditorCommand::Message(format!("terminal: {e:#}"))),
        }
    }

    // `send` — type a line at the live session — went with `project-compile`.
    // It had one caller: `main.rs` typing `cargo build\r` into whatever shell
    // happened to be open. `runtime/plugins/project.lisp` asks for an *output
    // pane* instead (`output:compile:…`), which is a child of its own with the
    // project root as its working directory, so there is nothing left to type
    // into somebody else's shell. `paste` below is the one remaining way bytes
    // the user did not press reach a child, and it is the register.

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
            Key::BackTab => Input::BackTab,
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
            // ...and `⌘←`/`⌘→` the same way: a word at a time in readline, in
            // an agent's input box, and in the editor's own Insert mode.
            Key::MetaLeft => Input::AltLeft,
            Key::MetaRight => Input::AltRight,
            // A shifted arrow or Enter is sent as the plain one, which is what a
            // child saw before shift was a modifier at all: `Input` speaks the
            // VT sequences a terminal has, and there is no `ESC [1;2D` in it —
            // so the alternative was `⇧⏎` silently doing nothing in a shell.
            Key::ShiftEnter => Input::Enter,
            Key::ShiftLeft => Input::Left,
            Key::ShiftRight => Input::Right,
            Key::ShiftUp => Input::Up,
            Key::ShiftDown => Input::Down,
            // `C-M-x` and the two split keys belong to the editor. Leaving them
            // unhandled is what lets `C-<ret>` still split a window while a
            // child has the keyboard — and the `M-S-` keys join them because
            // they are org's, and a shell has no reading of them at all.
            Key::CtrlMeta(_)
            | Key::CtrlEnter
            | Key::CtrlMetaEnter
            | Key::MetaEnter
            | Key::MetaShiftEnter
            | Key::MetaShiftLeft
            | Key::MetaShiftRight => return false,
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

    /// Type the register into the child.
    ///
    /// Silent with an empty register or no session: a paste of nothing is
    /// nothing, not a message worth interrupting a shell for.
    pub fn paste(&self, editor: &Editor) {
        let Some(i) = self.current(editor) else { return };
        let (text, _) = editor.register();
        if !text.is_empty() {
            self.sessions[i].inner.paste(text);
        }
    }

    /// Give the keyboard back to the child.
    pub fn thaw(&mut self, editor: &mut Editor) {
        let Some(i) = self.current(editor) else { return };
        self.sessions[i].frozen = false;
        editor.apply(EditorCommand::SetMode(Mode::Terminal));
    }

    /// Resize every session to the pane it is shown in, drain each child's
    /// requests, refresh the live one's buffer text — and report any `:!` that
    /// finished, which has no session and no pane but the same need to be
    /// looked at once a frame.
    ///
    /// Called every frame. `poll` is the part that must not be skipped, and it
    /// must not be skipped *per session*: a program asking the terminal how big
    /// it is blocks until the answer is written back, so a background agent
    /// that nobody polls is a background agent that hangs.
    ///
    /// `sizes` is `(buffer, cols, rows)` per session, measured by the app —
    /// core owns no geometry and this crate owns no renderer.
    pub fn sync(&mut self, editor: &mut Editor, sizes: &[(BufferId, usize, usize)]) {
        self.reap_bangs(editor);
        self.reap(editor);

        let mut exited: Vec<(String, Option<i32>, String)> = Vec::new();
        self.sessions.retain_mut(|session| {
            if let Some((_, cols, rows)) = sizes.iter().find(|(id, ..)| *id == session.buffer) {
                session.inner.resize(*cols, *rows);
            }
            session.inner.poll();
            if session.inner.exited() {
                // The scrollback, taken here because this is the last moment it
                // exists — the refresh below runs only while a session is still
                // in the list, so without this the buffer keeps whatever the
                // previous frame happened to leave and a build loses the last
                // lines it printed. On a failure those are the error.
                exited.push((
                    session.name.clone(),
                    session.inner.exit_status(),
                    session.inner.history_text(),
                ));
                return false;
            }
            true
        });

        // Report and freeze *after* the retain, so the borrow of `self` is over
        // before the editor is touched.
        for (name, status, text) in exited {
            self.retire(editor, &name, status, &text);
        }

        // Only the live session's buffer is refreshed. A parked one keeps the
        // last screenful it had, and catches up on the frame after you switch
        // to it — which is a frame, not a delay anyone can see, and it keeps
        // `show_named`'s revision bump off every background agent's output.
        let Some(i) = self.current(editor) else { return };
        // `frozen` follows the mode rather than being set alongside it.
        //
        // `freeze` and `thaw` move the two together, but they are not the only
        // way into a terminal buffer: the switcher, `switch-to-buffer` and
        // killing the buffer next door all land here through
        // `Editor::switch_buffer`, which takes the mode from the buffer *kind*
        // and has never heard of a session. That left `Mode::Terminal` beside
        // `frozen: true`, which is the one combination that is silently wrong in
        // both directions at once — the mode routes every keystroke to the child
        // while the freeze stops the buffer refreshing and makes `screen` answer
        // `None`, so you type into an agent and watch a stale screenful not
        // change.
        //
        // The mode is the half the user can see, in the modeline and in what the
        // keys do, so the mode is the half that decides.
        //
        // An output pane is the one session that is never frozen. Freezing
        // means "you stepped out to read the scrollback", and this pane was
        // never stepped into — the editor has had the keyboard since the child
        // started, so the rule above would stop the refresh the pane exists for
        // and leave a build printing into a buffer nobody redraws.
        self.sessions[i].frozen = !self.sessions[i].output && editor.mode != Mode::Terminal;
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
        let want = if self.sessions[i].output {
            "terminal-output-mode"
        } else if self.sessions[i].inner.command().is_some() {
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
    fn retire(&mut self, editor: &mut Editor, name: &str, status: Option<i32>, text: &str) {
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
            // The final scrollback goes in here rather than being left to the
            // refresh below, which no longer runs for a session that is gone.
            editor.show_named(BufferKind::Terminal, Some(name), text);
            editor.apply(EditorCommand::SetMode(Mode::Normal));
        }
    }

    // ponytail: only into the buffer that is *current*. `show_named` is the one
    // way to write buffer text and it switches to what it writes, so doing this
    // unconditionally would have a background agent's death steal the window
    // out from under you. A pane you were not looking at keeps the flattening
    // its last visible frame left; a by-id text setter in core is the upgrade.

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

    /// The live grid of every session on screen, keyed by the buffer showing it,
    /// for the renderer to draw in colour.
    ///
    /// This used to answer for the session with the *keyboard* and no other, and
    /// that was the whole of "a terminal goes grey when you look away": colour
    /// lives in the grid, the buffer text is a flattening with every attribute
    /// already gone, and a pane handed no grid falls through to the text path.
    /// So a shell beside the file you were editing, an agent in the other half
    /// of a split, and both halves of a two-terminal split were all drawn as
    /// plain grey text — correct characters, no colour, no block cursor.
    ///
    /// Only the sessions actually shown in a pane. A background agent's grid is
    /// a few thousand cells to copy every frame and nothing to draw them onto.
    ///
    /// A frozen session is still left out, and for the same reason as before:
    /// frozen means the editor has the keyboard and the buffer holds the
    /// scrollback you stepped out to read, so drawing the child's live screen
    /// over it would take away the thing freezing is for.
    pub fn screens(&self, editor: &Editor) -> Vec<(BufferId, Screen)> {
        let shown = |id: BufferId| {
            editor
                .frames
                .iter()
                .any(|f| f.windows.iter().any(|w| w.buffer == id))
        };
        self.sessions
            .iter()
            .filter(|s| !s.frozen && shown(s.buffer))
            .map(|s| (s.buffer, s.inner.screen(fg(editor), bg(editor))))
            .collect()
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
        // The live session's own grid, built here rather than looked up in
        // `screens`: a click lands in the pane with the pointer in it, which is
        // the one with the keyboard, and a frozen session is being read as text
        // and has no grid coordinates to answer about.
        let i = self.current(editor)?;
        let session = &self.sessions[i];
        if session.frozen {
            return None;
        }
        let screen = session.inner.screen(fg(editor), bg(editor));
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

/// A command line split into arguments, the way a shell splits one: runs of
/// non-space, with `"…"` and `'…'` holding a run together and `\` escaping the
/// next character outside single quotes.
///
/// **Splitting only.** No globbing, no `$VAR`, no `~`, no pipes, no `;` — and
/// nothing here is handed to a shell, so a quote is punctuation and not a
/// promise. The one thing it buys is `claude -p "fix the failing test"`, which
/// is one argument and used to be four.
///
/// An unterminated quote takes everything to the end of the line, which is the
/// forgiving reading and the right one for a prompt somebody typed: `-p "fix
/// the "test"` should run, not report a syntax error at a quote nobody meant.
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut open: Option<char> = None;
    let mut started = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            // Inside single quotes a backslash is a backslash — that is the
            // whole difference between the two quote characters in every shell.
            '\\' if open != Some('\'') => {
                if let Some(next) = chars.next() {
                    word.push(next);
                    started = true;
                }
            }
            '"' | '\'' if open.is_none() => {
                open = Some(c);
                // An empty `""` is still an argument, and a program told to
                // send an empty prompt should be told exactly that.
                started = true;
            }
            c if open == Some(c) => open = None,
            c if c.is_whitespace() && open.is_none() => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(word);
    }
    out
}

/// A command's output as the one line the echo area has room for.
///
/// The first line, plus a count of what is not being shown — so `:!wc -l *` is
/// answered in place, and `:!ls` says how much it is not showing rather than
/// pretending the first name was all of it. `Editor::status_line` is a single
/// line, so a newline in here would be drawn as a box.
///
/// Silence means it worked, which is the shell's own convention, and the exit
/// code is the only news when it did not.
fn echo_line(text: &str, status: std::process::ExitStatus) -> String {
    let code = match status.success() {
        true => String::new(),
        false => format!(" [exit {}]", status.code().unwrap_or(-1)),
    };
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("done");
    match lines.count() {
        0 => format!("{first}{code}"),
        n => format!("{first}{code} (+{n} lines)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `:!` leaves behind now that it leaves no buffer behind: one line,
    /// honest about how much it is not showing, and quiet when there is nothing
    /// to say.
    #[test]
    fn a_bang_is_reported_in_one_line() {
        let run = |line: &str| {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(line)
                .output()
                .unwrap();
            let text = match out.stdout.is_empty() {
                true => String::from_utf8_lossy(&out.stderr).into_owned(),
                false => String::from_utf8_lossy(&out.stdout).into_owned(),
            };
            echo_line(text.trim(), out.status)
        };

        // A pipeline, which is the reason `:!` goes through a shell at all.
        assert_eq!(run("printf 'a\\nb\\nc\\n' | wc -l | tr -d ' '"), "3");
        // More than fits: the first line, and the count of what does not.
        assert_eq!(run("printf 'a\\nb\\nc\\n'"), "a (+2 lines)");
        // Silence is success — the shell's own convention.
        assert_eq!(run("true"), "done");
        // ...and failure says so even when nothing was printed.
        assert_eq!(run("exit 3"), "done [exit 3]");
        assert_eq!(run("echo boom; exit 1"), "boom [exit 1]");
    }

    /// ...and it is reported from a *later* frame than the one that asked for
    /// it, which is the whole of "`:!make` no longer freezes the editor".
    ///
    /// `sleep 1` and not a smaller number: the claim is that `run` returned
    /// while the command was still running, and a command that can finish
    /// inside the assertion proves nothing. The second bang is the answer to
    /// "what if one is fired while another is running" — both run, both report,
    /// and the fast one lands first because finishing is what decides.
    #[test]
    fn a_bang_does_not_block_the_frame_and_reports_when_it_lands() {
        use std::time::{Duration, Instant};

        let mut ed = Editor::new();
        let mut term = Term::default();
        let start = Instant::now();
        term.run(&mut ed, "shell:sleep 1; echo slow");
        term.run(&mut ed, "shell:echo fast");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "the bang blocked the caller for {:?}",
            start.elapsed()
        );
        // The one thing that keeps `housekeep` calling `sync` at all.
        assert!(term.is_live(), "a running bang has to be polled");
        // Both outstanding at once, which is the whole of "two bangs run
        // concurrently" — and asserted *structurally* rather than by racing
        // `echo` against `sleep 1`. This test used to check that the reports
        // arrived in the order `["fast", "slow"]`, which is true whenever the
        // machine is idle and false whenever it is not: under a loaded parallel
        // run, spawning the second thread can lose to a one-second sleep. Two
        // separate agents hit that flake before it was written this way.
        assert_eq!(term.bangs.len(), 2, "a bang must not wait for its sibling");

        let deadline = Instant::now() + Duration::from_secs(30);
        while term.is_live() {
            assert!(Instant::now() < deadline, "the bangs never reported");
            term.sync(&mut ed, &[]);
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut reported: Vec<&str> = ed
            .messages
            .iter()
            .filter(|m| !m.starts_with("running "))
            .map(String::as_str)
            .collect();
        reported.sort_unstable();
        assert_eq!(reported, ["fast", "slow"], "{:?}", ed.messages);
        // Whichever landed last owns the echo area — which of the two that is
        // is a fact about the machine, not about the editor.
        assert!(ed.status == "slow" || ed.status == "fast", "{:?}", ed.status);
        // No buffer, no window, no session — the point of the bang.
        assert!(term.sessions.is_empty());
        assert_eq!(ed.buffer_names(), Editor::new().buffer_names());
    }

    #[test]
    fn a_command_line_splits_the_way_a_shell_splits_one() {
        let cases: [(&str, &[&str]); 7] = [
            // What every resume flag has always been, and still is.
            ("claude -r", &["claude", "-r"]),
            ("cursor-agent   --resume", &["cursor-agent", "--resume"]),
            // The case this function exists for.
            (
                r#"claude -p "fix the failing test""#,
                &["claude", "-p", "fix the failing test"],
            ),
            (r#"claude -p 'it'"#, &["claude", "-p", "it"]),
            // A quote inside a prompt, escaped, and an apostrophe protected by
            // the other quote character.
            (
                r#"claude -p "say \"hi\"""#,
                &["claude", "-p", r#"say "hi""#],
            ),
            (r#"claude -p "don't""#, &["claude", "-p", "don't"]),
            // Forgiving, on purpose: an unterminated quote is not an error.
            (r#"claude -p "half"#, &["claude", "-p", "half"]),
        ];
        for (line, want) in cases {
            assert_eq!(words(line), want, "{line:?}");
        }
        // An empty argument survives, because "send nothing" is a thing to say.
        assert_eq!(words(r#"x "" y"#), ["x", "", "y"]);
        assert!(words("   ").is_empty());
    }

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

    /// `output:` is `rerun:` with the three differences that make it a
    /// compilation pane rather than a session: a window of its own, the
    /// keyboard left with the editor, and a major mode `q` can be bound in.
    ///
    /// The mode assertion is the one that would break silently. Every other
    /// path into a terminal buffer sets `Mode::Terminal` from the buffer
    /// *kind*, so an output pane that forgot to undo that would look right and
    /// type the first `q` at make.
    #[test]
    fn an_output_pane_keeps_the_keyboard_and_opens_beside_you() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        assert_eq!(ed.frame().windows.len(), 1);

        term.run(&mut ed, "output:make:cat");
        assert_eq!(ed.buffer.name(), "*make*");
        assert_eq!(ed.mode, Mode::Normal, "the child must not get the keyboard");
        assert_eq!(ed.frame().windows.len(), 2, "it opens in a split");
        assert!(term.sessions[0].output);

        // Already on screen, so a second press re-runs into the same pane
        // instead of halving the frame again.
        let first = term.sessions[0].buffer;
        term.run(&mut ed, "output:make:cat");
        assert_eq!(term.sessions.len(), 1);
        assert_eq!(term.sessions[0].buffer, first);
        assert_eq!(ed.frame().windows.len(), 2);
        assert_eq!(ed.mode, Mode::Normal);

        // Dismissed with `q` — the window goes, the session stays — and the
        // next press has to build a new window rather than taking over the one
        // you moved on to.
        ed.apply(EditorCommand::CloseWindow);
        assert_eq!(ed.frame().windows.len(), 1);
        term.run(&mut ed, "output:make:cat");
        assert_eq!(ed.frame().windows.len(), 2);
    }

    /// The whole route a command name takes, because the halves are tested
    /// separately and the seam between them is a string: core turns the name
    /// into a verb by stripping a prefix, and this file matches on what is
    /// left. Either side can be right about its own half while the two disagree
    /// about the spelling, and the symptom of that is "unknown terminal verb"
    /// naming a verb that is plainly in the match below.
    #[test]
    fn the_command_name_reaches_the_verb_that_handles_it() {
        for (name, verb) in [
            ("terminal-new", "new"),
            ("terminal-next", "next"),
            ("terminal-prev", "prev"),
            ("terminal-close", "close"),
            ("terminal-restart", "restart"),
            ("terminal-normal", "normal"),
            ("terminal-insert", "insert"),
            ("terminal-paste", "paste"),
        ] {
            let mut ed = Editor::new();
            let out = ed.run_action(name);
            assert_eq!(
                out,
                vec![EditorCommand::Term(verb.into())],
                "{name} should become the {verb:?} verb"
            );
        }

        // ...and the verb the name produces is one this file answers. `run`
        // reports an unknown verb through the status line rather than by
        // failing, so the assertion is on what it did *not* say.
        let mut ed = Editor::new();
        let mut term = Term::default();
        for cmd in ed.run_action("terminal-new") {
            if let EditorCommand::Term(verb) = cmd {
                term.run(&mut ed, &verb);
            }
        }
        assert!(
            !ed.status.contains("unknown terminal verb"),
            "status was {:?}",
            ed.status
        );
        assert_eq!(term.sessions.len(), 1, "a session should have started");
    }

    /// `terminal-new` from inside a session starts where that session did,
    /// rather than where the editor was launched from — a terminal buffer has
    /// no path for the file-buffer rule to read.
    #[test]
    fn a_session_opened_from_a_terminal_inherits_its_directory() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        term.run(&mut ed, "run:one:cat");
        let dir = PathBuf::from("/usr");
        term.sessions[0].cwd = Some(dir.clone());
        term.run(&mut ed, "new");
        assert_eq!(term.sessions[1].cwd, Some(dir));
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
        assert!(
            term.screens(&ed).iter().any(|(id, _)| *id == first),
            "the first session still draws"
        );
    }

    /// The switcher is not `show`, and reaching a frozen session through it used
    /// to leave the mode saying the child had the keyboard while the session
    /// said it was parked. Every keystroke then went to the agent and nothing
    /// on screen moved.
    #[test]
    fn reaching_a_frozen_session_through_the_switcher_gives_the_child_the_keyboard() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        term.run(&mut ed, "run:one:cat");
        let session = ed.buffer.id;
        // Step out of the child to read the scrollback, then go somewhere else.
        term.run(&mut ed, "normal");
        assert!(term.sessions[0].frozen);
        ed.create_buffer("notes".into());
        assert_eq!(ed.mode, Mode::Normal);

        // Back through the switcher rather than through `terminal-next`: the
        // mode comes from the buffer kind, so the session has to follow it.
        ed.switch_buffer_id(session);
        assert_eq!(ed.mode, Mode::Terminal);
        term.sync(&mut ed, &[]);
        assert!(
            !term.sessions[0].frozen,
            "the mode says the child has the keyboard, so it must also be drawing"
        );
        assert!(
            term.screens(&ed).iter().any(|(id, _)| *id == session),
            "and the grid must be drawn"
        );
    }

    /// Colour is per session and not "the terminal's". A session in a pane you
    /// are not looking at still hands the renderer its grid — without one the
    /// pane falls through to the text path, which is the same characters with
    /// every attribute already flattened out of them.
    #[test]
    fn every_session_on_screen_offers_its_own_grid() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        term.run(&mut ed, "run:one:cat");
        let one = ed.buffer.id;
        // A split, so both buffers are in a window at once. The second session
        // takes the focused pane; the first is still shown in the other.
        ed.apply(EditorCommand::SplitWindow(
            zemacs_core::frame::Split::Columns,
        ));
        term.run(&mut ed, "run:two:cat");
        let two = ed.buffer.id;

        let ids: Vec<BufferId> = term.screens(&ed).into_iter().map(|(id, _)| id).collect();
        assert!(ids.contains(&two), "the focused session draws: {ids:?}");
        assert!(ids.contains(&one), "and so does the one beside it: {ids:?}");
    }

    /// ...but only what is on screen. A background agent's grid is a few
    /// thousand cells to copy every frame and nothing to draw them onto.
    #[test]
    fn a_session_in_no_window_is_not_copied() {
        let mut ed = Editor::new();
        let mut term = Term::default();
        term.run(&mut ed, "run:one:cat");
        let parked = ed.buffer.id;
        term.run(&mut ed, "run:two:cat");
        let ids: Vec<BufferId> = term.screens(&ed).into_iter().map(|(id, _)| id).collect();
        assert!(!ids.contains(&parked), "nothing shows it: {ids:?}");
    }

    /// A brand-new session's window starts at the top of its brand-new buffer,
    /// whatever the buffer it replaced was scrolled to.
    #[test]
    fn a_new_session_does_not_inherit_the_previous_buffers_scroll() {
        let mut ed = Editor::new();
        ed.viewport_lines = 10;
        let long: String = (0..400).map(|i| format!("line {i}\n")).collect();
        ed.load(&long, Some(PathBuf::from("/tmp/scrolled.txt")), None);
        ed.apply(EditorCommand::ScrollLines(300));
        assert!(ed.scroll > 0, "the file has to be scrolled for this to test");

        let mut term = Term::default();
        term.run(&mut ed, "new");
        assert_eq!(ed.scroll, 0);
        let frame = &ed.frames[0];
        assert_eq!(frame.windows[frame.current].scroll, 0);
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
