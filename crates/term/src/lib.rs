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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use alacritty_terminal::event::{Event, EventListener, Notify, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Grid, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
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

/// A stretch of scrollback the child gave the same attributes, in **char**
/// offsets into the text [`Terminal::history`] hands back beside it.
///
/// A run rather than a cell, because the frozen view turns each of these into an
/// overlay and a 10,000-line scrollback is two million cells. `None` is "the
/// editor's own", which is what an overlay's `None` already means — so a plain
/// shell line produces no run at all and the buffer carries overlays only where
/// the child asked for something.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Run {
    pub start: usize,
    pub end: usize,
    pub fg: Option<[u8; 3]>,
    pub bg: Option<[u8; 3]>,
    pub bold: bool,
    pub italic: bool,
}

/// What a press selects: the cell under it, the word, or the whole line — one
/// click, two, three, the way every terminal has done it since X.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Select {
    Cell,
    Word,
    Line,
}

impl Select {
    /// SDL counts clicks; this is what the count means.
    pub fn from_clicks(clicks: u8) -> Self {
        match clicks {
            1 => Select::Cell,
            2 => Select::Word,
            _ => Select::Line,
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
    /// `⌥←` / `⌥→` — a word at a time. Their own variants rather than an `Alt`
    /// wrapping a `Left`, because a modified arrow is a *different escape
    /// sequence* rather than an ESC glued to the front of one: see [`encode`].
    AltLeft,
    AltRight,
    /// The block above the arrows, none of which used to reach a child at all.
    /// Home and End are readline's line-start and line-end, the page keys move
    /// a full-screen program a screenful, and `Delete` is forward-delete —
    /// nothing else in this enum does any of those.
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    /// `F1`–`F12`, the menu bar of every TUI. Numbered rather than twelve
    /// variants, as the editor's own `Key::F` is; outside that range [`encode`]
    /// sends nothing, because there is nothing to send.
    F(u8),
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
        // `CSI 1 ; 3 C` — xterm's modifyOtherKeys form, where the `3` is
        // "Alt". Always `CSI` and never `SS3`: DECCKM only ever governed the
        // *unmodified* arrows, and a modified one has parameters, which `ESC O`
        // has no room for. This is what readline reads as forward-word and what
        // an agent's input box reads as a word jump.
        Input::AltRight => vec![0x1b, b'[', b'1', b';', b'3', b'C'],
        Input::AltLeft => vec![0x1b, b'[', b'1', b';', b'3', b'D'],
        // Home and End go through `arrow` because they are arrows as far as
        // DECCKM is concerned: `ESC [ H`/`ESC [ F` normally, `ESC O H`/`ESC O F`
        // in application mode, which is what `infocmp xterm-256color` lists as
        // `khome`/`kend` — and `child_env` sets exactly that TERM. The VT220
        // `ESC [ 1 ~`/`ESC [ 4 ~` spellings are the other tradition and are
        // deliberately not what we send.
        Input::Home => arrow(b'H'),
        Input::End => arrow(b'F'),
        // The `~` family carries its own number, so there is no room for a mode
        // to change it: these three are the same bytes either way. `kpp`, `knp`
        // and `kdch1`.
        Input::PageUp => vec![0x1b, b'[', b'5', b'~'],
        Input::PageDown => vec![0x1b, b'[', b'6', b'~'],
        Input::Delete => vec![0x1b, b'[', b'3', b'~'],
        // F1–F4 are SS3 and the rest are the `~` family, which is history
        // rather than design — and the numbers skip 16 and 22, which is the
        // part everyone gets wrong. Read off `kf1`–`kf12` of xterm-256color.
        Input::F(n) => match n {
            1..=4 => vec![0x1b, b'O', b'P' + (n - 1)],
            5..=12 => {
                let code = [15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5];
                format!("\x1b[{code}~").into_bytes()
            }
            _ => vec![],
        },
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
struct Proxy {
    events: Sender<Event>,
    /// Shared with the [`Terminal`] — see its `dirty`. Set here, on the PTY
    /// thread, so the flag is what decides whether the child's output is worth
    /// a wakeup: a grid nobody has looked at since the last one is not.
    dirty: Arc<AtomicBool>,
}

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        // The child printed. Wake the loop only if it has not already been
        // told: a session parked behind another buffer keeps its flag until
        // someone switches to it, so a build spewing into a background pane
        // costs one wakeup and then none, and the visible one costs at most one
        // per frame. Everything else — a size query the child is blocked on,
        // the exit — is rare and wakes unconditionally.
        let wake = match event {
            Event::Wakeup => !self.dirty.swap(true, Ordering::Relaxed),
            _ => true,
        };
        let _ = self.events.send(event);
        if wake {
            if let Some(f) = WAKE.get() {
                f();
            }
        }
    }
}

/// Who to tell when a child prints, so the main loop can park instead of
/// polling every session at the display's rate for as long as one exists.
///
/// The same shape as `zemacs_core::set_waker`, and installed with it: this
/// crate knows nothing of the editor, so it cannot call that one itself. `None`
/// — every test, every headless caller — means the events queue up and the
/// next `poll` finds them, which is what always happened.
static WAKE: OnceLock<fn()> = OnceLock::new();

/// Install the waker. The second call is ignored.
pub fn set_waker(f: fn()) {
    let _ = WAKE.set(f);
}

pub struct Terminal {
    term: Arc<FairMutex<Term<Proxy>>>,
    notifier: Notifier,
    events: Receiver<Event>,
    cols: usize,
    rows: usize,
    title: String,
    exited: bool,
    /// Whether the grid may have changed since the last time anyone flattened
    /// it, so the app can skip the flattening when it cannot have.
    ///
    /// Copying the screen out is not cheap — `rows * cols` cells out from under
    /// alacritty's lock, then a `String` per row — and the app used to do it on
    /// every frame for as long as a terminal buffer was on screen, which is to
    /// say sixty times a second at a shell prompt nobody was typing at. The
    /// grid changes when the child prints, and the child printing is exactly
    /// what [`alacritty_terminal::event::Event::Wakeup`] means.
    ///
    /// True to begin with, so the first screenful goes up without waiting for
    /// the child to say anything.
    dirty: Arc<AtomicBool>,
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
            env: child_env(),
        };
        let pty = tty::new(&options, window_size(cols, rows), 0)?;

        let (tx, events) = channel();
        // True to begin with: the first screenful goes up without waiting for
        // the child to say anything — see `dirty`.
        let dirty = Arc::new(AtomicBool::new(true));
        let proxy = Proxy { events: tx, dirty: dirty.clone() };
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
            dirty,
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

    /// Begin a text selection in the grid, at the cell `(col, row)` of the
    /// visible screen. `right` is which half of that cell the pointer was in,
    /// which is the difference between a selection that includes the character
    /// under the press and one that starts after it.
    ///
    /// This is the editor's own selection, not the child's: a program that has
    /// claimed the mouse never hears about it. That is deliberate and it is what
    /// every terminal emulator does — see the shift-bypass in `main.rs`, without
    /// which text inside `vim`, `htop` or an agent's pane could not be copied at
    /// all.
    pub fn select_start(&self, col: usize, row: usize, right: bool, kind: Select) {
        let mut term = self.term.lock();
        let point = point_at(&term, col, row);
        let ty = match kind {
            Select::Cell => SelectionType::Simple,
            Select::Word => SelectionType::Semantic,
            Select::Line => SelectionType::Lines,
        };
        term.selection = Some(Selection::new(ty, point, side(right)));
    }

    /// Drag the far end of the selection to `(col, row)`. Silent with no
    /// selection started, so a stray motion cannot invent one.
    pub fn select_update(&self, col: usize, row: usize, right: bool) {
        let mut term = self.term.lock();
        let point = point_at(&term, col, row);
        if let Some(selection) = term.selection.as_mut() {
            selection.update(point, side(right));
        }
    }

    pub fn select_clear(&self) {
        self.term.lock().selection = None;
    }

    /// What is selected, or `None` when nothing is — which is also the answer
    /// for a selection of only blank cells, since copying a rectangle of spaces
    /// is never what the gesture meant.
    pub fn selection_text(&self) -> Option<String> {
        let text = self.term.lock().selection_to_string()?;
        (!text.trim().is_empty()).then_some(text)
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
                // "New terminal content available" — the child printed. The
                // flag was already raised on the way in, by `Proxy`, since
                // raising it is what decided whether to wake anyone.
                Event::Wakeup => {}
                // Bell, clipboard, colour queries and cursor-blink changes are
                // ignored deliberately — none of them has anywhere to go yet.
                _ => {}
            }
        }
    }

    /// Whether the grid changed since the last ask, clearing the flag.
    ///
    /// Taken rather than read so the caller cannot forget to clear it, and
    /// cleared *late* — the app asks after it has decided the buffer is worth
    /// refreshing at all, so a session parked behind another buffer keeps its
    /// flag and catches up whole when you switch back to it.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    /// The grid changed for a reason the child did not announce.
    fn mark_dirty(&self) {
        self.dirty
            .store(true, Ordering::Relaxed);
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
        // A reflow rewrites the grid without the child having said a word, so
        // it is the one change `Event::Wakeup` does not cover.
        self.mark_dirty();
        self.term.lock().resize(Size { cols, rows });
        let _ = self.notifier.0.send(Msg::Resize(window_size(cols, rows)));
    }

    /// Scroll through the scrollback. Positive is toward the top of the
    /// history, matching the editor's own wheel handling.
    pub fn scroll(&self, lines: i32) {
        // Moving the viewport through the scrollback shows different rows
        // without the child having printed any — the second of the two changes
        // `Event::Wakeup` does not cover. See [`Terminal::dirty`].
        self.mark_dirty();
        self.term.lock().scroll_display(Scroll::Delta(lines));
    }

    /// Everything the terminal remembers — the whole scrollback followed by the
    /// visible screen — as text, and as the colour that text was printed in:
    /// one [`Run`] per stretch of cells sharing their attributes.
    ///
    /// This is what goes in the buffer when the editor takes the keyboard back,
    /// and it is the only reason the motions have anything to work on — the
    /// visible grid alone is one screenful with no history to search, yank from,
    /// or jump around in. The runs are what stops that buffer being the grey
    /// flattening it used to be: see `show_history` in `zemacs-app`.
    ///
    /// One function and not two, which is the whole design of it: every row here
    /// loses its trailing blanks and the blank rows below the prompt are dropped
    /// altogether, so a second pass that re-derived either would have to agree
    /// with this one about both — and [`Screen::to_text`] beside [`Screen`] is
    /// the standing example of what happens when two such passes drift. The
    /// offsets are counted off the text as it is built, so they cannot.
    ///
    /// Char offsets, because that is what the rope and an overlay both count.
    pub fn history(&self, fg: [u8; 3], bg: [u8; 3]) -> (String, Vec<Run>) {
        let term = self.term.lock();
        let grid = term.grid();
        let (rows, cols) = (grid.screen_lines(), grid.columns());
        let first = -(grid.history_size() as i32);

        let mut out = String::new();
        let mut runs = Vec::new();
        let mut at = 0usize;
        let mut cells: Vec<(char, Attrs)> = Vec::with_capacity(cols);
        for row in first..rows as i32 {
            let line = &grid[Line(row)];
            cells.clear();
            cells.extend(
                (0..cols)
                    .map(|c| &line[Column(c)])
                    // A wide character owns two columns and the spacer after it
                    // has no glyph; the text takes one char for the pair.
                    .filter(|c| !c.flags.contains(Flags::WIDE_CHAR_SPACER))
                    .map(|c| (c.c, attrs(c, fg, bg))),
            );
            // `trim_end`'s rule — the same `char::is_whitespace` it uses —
            // asked of the cells, so the offsets below are measured against the
            // text that lands in the buffer rather than against the grid.
            let used = cells
                .iter()
                .rposition(|(c, _)| !c.is_whitespace())
                .map_or(0, |i| i + 1);
            out.extend(cells[..used].iter().map(|&(c, _)| c));
            out.push('\n');

            let mut col = 0;
            while col < used {
                let a = cells[col].1;
                let end = cells[col..used]
                    .iter()
                    .position(|&(_, b)| b != a)
                    .map_or(used, |n| col + n);
                // Nothing to say about a cell the child left alone: that is
                // most of a scrollback, and an overlay per row of plain output
                // would be a bill the renderer pays every frame.
                if a != (fg, bg, false, false) {
                    runs.push(Run {
                        start: at + col,
                        end: at + end,
                        fg: (a.0 != fg).then_some(a.0),
                        bg: (a.1 != bg).then_some(a.1),
                        bold: a.2,
                        italic: a.3,
                    });
                }
                col = end;
            }
            at += used + 1;
        }
        // Blank rows below the prompt are not history, and landing on them with
        // `G` would look like the buffer had lost its contents. No run reaches
        // into what this drops — a blank row trimmed to nothing has none.
        let keep = out.trim_end_matches('\n').len();
        out.truncate(keep);
        (out, runs)
    }

    /// Copy the visible grid out. `fg`/`bg` are the editor's own colours, used
    /// wherever the child asked for "default", so a terminal pane matches the
    /// theme instead of being a black rectangle in the middle of it.
    pub fn screen(&self, fg: [u8; 3], bg: [u8; 3]) -> Screen {
        let term = self.term.lock();
        let selection = term.selection.as_ref().and_then(|s| s.to_range(&term));
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
                let (mut fg_c, mut bg_c, bold, italic) = attrs(cell, fg, bg);
                // A selection reads as reverse video, which is what a terminal
                // has always done and the one highlight that is legible against
                // any theme and any program's own colours.
                //
                // ponytail: not a themed selection face. Ceiling: a config that
                // wants to colour it. The upgrade path is a third colour
                // argument here, beside `fg` and `bg`, which the app already
                // reads out of the theme to pass the other two.
                if selection
                    .as_ref()
                    .is_some_and(|r| r.contains(Point::new(line, Column(col))))
                {
                    std::mem::swap(&mut fg_c, &mut bg_c);
                }
                cells[row * cols + col] = Cell {
                    c: cell.c,
                    fg: fg_c,
                    bg: bg_c,
                    bold,
                    italic,
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

/// The grid point under cell `(col, row)` of the *visible* screen.
///
/// Clamped rather than fallible: a drag runs off the edge of the pane all the
/// time, and the selection it makes should stop at the last column rather than
/// stop existing. `display_offset` is what makes a selection made while scrolled
/// back name the scrollback line the eye is on, not the one the child is
/// writing.
fn point_at<T>(term: &Term<T>, col: usize, row: usize) -> Point {
    let grid = term.grid();
    let row = row.min(grid.screen_lines().saturating_sub(1));
    let col = col.min(grid.columns().saturating_sub(1));
    Point::new(
        Line(row as i32 - grid.display_offset() as i32),
        Column(col),
    )
}

/// Which half of a cell the pointer is in. Sub-cell precision only a selection
/// needs: a mouse *report* names a cell and stops there.
fn side(right: bool) -> Side {
    if right {
        Side::Right
    } else {
        Side::Left
    }
}

/// What every child is told about the terminal it is running in. Added to the
/// environment zemacs was launched with rather than replacing it, which is what
/// `tty::Options::env` means.
///
/// `TERM` is not `alacritty`, which is the library's default: that terminfo
/// entry only exists where Alacritty is installed, and a shell that cannot find
/// its `TERM` entry loses colour, arrow keys and clear-screen. `xterm-256color`
/// is everywhere.
///
/// `COLORTERM` is the one that decides whether an agent's code blocks are
/// syntax-highlighted at all. Terminfo has no capability for 24-bit colour, so
/// the convention every emulator settled on is this variable — and the libraries
/// the harnesses are built from read it directly: Node's `supports-color`, which
/// is what `chalk` and every Ink CLI ask, reports truecolor only when it is set
/// and otherwise falls back to a 256-colour level that most highlighting themes
/// answer by emitting *no* styling.
///
/// It is set here rather than inherited because inheriting it is the bug: a
/// zemacs started from a terminal picks the variable up from that terminal and
/// the highlighting works, while the same zemacs started from the Dock gets the
/// bare GUI environment and the same harness prints flat text. Nothing about the
/// grid differs between those two — `resolve` has always handled `Color::Spec` —
/// so the colour was never arriving in the first place.
fn child_env() -> HashMap<String, String> {
    HashMap::from([
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
    ])
}

// --- colour ---------------------------------------------------------------

/// What one grid cell is drawn in: foreground, background, bold, italic.
type Attrs = ([u8; 3], [u8; 3], bool, bool);

/// Everything about a cell that is not its character, resolved against the
/// editor's own colours.
///
/// Shared by the live view and the frozen one, and it has to be: reverse video
/// and `HIDDEN` are the two rules a second copy would forget, and an agent's
/// selected menu item is drawn with the first of them. [`Screen`]'s own
/// selection is the one thing left outside — it belongs to the *view*, not to
/// the cell, and there is no selection in a scrollback.
fn attrs(cell: &alacritty_terminal::term::cell::Cell, fg: [u8; 3], bg: [u8; 3]) -> Attrs {
    let flags = cell.flags;
    let bold = flags.contains(Flags::BOLD);
    let (mut fg_c, mut bg_c) = (
        resolve(cell.fg, fg, bg, bold),
        resolve(cell.bg, fg, bg, false),
    );
    if flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg_c, &mut bg_c);
    }
    if flags.contains(Flags::HIDDEN) {
        fg_c = bg_c;
    }
    (fg_c, bg_c, bold, flags.contains(Flags::ITALIC))
}

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

    /// Home and End are arrows as far as DECCKM is concerned, which is the one
    /// thing about them that is easy to get wrong: readline turns the mode on,
    /// and `ESC [ H` there is not what Home means.
    ///
    /// Checked against `infocmp xterm-256color` — `khome=\EOH`, `kend=\EOF` —
    /// which is the terminal `child_env` claims to be. The VT220 `ESC [ 1 ~`
    /// and `ESC [ 4 ~` are the other tradition and deliberately not these.
    #[test]
    fn home_and_end_follow_application_cursor_mode() {
        assert_eq!(encode(Input::Home, false), b"\x1b[H".to_vec());
        assert_eq!(encode(Input::End, false), b"\x1b[F".to_vec());
        assert_eq!(encode(Input::Home, true), b"\x1bOH".to_vec());
        assert_eq!(encode(Input::End, true), b"\x1bOF".to_vec());
    }

    /// The `~` family carries its own number, so no mode can change it: `kpp`,
    /// `knp` and `kdch1` of xterm-256color, the same bytes either way.
    #[test]
    fn the_page_keys_and_forward_delete_ignore_the_cursor_mode() {
        for app in [false, true] {
            assert_eq!(encode(Input::PageUp, app), b"\x1b[5~".to_vec());
            assert_eq!(encode(Input::PageDown, app), b"\x1b[6~".to_vec());
            assert_eq!(encode(Input::Delete, app), b"\x1b[3~".to_vec());
        }
        // Forward delete is not Backspace, however macOS labels the key.
        assert_ne!(encode(Input::Delete, false), encode(Input::Backspace, false));
    }

    /// `kf1`–`kf12`, read off xterm-256color. F1–F4 are SS3 and the rest are
    /// the `~` family — and the numbers skip 16 and 22, which is the part
    /// everybody gets wrong.
    #[test]
    fn the_function_keys_match_terminfo_gaps_and_all() {
        let want: [&[u8]; 12] = [
            b"\x1bOP",
            b"\x1bOQ",
            b"\x1bOR",
            b"\x1bOS",
            b"\x1b[15~",
            b"\x1b[17~",
            b"\x1b[18~",
            b"\x1b[19~",
            b"\x1b[20~",
            b"\x1b[21~",
            b"\x1b[23~",
            b"\x1b[24~",
        ];
        for (i, bytes) in want.iter().enumerate() {
            let n = i as u8 + 1;
            assert_eq!(encode(Input::F(n), false), bytes.to_vec(), "F{n}");
            // Application-cursor mode governs the arrows and nothing else.
            assert_eq!(encode(Input::F(n), true), bytes.to_vec(), "F{n} in app mode");
        }
        // Nothing outside the twelve, rather than bytes a child would read as
        // some other key entirely.
        assert_eq!(encode(Input::F(0), false), Vec::<u8>::new());
        assert_eq!(encode(Input::F(13), false), Vec::<u8>::new());
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

    /// Dragging out text and getting it back, which is the whole of what a
    /// selection is for. The grid, not the buffer: the buffer's flattening of
    /// this screen is rewritten every time the child prints.
    ///
    /// Also the proof that the sub-cell side is wired up — a drag that ends on
    /// the *right* half of the `o` includes it, and one ending on the left half
    /// stops before it. That one character is the difference between copying a
    /// path and copying a path with its last letter missing.
    #[test]
    fn a_drag_selects_cells_and_a_double_click_selects_the_word() {
        let mut term = Terminal::spawn_command(
            40,
            4,
            None,
            Some(Command::new("printf", vec!["hello world".into()])),
        )
        .expect("printf must be on $PATH");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            term.poll();
            if term.screen([0; 3], [0; 3]).cell(0, 0).map(|c| c.c) == Some('h') {
                break;
            }
            assert!(Instant::now() < deadline, "the child never printed");
            std::thread::sleep(Duration::from_millis(10));
        }

        term.select_start(0, 0, false, Select::Cell);
        term.select_update(4, 0, true);
        assert_eq!(term.selection_text().as_deref(), Some("hello"));
        term.select_update(4, 0, false);
        assert_eq!(term.selection_text().as_deref(), Some("hell"));

        // A press in the middle of a word takes the word, which is what the
        // second click of a double-click means everywhere.
        term.select_start(8, 0, false, Select::Word);
        assert_eq!(term.selection_text().as_deref(), Some("world"));

        // ...and the third takes the line, newline and all — a line yanked
        // without its break pastes into the middle of whatever it lands on.
        term.select_start(8, 0, false, Select::Line);
        assert_eq!(term.selection_text().as_deref(), Some("hello world\n"));

        // What the eye sees: reverse video over the selected cells and nothing
        // over the rest, since the renderer draws `Screen` and knows no more
        // about a selection than it does about a hyperlink.
        term.select_start(0, 0, false, Select::Cell);
        term.select_update(4, 0, true);
        let screen = term.screen([1, 2, 3], [9, 8, 7]);
        let selected = screen.cell(0, 0).expect("the grid is 40 wide");
        assert_eq!((selected.fg, selected.bg), ([9, 8, 7], [1, 2, 3]), "reversed");
        let plain = screen.cell(0, 6).expect("the grid is 40 wide");
        assert_eq!((plain.fg, plain.bg), ([1, 2, 3], [9, 8, 7]), "past the selection");

        term.select_clear();
        assert_eq!(term.selection_text(), None);
    }

    /// The two variables a harness reads before it decides whether to colour
    /// anything. Asserted on the map rather than on a live child on purpose: the
    /// bug being guarded against is *inheritance*, and a test process started
    /// from a terminal has `COLORTERM` set already, so a child that echoed it
    /// back would pass here and still print flat text from the Dock.
    #[test]
    fn a_child_is_told_it_has_a_terminal_and_that_it_has_true_colour() {
        let env = child_env();
        assert_eq!(env.get("TERM").map(String::as_str), Some("xterm-256color"));
        assert_eq!(env.get("COLORTERM").map(String::as_str), Some("truecolor"));
    }

    /// ...and the other half: a 24-bit SGR sequence has to survive the grid as
    /// the exact colour asked for. Nothing rounds it to the 256-colour cube, so
    /// a highlighting theme's greys stay distinguishable.
    #[test]
    fn a_24_bit_colour_reaches_the_cell_unrounded() {
        let mut term = Terminal::spawn_command(
            20,
            2,
            None,
            // `printf` and not `echo -e`: `/bin/sh` is `dash` on some systems
            // and its `echo` does not read the escapes.
            Some(Command::new(
                "printf",
                vec!["\\033[38;2;17;34;51mx\\033[0m".into()],
            )),
        )
        .expect("printf must be on $PATH");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            term.poll();
            let screen = term.screen([0; 3], [0; 3]);
            if let Some(cell) = screen.cell(0, 0).filter(|c| c.c == 'x') {
                assert_eq!(cell.fg, [17, 34, 51], "the exact RGB, not a palette match");
                return;
            }
            assert!(Instant::now() < deadline, "the child never printed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The frozen half of the same story: the buffer gets the scrollback as
    /// text, and the runs beside it have to point at exactly the characters the
    /// child coloured — an offset a column out paints the wrong letter on every
    /// line, which is the whole reason the text and the runs come from one pass.
    ///
    /// Three things at once, because they are the three ways the offsets can
    /// drift: a wide character is two columns and one char, a row loses its
    /// trailing blanks even when they were coloured, and a run that claims only
    /// emphasis is still a run.
    #[test]
    fn the_runs_of_a_frozen_scrollback_land_on_the_characters_that_were_coloured() {
        const FG: [u8; 3] = [1, 2, 3];
        const BG: [u8; 3] = [4, 5, 6];
        let mut term = Terminal::spawn_command(
            20,
            4,
            None,
            Some(Command::new(
                "printf",
                // 日本 in 24-bit blue, then plain text; then a bold word with
                // two coloured spaces after it that the trim must eat.
                vec!["\\033[38;2;17;34;51m日本\\033[0m x\\n\\033[1mbold  \\033[0m\\n".into()],
            )),
        )
        .expect("printf must be on $PATH");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            term.poll();
            let (text, runs) = term.history(FG, BG);
            if text == "日本 x\nbold" {
                // Char offsets, so 日本 is two of them and not six bytes.
                let of = |r: &Run| -> String {
                    text.chars().skip(r.start).take(r.end - r.start).collect()
                };
                assert_eq!(runs.len(), 2, "one per stretch, not one per cell: {runs:?}");
                assert_eq!(of(&runs[0]), "日本");
                assert_eq!(runs[0].fg, Some([17, 34, 51]), "the exact RGB");
                assert_eq!(runs[0].bg, None, "the child never asked for a background");
                assert!(!runs[0].bold);
                // The two spaces the child printed in bold are trimmed off the
                // text, so the run must stop where the text does.
                assert_eq!(of(&runs[1]), "bold");
                let want = Run {
                    start: 5,
                    end: 9,
                    fg: None,
                    bg: None,
                    bold: true,
                    italic: false,
                };
                assert_eq!(runs[1], want);
                return;
            }
            assert!(Instant::now() < deadline, "the child never printed; got {text:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A rectangle of blank cells is not a copy anybody meant, so it answers
    /// nothing and the app treats the gesture as the click it was.
    #[test]
    fn selecting_only_blank_cells_copies_nothing() {
        let mut term =
            Terminal::spawn_command(40, 4, None, Some(Command::new("true", vec![]))).unwrap();
        term.poll();
        term.select_start(10, 2, false, Select::Cell);
        term.select_update(20, 2, true);
        assert_eq!(term.selection_text(), None);
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
