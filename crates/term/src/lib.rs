//! zemacs-term — a real terminal in a buffer.
//!
//! A terminal is a *grid of cells*, not text, which is the one thing in zemacs
//! that does not fit the rope. So this crate keeps its own state and hands the
//! renderer a [`Screen`] — a flat array of coloured cells — while the editor
//! keeps a plain-text flattening in the buffer, so that buffer switching, the
//! modeline and `buffer-list` all work without knowing what a terminal is.
//!
//! # Why `alacritty_terminal`
//!
//! Writing a VT parser by hand is a famous way to spend a month on edge cases,
//! so this is one dependency that does the whole job: PTY, escape sequences,
//! the cell grid, and scrollback. It is pure Rust, so unlike libvterm (a C build
//! plus a separate `forkpty`, and no scrollback of its own) or libghostty (a Zig
//! toolchain in the build) it costs nothing at the build boundary.
//!
//! Embedding another terminal *application* is not an option on macOS: window
//! embedding needs a protocol like X11's XEmbed, and there is no equivalent —
//! SDL owns its window and nothing can be reparented into it.
//!
//! # Threads
//!
//! `alacritty_terminal` runs its own PTY reader thread, the fourth in zemacs
//! after input/draw, Lisp and syntax. It owns the `Term` behind a mutex, and
//! this crate only ever takes that lock to copy a screenful out, so a program
//! spewing output cannot stall the editor.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener, Notify, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Grid, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// How much scrollback to keep — what a terminal emulator typically ships with.
const SCROLLBACK: usize = 10_000;

// --- what the renderer draws ---------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cell {
    pub c: char,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl Cell {
    fn blank(fg: [u8; 3], bg: [u8; 3]) -> Self {
        Self {
            c: ' ',
            fg,
            bg,
            bold: false,
            italic: false,
            underline: false,
        }
    }
}

/// One frame of the terminal: `rows * cols` cells in row-major order.
pub struct Screen {
    pub rows: usize,
    pub cols: usize,
    pub cells: Vec<Cell>,
    /// `(row, col)` of the block cursor, or `None` when it is hidden or has
    /// been scrolled out of view.
    pub cursor: Option<(usize, usize)>,
}

impl Screen {
    pub fn cell(&self, row: usize, col: usize) -> Option<&Cell> {
        (row < self.rows && col < self.cols).then(|| &self.cells[row * self.cols + col])
    }

    /// The screen as plain text, one line per row with trailing blanks removed.
    ///
    /// This is what goes in the buffer. Deliberately lossy — colour lives in
    /// [`Screen::cells`] and is drawn from there — but it means everything that
    /// reads buffer text (the buffer switcher, `buffer-string`, the modeline's
    /// line count) works on a terminal with no special case.
    pub fn to_text(&self) -> String {
        let mut out = String::with_capacity(self.rows * (self.cols + 1));
        for row in 0..self.rows {
            let line: String = (0..self.cols)
                .filter_map(|col| self.cell(row, col).map(|cell| cell.c))
                .collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        // A terminal is mostly blank space; the buffer should end after the last
        // line with anything on it rather than carry forty empty rows.
        let keep = out.trim_end_matches('\n').len();
        out.truncate(keep);
        out
    }
}

// --- keyboard -------------------------------------------------------------

/// A keystroke, in the only terms a terminal cares about.
///
/// Deliberately not `zemacs_core::Key`: encoding keys for a PTY is terminal
/// knowledge and belongs here, but nothing else in this crate needs the editor,
/// and keeping that dependency out means the whole thing is testable without one.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Input {
    Char(char),
    Ctrl(char),
    Alt(char),
    Enter,
    Tab,
    /// `⇧⇥`.
    BackTab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
}

/// The bytes a real terminal would send for `input`.
///
/// `app_cursor` is DECCKM, which the child turns on and off as it pleases —
/// readline sets it, and so does every full-screen program. In that mode the
/// arrows are `ESC O A`, not `ESC [ A`; sending the wrong one is why shell
/// history appears to ignore Up.
pub fn encode(input: Input, app_cursor: bool) -> Vec<u8> {
    // `ESC O` in application mode, `ESC [` otherwise. The final letter is the
    // same either way.
    let arrow = |letter: u8| vec![0x1b, if app_cursor { b'O' } else { b'[' }, letter];
    match input {
        Input::Char(c) => c.to_string().into_bytes(),
        // C-a is 0x01 up to C-z at 0x1a; the control codes above those are
        // spelled with punctuation, and C-space is NUL.
        Input::Ctrl(c) => {
            let c = c.to_ascii_lowercase();
            let byte = match c {
                'a'..='z' => c as u8 - b'a' + 1,
                '@' | ' ' => 0,
                '[' => 0x1b,
                '\\' => 0x1c,
                ']' => 0x1d,
                '^' => 0x1e,
                '_' | '?' => 0x1f,
                _ => return c.to_string().into_bytes(),
            };
            vec![byte]
        }
        // Meta is an ESC prefix, which is what every terminal since the VT100
        // has meant by it.
        Input::Alt(c) => {
            let mut out = vec![0x1b];
            out.extend_from_slice(c.to_string().as_bytes());
            out
        }
        // CR, not LF: Enter means carriage return, and sending \n gets you a
        // shell that never runs anything.
        Input::Enter => vec![b'\r'],
        Input::Tab => vec![b'\t'],
        // CSI Z — `kcbt` in terminfo, and unaffected by DECCKM: backtab has
        // never had an application-mode spelling the way the arrows do.
        Input::BackTab => vec![0x1b, b'[', b'Z'],
        // DEL rather than BS, which is what termios expects for erase.
        Input::Backspace => vec![0x7f],
        Input::Esc => vec![0x1b],
        Input::Up => arrow(b'A'),
        Input::Down => arrow(b'B'),
        Input::Right => arrow(b'C'),
        Input::Left => arrow(b'D'),
    }
}

// --- mouse ----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Button {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MouseKind {
    Press,
    Release,
    /// The pointer moved with a button held.
    Drag,
}

/// A mouse event in *cell* coordinates, zero-based from the top-left of the
/// grid. Converting from pixels is the renderer's job, not this crate's.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Mouse {
    pub button: Button,
    pub kind: MouseKind,
    pub col: usize,
    pub row: usize,
}

/// The bytes a terminal sends for `mouse`, or `None` if the child has not asked
/// for mouse events.
///
/// Two wire formats. SGR (`ESC [ < b ; x ; y M`) is what everything modern
/// negotiates and is unbounded; the original X10 encoding packs each coordinate
/// into one byte biased by 32, so it cannot express a column past 223 — which is
/// exactly why SGR exists and why it is preferred whenever it is on.
fn encode_mouse(mouse: Mouse, modes: Modes) -> Option<Vec<u8>> {
    if !modes.mouse {
        return None;
    }
    // Motion is only wanted by programs that asked for drag reporting.
    if mouse.kind == MouseKind::Drag && !modes.mouse_drag {
        return None;
    }
    let mut code = match mouse.button {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
        // The wheel is reported as buttons 4 and 5, in the high bank.
        Button::WheelUp => 64,
        Button::WheelDown => 65,
    };
    if mouse.kind == MouseKind::Drag {
        code += 32;
    }
    let (x, y) = (mouse.col + 1, mouse.row + 1); // the wire is 1-based

    if modes.sgr_mouse {
        // Release is the same code with a lowercase final byte, which is the
        // whole reason SGR can report *which* button came up and X10 cannot.
        let last = if mouse.kind == MouseKind::Release {
            'm'
        } else {
            'M'
        };
        return Some(format!("\x1b[<{code};{x};{y}{last}").into_bytes());
    }

    // X10: a release is button 3, and anything past column 223 is unsendable,
    // so it is dropped rather than reported at a wrong position.
    let code = if mouse.kind == MouseKind::Release {
        3
    } else {
        code
    };
    if x > 223 || y > 223 {
        return None;
    }
    Some(vec![
        0x1b,
        b'[',
        b'M',
        32 + code as u8,
        32 + x as u8,
        32 + y as u8,
    ])
}

/// The child's current expectations, as far as anything outside this crate
/// needs to care. All of them are set and cleared by the program running in the
/// terminal, not by us.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Modes {
    /// DECCKM: arrows are `ESC O A`, not `ESC [ A`.
    pub app_cursor: bool,
    /// The child wants mouse events at all.
    pub mouse: bool,
    /// ...including motion with a button held.
    pub mouse_drag: bool,
    /// The child negotiated the modern, unbounded mouse encoding.
    pub sgr_mouse: bool,
    /// A full-screen program is running. There is no scrollback to scroll.
    pub alt_screen: bool,
    /// The child wants the wheel translated into arrow keys on the alternate
    /// screen — how `less` and `man` expect to be scrolled.
    pub alternate_scroll: bool,
    /// The child wants a paste to arrive marked as one rather than as typing.
    pub bracketed_paste: bool,
}

/// Clipboard text, as the child expects to receive it.
///
/// Newlines become CR for the same reason Enter does: a shell reads LF as
/// nothing. `bracketed` wraps the text in the markers a program turns the mode
/// on to ask for — without them a multi-line paste *runs* every line but the
/// last the instant it lands, which is the oldest footgun a terminal has.
///
/// ESC is dropped from a bracketed paste: the clipboard is text from outside,
/// and an `ESC [ 2 0 1 ~` sitting in it would end the paste early and hand the
/// rest to the shell as keystrokes. Unbracketed, a paste *is* typing and an ESC
/// in it means what pressing ESC means.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace('\n', "\r");
    match bracketed {
        true => format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).into_bytes(),
        false => text.into_bytes(),
    }
}

// --- the terminal ---------------------------------------------------------

/// Events the PTY thread raises.
///
/// Queued rather than handled on the spot: answering one usually means writing
/// back to the PTY, and the writer does not exist yet when the listener has to
/// be constructed. [`Terminal::poll`] drains them.
#[derive(Clone)]
struct Proxy(Sender<Event>);

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event);
    }
}

pub struct Terminal {
    term: Arc<FairMutex<Term<Proxy>>>,
    notifier: Notifier,
    events: Receiver<Event>,
    cols: usize,
    rows: usize,
    title: String,
    exited: bool,
    /// The child's exit code, once it has one. Kept rather than thrown away
    /// because a *harness* that dies on the first frame — a bad flag, an
    /// expired login — is otherwise indistinguishable from a shell you quit on
    /// purpose, and 127 is the difference between "not installed" and "fine".
    status: Option<i32>,
    /// What was spawned, so a session can be restarted with the same command.
    /// `None` is `$SHELL`.
    command: Option<Command>,
}

/// A program to run on the PTY instead of `$SHELL` — how a coding-agent CLI
/// becomes a terminal buffer. Args are a vector rather than a line because
/// nothing here should be re-parsing quoting; whoever built the list knows
/// where the words are.
#[derive(Clone, PartialEq, Debug)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
}

impl Command {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
        }
    }
}

/// Where `program` is on `$PATH`, if anywhere.
///
/// A bare `which`, in eight lines, so the app can say "cursor-agent is not
/// installed" rather than forking a PTY that dies before its first frame. A
/// name containing a separator is a path and is taken as one, which is what
/// every shell does.
///
/// ponytail: no `PATHEXT`, no executable-bit check beyond what `metadata` will
/// tell us — this is macOS and Linux, and a non-executable file on `$PATH` with
/// the name of a coding agent is not a case worth code.
pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let direct = PathBuf::from(program);
        return direct.is_file().then_some(direct);
    }
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join(program))
        .find(|candidate| candidate.is_file())
}

/// `Dimensions` without the scrollback, which is all `Term::new` and `resize`
/// need. Alacritty ships one of these in a `test` module; this is three lines
/// and does not tie the editor to somebody else's test helper.
#[derive(Clone, Copy)]
struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

fn window_size(cols: usize, rows: usize) -> WindowSize {
    WindowSize {
        num_lines: rows as u16,
        num_cols: cols as u16,
        // Only used to answer pixel-size queries from the child; the real
        // geometry is the renderer's.
        cell_width: 1,
        cell_height: 1,
    }
}

impl Terminal {
    /// Start a shell on a new PTY, `cols` by `rows`.
    pub fn spawn(cols: usize, rows: usize, cwd: Option<PathBuf>) -> anyhow::Result<Self> {
        Self::spawn_command(cols, rows, cwd, None)
    }

    /// The same, running `command` instead of `$SHELL`.
    ///
    /// This is the whole difference between a shell buffer and an AI session:
    /// the harness *is* the child, so quitting it ends the session and there is
    /// no shell underneath to be left sitting at a prompt.
    pub fn spawn_command(
        cols: usize,
        rows: usize,
        cwd: Option<PathBuf>,
        command: Option<Command>,
    ) -> anyhow::Result<Self> {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let size = Size { cols, rows };
        // Checked before the fork rather than after: `tty::new` reports a failed
        // exec as a bare "No such file or directory" with no hint of *what* was
        // missing, and a menu that offers a harness has to be able to say the
        // binary is not there.
        if let Some(cmd) = &command {
            if which(&cmd.program).is_none() {
                anyhow::bail!("{} is not installed (not on $PATH)", cmd.program);
            }
        }

        let mut env = HashMap::new();
        // Not `alacritty`, which is the default: that terminfo entry only exists
        // where Alacritty is installed, and a shell that cannot find its TERM
        // entry loses colour, arrow keys and clear-screen. `xterm-256color` is
        // everywhere.
        env.insert("TERM".to_string(), "xterm-256color".to_string());

        let options = tty::Options {
            shell: command
                .clone()
                .map(|c| tty::Shell::new(c.program, c.args)),
            working_directory: cwd,
            // A harness that exits has usually just printed the reason. Draining
            // means that last screenful is in the grid when the buffer freezes,
            // instead of the session going blank at the moment it has something
            // to say.
            drain_on_exit: true,
            env,
        };
        let pty = tty::new(&options, window_size(cols, rows), 0)?;

        let (tx, events) = channel();
        let proxy = Proxy(tx);
        let config = Config {
            scrolling_history: SCROLLBACK,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, proxy.clone())));

        let event_loop = EventLoop::new(term.clone(), proxy, pty, false, false)?;
        let notifier = Notifier(event_loop.channel());
        event_loop.spawn();

        Ok(Self {
            term,
            notifier,
            events,
            cols,
            rows,
            title: "terminal".into(),
            exited: false,
            status: None,
            command,
        })
    }

    /// Send bytes to the child.
    pub fn send(&self, bytes: Vec<u8>) {
        self.notifier.notify(bytes);
    }

    pub fn input(&self, input: Input) {
        self.send(encode(input, self.modes().app_cursor));
    }

    /// What the program in the terminal currently expects.
    pub fn modes(&self) -> Modes {
        let term = self.term.lock();
        let mode = *term.mode();
        Modes {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            mouse: mode.intersects(TermMode::MOUSE_MODE),
            mouse_drag: mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION),
            sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        }
    }

    /// Hand TEXT to the child as a paste.
    pub fn paste(&self, text: &str) {
        self.send(encode_paste(text, self.modes().bracketed_paste));
    }

    /// Report a mouse event to the child. False when it has not asked for
    /// mouse events, so the caller can do something editor-shaped instead.
    pub fn mouse(&self, mouse: Mouse) -> bool {
        match encode_mouse(mouse, self.modes()) {
            Some(bytes) => {
                self.send(bytes);
                true
            }
            None => false,
        }
    }

    /// Turn a wheel notch into whatever this program expects.
    ///
    /// Three cases, and getting them wrong is what makes a terminal feel broken:
    /// a program with mouse reporting on wants the event; `less` and friends on
    /// the alternate screen want arrow keys, because there is no scrollback to
    /// move through; anything else wants the view scrolled. `lines` is positive
    /// toward the top of the history.
    pub fn wheel(&self, lines: i32, col: usize, row: usize) -> bool {
        let modes = self.modes();
        let button = if lines > 0 {
            Button::WheelUp
        } else {
            Button::WheelDown
        };
        let count = lines.unsigned_abs() as usize;
        if modes.mouse {
            for _ in 0..count {
                self.mouse(Mouse {
                    button,
                    kind: MouseKind::Press,
                    col,
                    row,
                });
            }
            return true;
        }
        if modes.alt_screen {
            if !modes.alternate_scroll {
                return false; // a full-screen program that wants neither
            }
            let arrow = if lines > 0 { Input::Up } else { Input::Down };
            let bytes = encode(arrow, modes.app_cursor);
            for _ in 0..count {
                self.send(bytes.clone());
            }
            return true;
        }
        self.scroll(lines);
        true
    }

    /// Handle whatever the PTY thread has raised since last time.
    ///
    /// Must be called regularly: a program asking "where is the cursor?" or
    /// "how big is the window?" is *blocked* until the answer is written back,
    /// so skipping this hangs anything that queries the terminal.
    pub fn poll(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::PtyWrite(text) => self.send(text.into_bytes()),
                Event::Title(title) => self.title = title,
                Event::ResetTitle => self.title = "terminal".into(),
                Event::TextAreaSizeRequest(reply) => {
                    let answer = reply(window_size(self.cols, self.rows));
                    self.send(answer.into_bytes());
                }
                Event::ChildExit(code) => {
                    self.exited = true;
                    self.status = Some(code);
                }
                Event::Exit => self.exited = true,
                // Bell, clipboard, colour queries and cursor-blink changes are
                // ignored deliberately — none of them has anywhere to go yet.
                _ => {}
            }
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// True once the shell has exited, so the app can retire the buffer.
    pub fn exited(&self) -> bool {
        self.exited
    }

    /// The child's exit code, once there is one.
    pub fn exit_status(&self) -> Option<i32> {
        self.status
    }

    /// What this session is running; `None` for a plain `$SHELL`. Kept so a
    /// session can be restarted without the app having to remember for it.
    pub fn command(&self) -> Option<&Command> {
        self.command.as_ref()
    }

    /// Point an existing session at a different command, to take effect on the
    /// next restart.
    ///
    /// For re-*running* rather than resuming: a run reuses the session so the
    /// buffer and window survive, but the thing being run may have been
    /// regenerated since, and restarting the remembered command would quietly
    /// execute the previous one. Does not disturb the child that is live now —
    /// `restart` is what ends that.
    pub fn set_command(&mut self, command: Command) {
        self.command = Some(command);
    }

    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    /// Tell the child the window changed shape. A no-op when it did not, since
    /// a needless `SIGWINCH` makes full-screen programs redraw for nothing.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.term.lock().resize(Size { cols, rows });
        let _ = self.notifier.0.send(Msg::Resize(window_size(cols, rows)));
    }

    /// Scroll through the scrollback. Positive is toward the top of the
    /// history, matching the editor's own wheel handling.
    pub fn scroll(&self, lines: i32) {
        self.term.lock().scroll_display(Scroll::Delta(lines));
    }

    /// Everything the terminal remembers, as plain text: the whole scrollback
    /// followed by the visible screen.
    ///
    /// This is what goes in the buffer when the editor takes the keyboard back,
    /// and it is the only reason the motions have anything to work on — the
    /// visible grid alone is one screenful with no history to search, yank from,
    /// or jump around in.
    pub fn history_text(&self) -> String {
        let term = self.term.lock();
        let grid = term.grid();
        let (rows, cols) = (grid.screen_lines(), grid.columns());
        let first = -(grid.history_size() as i32);

        let mut out = String::new();
        for row in first..rows as i32 {
            let line = &grid[Line(row)];
            let text: String = (0..cols)
                .filter(|c| !line[Column(*c)].flags.contains(Flags::WIDE_CHAR_SPACER))
                .map(|c| line[Column(c)].c)
                .collect();
            out.push_str(text.trim_end());
            out.push('\n');
        }
        // Blank rows below the prompt are not history, and landing on them with
        // `G` would look like the buffer had lost its contents.
        let keep = out.trim_end_matches('\n').len();
        out.truncate(keep);
        out
    }

    /// Copy the visible grid out. `fg`/`bg` are the editor's own colours, used
    /// wherever the child asked for "default", so a terminal pane matches the
    /// theme instead of being a black rectangle in the middle of it.
    pub fn screen(&self, fg: [u8; 3], bg: [u8; 3]) -> Screen {
        let term = self.term.lock();
        let grid = term.grid();
        let (rows, cols) = (grid.screen_lines(), grid.columns());
        let offset = grid.display_offset() as i32;

        let mut cells = vec![Cell::blank(fg, bg); rows * cols];
        for row in 0..rows {
            let line = Line(row as i32 - offset);
            for col in 0..cols {
                let cell = &grid[line][Column(col)];
                let flags = cell.flags;
                // A wide character occupies two columns; the spacer that follows
                // it has no glyph of its own, and drawing its `c` would paint a
                // stray space over the character's right half.
                if flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let (mut fg_c, mut bg_c) = (
                    resolve(cell.fg, fg, bg, flags.contains(Flags::BOLD)),
                    resolve(cell.bg, fg, bg, false),
                );
                if flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg_c, &mut bg_c);
                }
                if flags.contains(Flags::HIDDEN) {
                    fg_c = bg_c;
                }
                cells[row * cols + col] = Cell {
                    c: cell.c,
                    fg: fg_c,
                    bg: bg_c,
                    bold: flags.contains(Flags::BOLD),
                    italic: flags.contains(Flags::ITALIC),
                    underline: flags.contains(Flags::UNDERLINE),
                };
            }
        }

        let cursor = cursor_at(grid, *term.mode(), rows, cols, offset);
        Screen {
            rows,
            cols,
            cursor,
            cells,
        }
    }

    /// The OSC 8 hyperlink the child attached to the cell at `(col, row)`, if
    /// any — the target of `cargo`'s error codes and of `ls --hyperlink`, where
    /// the text you see is a word and the link behind it is a URL.
    ///
    /// Asked for on demand rather than carried on every [`Cell`]: the grid is
    /// copied out sixty times a second and a cell is `Copy`, so a `String` per
    /// cell would be tens of thousands of allocations a frame to answer a
    /// question only a click ever asks.
    pub fn link_at(&self, col: usize, row: usize) -> Option<String> {
        let term = self.term.lock();
        let grid = term.grid();
        if row >= grid.screen_lines() || col >= grid.columns() {
            return None;
        }
        let line = Line(row as i32 - grid.display_offset() as i32);
        let uri = grid[line][Column(col)].hyperlink()?.uri().to_string();
        Some(uri)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Without this the reader thread outlives the buffer and the shell is
        // left running with nobody reading it.
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

fn cursor_at(
    grid: &Grid<alacritty_terminal::term::cell::Cell>,
    mode: TermMode,
    rows: usize,
    cols: usize,
    offset: i32,
) -> Option<(usize, usize)> {
    if !mode.contains(TermMode::SHOW_CURSOR) {
        return None;
    }
    let Point { line, column } = grid.cursor.point;
    // Scrolled back into history puts the cursor off-screen; drawing it clamped
    // to an edge would be a lie about where typing will go.
    let row = usize::try_from(line.0 + offset).ok()?;
    (row < rows && column.0 < cols).then_some((row, column.0))
}

// --- colour ---------------------------------------------------------------

/// Turn a terminal colour into RGB, given the editor's default foreground and
/// background.
fn resolve(color: Color, fg: [u8; 3], bg: [u8; 3], bold: bool) -> [u8; 3] {
    match color {
        Color::Spec(rgb) => [rgb.r, rgb.g, rgb.b],
        Color::Indexed(i) => indexed(i),
        Color::Named(named) => match named {
            NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground => fg,
            NamedColor::Background => bg,
            NamedColor::Cursor => fg,
            // Everything else is one of the 16, possibly under its dim name.
            // Bold has historically meant "bright" as well as heavy, and enough
            // programs still assume it that ignoring it loses colour in `ls`
            // and `git`.
            other => {
                let i = other as usize;
                let base = if i < 16 { i as u8 } else { dim_to_ansi(other) };
                indexed(if bold && base < 8 { base + 8 } else { base })
            }
        },
    }
}

/// The `Dim*` names sit above 256 in declaration order, starting at `DimBlack`.
fn dim_to_ansi(named: NamedColor) -> u8 {
    let dim_black = NamedColor::DimBlack as usize;
    ((named as usize).saturating_sub(dim_black) % 8) as u8
}

/// The xterm 256-colour palette: 16 named, a 6×6×6 cube, then 24 greys.
fn indexed(i: u8) -> [u8; 3] {
    const ANSI: [[u8; 3]; 16] = [
        [0x00, 0x00, 0x00],
        [0xcd, 0x31, 0x31],
        [0x0d, 0xbc, 0x79],
        [0xe5, 0xe5, 0x10],
        [0x24, 0x72, 0xc8],
        [0xbc, 0x3f, 0xbc],
        [0x11, 0xa8, 0xcd],
        [0xe5, 0xe5, 0xe5],
        [0x66, 0x66, 0x66],
        [0xf1, 0x4c, 0x4c],
        [0x23, 0xd1, 0x8b],
        [0xf5, 0xf5, 0x43],
        [0x3b, 0x8e, 0xea],
        [0xd6, 0x70, 0xd6],
        [0x29, 0xb8, 0xdb],
        [0xff, 0xff, 0xff],
    ];
    match i {
        0..=15 => ANSI[i as usize],
        16..=231 => {
            const LEVEL: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let i = i as usize - 16;
            [LEVEL[(i / 36) % 6], LEVEL[(i / 6) % 6], LEVEL[i % 6]]
        }
        _ => {
            let v = 8 + 10 * (i as u16 - 232);
            let v = v.min(255) as u8;
            [v, v, v]
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn control_characters_follow_the_ascii_table() {
        assert_eq!(encode(Input::Ctrl('a'), false), vec![0x01]);
        assert_eq!(encode(Input::Ctrl('c'), false), vec![0x03]); // the one everybody presses
        assert_eq!(encode(Input::Ctrl('d'), false), vec![0x04]); // EOF
        assert_eq!(encode(Input::Ctrl('z'), false), vec![0x1a]);
        // case does not matter: C-C is C-c
        assert_eq!(encode(Input::Ctrl('C'), false), encode(Input::Ctrl('c'), false));
        assert_eq!(encode(Input::Ctrl(' '), false), vec![0x00]);
        assert_eq!(encode(Input::Ctrl('['), false), encode(Input::Esc, false));
    }

    /// Enter must be CR. With LF the shell reads a line that never ends, and
    /// nothing typed ever runs.
    #[test]
    fn enter_is_a_carriage_return_and_backspace_is_del() {
        assert_eq!(encode(Input::Enter, false), vec![b'\r']);
        assert_eq!(encode(Input::Backspace, false), vec![0x7f]);
    }

    /// A paste is typing, so its newlines are the same CR that Enter is — and
    /// when the child asked for the mode it arrives marked, which is what stops
    /// three copied lines from being three commands.
    #[test]
    fn a_paste_carries_returns_and_is_bracketed_only_when_asked() {
        assert_eq!(encode_paste("ls -l\nwc\n", false), b"ls -l\rwc\r".to_vec());
        assert_eq!(
            encode_paste("ls -l\nwc\n", true),
            b"\x1b[200~ls -l\rwc\r\x1b[201~".to_vec()
        );
        // Text from the clipboard cannot end its own paste and go on typing.
        assert_eq!(
            encode_paste("a\x1b[201~rm -rf /", true),
            b"\x1b[200~a[201~rm -rf /\x1b[201~".to_vec()
        );
    }

    /// `⇧⇥` is CSI Z, and stays CSI Z in application-cursor mode — an agent's
    /// input box cycles its modes backwards on it, and a plain `\t` there just
    /// cycles forwards.
    #[test]
    fn backtab_is_csi_z_in_both_cursor_modes() {
        assert_eq!(encode(Input::BackTab, false), b"\x1b[Z".to_vec());
        assert_eq!(encode(Input::BackTab, true), b"\x1b[Z".to_vec());
        assert_ne!(encode(Input::BackTab, false), encode(Input::Tab, false));
    }

    #[test]
    fn arrows_and_meta_use_escape_sequences() {
        assert_eq!(encode(Input::Up, false), b"\x1b[A".to_vec());
        assert_eq!(encode(Input::Down, false), b"\x1b[B".to_vec());
        assert_eq!(encode(Input::Right, false), b"\x1b[C".to_vec());
        assert_eq!(encode(Input::Left, false), b"\x1b[D".to_vec());
        // M-b, as readline's "back one word"
        assert_eq!(encode(Input::Alt('b'), false), vec![0x1b, b'b']);
    }

    /// The bug that made shell history look broken: readline turns DECCKM on,
    /// and in that mode `ESC [ A` is not what Up means.
    #[test]
    fn application_cursor_mode_changes_what_the_arrows_send() {
        for (input, letter) in [
            (Input::Up, b'A'),
            (Input::Down, b'B'),
            (Input::Right, b'C'),
            (Input::Left, b'D'),
        ] {
            assert_eq!(encode(input, false), vec![0x1b, b'[', letter]);
            assert_eq!(encode(input, true), vec![0x1b, b'O', letter]);
        }
        // Nothing else changes with the mode.
        assert_eq!(encode(Input::Enter, true), vec![b'\r']);
        assert_eq!(encode(Input::Ctrl('c'), true), vec![0x03]);
    }

    /// `⌘⌫` is `ESC DEL`, which readline reads as backward-kill-word.
    #[test]
    fn meta_backspace_kills_a_word_in_the_shell() {
        assert_eq!(encode(Input::Alt('\u{7f}'), false), vec![0x1b, 0x7f]);
    }

    /// `cargo` marks its error codes this way, and `ls --hyperlink` its
    /// filenames: OSC 8 puts a URL *beside* the text rather than in it, so what
    /// is on screen is a word and reading the row would find nothing to open.
    /// The click path asks the cell instead — this is the proof the parser
    /// keeps the link at all.
    #[test]
    fn an_osc_8_hyperlink_is_readable_off_the_cell() {
        let link = "https://doc.rust-lang.org/error_codes/E0308.html";
        let mut term = Terminal::spawn_command(
            40,
            4,
            None,
            Some(Command::new(
                "printf",
                vec![format!("\\033]8;;{link}\\033\\\\E0308\\033]8;;\\033\\\\")],
            )),
        )
        .expect("printf must be on $PATH");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            term.poll();
            if let Some(uri) = term.link_at(0, 0) {
                assert_eq!(uri, link);
                let screen = term.screen([0; 3], [0; 3]);
                assert_eq!(screen.cell(0, 0).map(|c| c.c), Some('E'), "the text is the code");
                return;
            }
            assert!(Instant::now() < deadline, "no hyperlink arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_child_that_never_asked_gets_no_mouse_events() {
        let quiet = Modes::default();
        let event = Mouse {
            button: Button::Left,
            kind: MouseKind::Press,
            col: 3,
            row: 4,
        };
        assert_eq!(encode_mouse(event, quiet), None);
    }

    #[test]
    fn mouse_events_are_reported_in_whichever_format_was_negotiated() {
        let sgr = Modes {
            mouse: true,
            sgr_mouse: true,
            ..Modes::default()
        };
        let press = Mouse {
            button: Button::Left,
            kind: MouseKind::Press,
            col: 3,
            row: 4,
        };
        // 1-based on the wire, so column 3 is the 4th.
        assert_eq!(encode_mouse(press, sgr).unwrap(), b"\x1b[<0;4;5M".to_vec());
        // Release differs only in the final byte — the whole reason SGR exists,
        // since X10 cannot say *which* button came up.
        let release = Mouse {
            kind: MouseKind::Release,
            ..press
        };
        assert_eq!(encode_mouse(release, sgr).unwrap(), b"\x1b[<0;4;5m".to_vec());
        // The wheel rides in the high bank.
        let wheel = Mouse {
            button: Button::WheelUp,
            ..press
        };
        assert_eq!(encode_mouse(wheel, sgr).unwrap(), b"\x1b[<64;4;5M".to_vec());

        let x10 = Modes {
            mouse: true,
            ..Modes::default()
        };
        assert_eq!(
            encode_mouse(press, x10).unwrap(),
            vec![0x1b, b'[', b'M', 32, 32 + 4, 32 + 5]
        );
        // X10 packs a coordinate into one byte, so past 223 it can only lie —
        // dropping the event beats reporting the wrong cell.
        let far = Mouse { col: 300, ..press };
        assert_eq!(encode_mouse(far, x10), None);
        assert!(encode_mouse(far, sgr).is_some(), "SGR has no such limit");
    }

    /// Motion is noise to a program that only asked for clicks.
    #[test]
    fn drags_go_only_to_a_child_that_asked_for_them() {
        let drag = Mouse {
            button: Button::Left,
            kind: MouseKind::Drag,
            col: 0,
            row: 0,
        };
        let clicks_only = Modes {
            mouse: true,
            sgr_mouse: true,
            ..Modes::default()
        };
        assert_eq!(encode_mouse(drag, clicks_only), None);
        let dragging = Modes {
            mouse_drag: true,
            ..clicks_only
        };
        // +32 marks it as motion rather than a fresh press.
        assert_eq!(
            encode_mouse(drag, dragging).unwrap(),
            b"\x1b[<32;1;1M".to_vec()
        );
    }

    #[test]
    fn text_is_sent_as_utf8() {
        assert_eq!(encode(Input::Char('x'), false), vec![b'x']);
        assert_eq!(encode(Input::Char('é'), false), "é".as_bytes().to_vec());
    }

    #[test]
    fn the_palette_covers_all_256_slots() {
        assert_eq!(indexed(0), [0, 0, 0]);
        assert_eq!(indexed(15), [0xff, 0xff, 0xff]);
        // the cube: 16 is its black corner, 231 its white one
        assert_eq!(indexed(16), [0, 0, 0]);
        assert_eq!(indexed(231), [255, 255, 255]);
        // ...and one from the middle, 16 + 36*2 + 6*3 + 4
        assert_eq!(indexed(16 + 36 * 2 + 6 * 3 + 4), [135, 175, 215]);
        // the greys ramp without ever leaving u8
        assert_eq!(indexed(232), [8, 8, 8]);
        assert_eq!(indexed(255), [238, 238, 238]);
        for i in 0..=255u8 {
            let _ = indexed(i); // must not panic anywhere in the range
        }
    }

    #[test]
    fn default_colors_come_from_the_editor_theme() {
        let (fg, bg) = ([1, 2, 3], [4, 5, 6]);
        assert_eq!(
            resolve(Color::Named(NamedColor::Foreground), fg, bg, false),
            fg
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::Background), fg, bg, false),
            bg
        );
        // ...but a real colour is its own
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), fg, bg, false),
            indexed(1)
        );
        // bold brightens, which is what programs using it expect
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), fg, bg, true),
            indexed(9)
        );
        // and truecolor passes straight through
        let spec = alacritty_terminal::vte::ansi::Rgb {
            r: 10,
            g: 20,
            b: 30,
        };
        assert_eq!(resolve(Color::Spec(spec), fg, bg, false), [10, 20, 30]);
    }

    #[test]
    fn a_screen_flattens_to_trimmed_text() {
        let (fg, bg) = ([0; 3], [0; 3]);
        let blank = Cell::blank(fg, bg);
        let mut screen = Screen {
            rows: 3,
            cols: 4,
            cells: vec![blank; 12],
            cursor: None,
        };
        for (i, c) in "hi".chars().enumerate() {
            screen.cells[i] = Cell { c, ..blank };
        }
        // trailing blanks on the line go, and so do the empty rows below it
        assert_eq!(screen.to_text(), "hi");

        screen.cells[2 * 4] = Cell { c: 'z', ..blank };
        assert_eq!(screen.to_text(), "hi\n\nz");
    }

    /// The check that lets a menu say "not installed" instead of forking a PTY
    /// that dies before its first frame. `sh` is on every machine this builds
    /// on; the negative is a name nothing could plausibly own.
    #[test]
    fn which_finds_a_program_on_the_path() {
        assert!(which("sh").is_some());
        assert!(which("zemacs-no-such-harness-9f3c").is_none());
        // A name with a separator is a path, taken as one either way.
        assert_eq!(which("/bin/sh"), Some(PathBuf::from("/bin/sh")));
        assert_eq!(which("/bin/zemacs-no-such-harness-9f3c"), None);
    }

    /// Deliberately *not* a PTY: the failure has to come before the fork, or
    /// the caller gets "No such file or directory" with no idea what was
    /// missing.
    #[test]
    fn spawning_a_missing_program_reports_the_name() {
        let Err(err) = Terminal::spawn_command(
            80,
            24,
            None,
            Some(Command::new("zemacs-no-such-harness-9f3c", vec![])),
        ) else {
            panic!("nothing by that name exists, so this must not have spawned");
        };
        assert!(
            err.to_string().contains("zemacs-no-such-harness-9f3c"),
            "{err}"
        );
        assert!(err.to_string().contains("not installed"), "{err}");
    }

    /// Two PTYs at once, each with its own child, each remembering what it was
    /// asked to run — the foundation everything else in AI mode stands on.
    #[test]
    fn sessions_are_independent() {
        let one = Terminal::spawn_command(
            40,
            10,
            None,
            Some(Command::new("cat", vec![])),
        )
        .expect("cat exists");
        let two = Terminal::spawn_command(
            80,
            24,
            None,
            Some(Command::new("sh", vec!["-c".into(), "sleep 30".into()])),
        )
        .expect("sh exists");

        assert_eq!(one.size(), (40, 10));
        assert_eq!(two.size(), (80, 24), "sizing one does not size the other");
        assert_eq!(one.command().map(|c| c.program.as_str()), Some("cat"));
        assert_eq!(two.command().unwrap().args, vec!["-c", "sleep 30"]);
        assert!(!one.exited() && !two.exited());

        // Dropping one hangs up on *its* child only; the other keeps its PTY.
        drop(one);
        assert!(!two.exited());
    }
}
