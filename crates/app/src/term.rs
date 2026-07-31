//! The terminal half that needs a process, mirroring [`crate::magit`] and
//! [`crate::dired`].
//!
//! `zemacs-term` knows PTYs and escape sequences; core knows the buffer kind and
//! the mode. This is the seam: it forks the shell, feeds it keystrokes, and
//! flattens its grid back into buffer text so that the buffer switcher, the
//! modeline and `buffer-string` all work on a terminal without a special case.
//! The *colour* does not survive that flattening, so the renderer reads the
//! grid directly — see [`Term::screen`].
//!
//! One terminal, not many: the same call the magit and dired buffers make. A
//! second one needs a terminal per buffer id rather than per app, and nothing
//! yet asks for it.

use std::path::PathBuf;

use zemacs_core::{BufferKind, Editor, EditorCommand, Key, Mode};
use zemacs_term::{Input, Mouse, Screen, Terminal};

/// Rows and columns to start with, before the renderer has measured a pane.
/// Replaced on the first frame; the shell only sees the real size.
const INITIAL: (usize, usize) = (80, 24);

#[derive(Default)]
pub struct Term {
    inner: Option<Terminal>,
    /// The editor has the keyboard and the buffer holds the scrollback, so the
    /// shell must not rewrite it out from under the cursor.
    frozen: bool,
}

impl Term {
    /// True while a terminal is alive, so the app knows to pump it.
    pub fn is_live(&self) -> bool {
        self.inner.is_some()
    }

    pub fn run(&mut self, editor: &mut Editor, verb: &str) {
        match verb {
            "open" => self.open(editor),
            "close" => self.close(editor),
            // Leaving and re-entering the shell. `terminal-normal` in core sets
            // the mode; these are what put the right *text* in the buffer for
            // it, which is the half core cannot do.
            "normal" => self.freeze(editor),
            "insert" => self.thaw(editor),
            other => editor.apply(EditorCommand::Message(format!(
                "unknown terminal verb: {other}"
            ))),
        }
    }

    fn open(&mut self, editor: &mut Editor) {
        if self.inner.is_none() {
            // Start where the current file is, the way `M-x shell` does — a
            // terminal that opens in the wrong directory is a terminal you
            // immediately have to `cd` in.
            let cwd = editor
                .buffer
                .path
                .as_deref()
                .and_then(|p| if p.is_dir() { Some(p) } else { p.parent() })
                .map(PathBuf::from)
                .or_else(|| std::env::current_dir().ok());
            match Terminal::spawn(INITIAL.0, INITIAL.1, cwd) {
                Ok(term) => self.inner = Some(term),
                Err(e) => {
                    editor.apply(EditorCommand::Message(format!("terminal: {e:#}")));
                    return;
                }
            }
        }
        // Whether the terminal is new or was parked in Normal mode, opening it
        // means the shell has the keyboard again — without this the buffer
        // keeps showing a frozen scrollback and nothing typed ever appears.
        self.frozen = false;
        editor.show_special(BufferKind::Terminal, "");
        editor.apply(EditorCommand::SetMode(Mode::Terminal));
    }

    fn close(&mut self, editor: &mut Editor) {
        // Dropping it is what shuts the reader thread down and hangs up on the
        // shell; see `Terminal::drop`.
        self.inner = None;
        if editor.buffer.kind == BufferKind::Terminal {
            editor.apply(EditorCommand::SetMode(Mode::Normal));
            editor.apply(EditorCommand::SwitchBuffer(1));
        }
    }

    /// Send a keystroke to the shell. Returns false when the key is not one a
    /// terminal can carry, which is how the splits keep working inside it.
    /// Type something at the shell. Used to run a project's build command,
    /// which is then a command in a terminal like any other rather than a
    /// second process-spawning path whose output nobody can see.
    pub fn send(&self, bytes: Vec<u8>) {
        if let Some(term) = &self.inner {
            term.send(bytes);
        }
    }

    pub fn key(&mut self, key: Key) -> bool {
        let Some(term) = &self.inner else {
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
            // shell has the keyboard.
            Key::CtrlMeta(_) | Key::CtrlEnter | Key::CtrlMetaEnter => return false,
        };
        term.input(input);
        true
    }

    /// A wheel notch over the terminal. `lines` is positive downward, the way
    /// the editor counts; the grid counts up its history, hence the negation.
    pub fn wheel(&self, lines: i32, col: usize, row: usize) {
        if let Some(term) = &self.inner {
            term.wheel(-lines, col, row);
        }
    }

    /// A click or drag over the terminal, in cell coordinates. False when the
    /// child does not want mouse events, so the caller can fall back to
    /// whatever the editor would have done.
    pub fn mouse(&self, mouse: Mouse) -> bool {
        self.inner.as_ref().is_some_and(|term| term.mouse(mouse))
    }

    /// Hand the keyboard back to the editor, with the whole scrollback in the
    /// buffer so the motions have something to move through.
    ///
    /// Live updates stop while this is up: the shell would otherwise rewrite the
    /// buffer under the cursor sixty times a second, which is what made every
    /// vim key look broken here.
    pub fn freeze(&mut self, editor: &mut Editor) {
        let Some(term) = &self.inner else { return };
        if editor.buffer.kind != BufferKind::Terminal {
            return;
        }
        self.frozen = true;
        let text = term.history_text();
        editor.show_special(BufferKind::Terminal, &text);
        editor.apply(EditorCommand::SetMode(Mode::Normal));
        // Land at the bottom, where the prompt is — that is what was on screen
        // a moment ago, and starting at line 1 of a 10,000-line scrollback is
        // never what was meant.
        editor.buffer.move_to_line_col(editor.buffer.len_lines(), 0);
    }

    /// Give the keyboard back to the shell.
    pub fn thaw(&mut self, editor: &mut Editor) {
        if self.inner.is_none() {
            return;
        }
        self.frozen = false;
        editor.apply(EditorCommand::SetMode(Mode::Terminal));
    }

    /// Resize to fit the pane, drain the child's requests, and refresh the
    /// buffer text.
    ///
    /// Called every frame. `poll` is the part that must not be skipped: a
    /// program asking the terminal how big it is blocks until the answer is
    /// written back.
    pub fn sync(&mut self, editor: &mut Editor, cols: usize, rows: usize) {
        let Some(term) = &mut self.inner else { return };
        term.resize(cols, rows);
        term.poll();

        if term.exited() {
            self.close(editor);
            editor.apply(EditorCommand::Message("terminal: shell exited".into()));
            return;
        }

        // Frozen means the buffer is showing the scrollback for the editor to
        // work on; rewriting it would move the cursor under the user's hands.
        if self.frozen || editor.buffer.kind != BufferKind::Terminal {
            return;
        }
        let text = term.screen(fg(editor), bg(editor)).to_text();
        // Only when it changed: `show_special` bumps the revision, and doing
        // that every frame would have the syntax thread reparsing a terminal
        // sixty times a second.
        if editor.buffer.text != text {
            editor.show_special(BufferKind::Terminal, &text);
        }
        editor.buffer.major_mode = "terminal-mode".into();
    }

    /// The live grid, for the renderer to draw in colour.
    ///
    /// `None` while frozen, which is what makes the renderer fall back to the
    /// ordinary text path — and so the selection, the cursor and the line
    /// numbers all appear, which is the whole point of stepping out of the
    /// shell.
    pub fn screen(&self, editor: &Editor) -> Option<Screen> {
        let term = self.inner.as_ref()?;
        (!self.frozen).then(|| term.screen(fg(editor), bg(editor)))
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

    #[test]
    fn editor_colors_convert_to_bytes() {
        assert_eq!(to_bytes([0.0, 0.5, 1.0]), [0, 128, 255]);
        // out-of-range values clamp rather than wrapping around
        assert_eq!(to_bytes([-1.0, 2.0, 0.0]), [0, 255, 0]);
    }

    /// The window splits have to keep working while a shell has the keyboard,
    /// so they are the keys a terminal deliberately does not take.
    #[test]
    fn the_editors_own_keys_are_not_sent_to_the_shell() {
        let mut term = Term::default();
        // With no terminal running nothing is claimed at all.
        assert!(!term.key(Key::Char('x')));
        assert!(!term.key(Key::CtrlEnter));
        assert!(!term.key(Key::CtrlMetaEnter));
        assert!(!term.key(Key::CtrlMeta('j')));
    }
}
