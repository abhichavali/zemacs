//! The modal ("Evil") key grammar: `[count] operator motion`.
//!
//! `handle_key` is a pure translator — it reads the document to compute motion
//! targets but never mutates it. Everything it decides comes back as
//! [`EditorCommand`]s for [`Editor::apply`].
//!
//! With exactly one exception, and it earns its keep: replaying a macro applies
//! each key's commands before feeding the next, because a macro is a recording
//! of *decisions* and every decision after the first has to see what the one
//! before it did. `apply` is still the only writer — see [`Editor::replay`].
//!
//! Lookup order for every key, which is what makes the Lisp config authoritative:
//! prompt line → pending literal (`r`, `f`, `"`, `m`, `` ` ``, `q`, `@`) →
//! **user keymap** → built-in grammar.

use crate::{
    frame, BufferId, Direction, Editor, EditorCommand, Insertion, Key, MarkerId, Mode, Prompt,
    PromptKind,
};
use regex::Regex;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Delete,
    Change,
    Yank,
}

impl Op {
    fn char(self) -> char {
        match self {
            Op::Delete => 'd',
            Op::Change => 'c',
            Op::Yank => 'y',
        }
    }
}

/// Everything the grammar needs to remember between keystrokes.
#[derive(Default)]
pub(crate) struct Pending {
    pub count: Option<usize>,
    pub op: Option<Op>,
    /// Accumulated key-sequence tokens, for multi-key bindings (`g g`, `SPC f f`).
    pub keys: Vec<String>,
    /// Awaiting the target of `f`/`F`/`t`/`T`.
    pub find: Option<char>,
    /// Awaiting the replacement char of `r`.
    pub replace: bool,
    /// Awaiting the literal argument of `"`, `m`, `` ` ``, `'`, `q` or `@` —
    /// and *which* of them is waiting is the character itself, since none of
    /// them needs any other state to finish.
    pub literal: Option<char>,
}

impl Pending {
    pub fn clear(&mut self) {
        self.count = None;
        self.op = None;
        self.keys.clear();
        self.find = None;
        self.replace = false;
        self.literal = None;
    }

    fn count(&self) -> usize {
        self.count.unwrap_or(1)
    }

    /// A which-key-ish trail shown at the right of the status line.
    pub fn hint(&self) -> String {
        let mut s = String::new();
        if let Some(n) = self.count {
            s.push_str(&n.to_string());
        }
        if let Some(op) = self.op {
            s.push(op.char());
        }
        s.push_str(&self.keys.join(""));
        if let Some(f) = self.find {
            s.push(f);
        }
        if self.replace {
            s.push('r');
        }
        if let Some(l) = self.literal {
            s.push(l);
        }
        if s.is_empty() {
            String::new()
        } else {
            format!("   [{s}]")
        }
    }
}

/// How deep `@` may nest before it is called a runaway.
///
/// A macro that replays itself is the obvious way to write one, and it is a
/// *stack* overflow rather than an error the grammar could see coming — replay
/// re-enters `handle_key`. So the depth is capped instead of the recursion
/// detected: `@a` containing `@a` is legitimate right up until it does not
/// terminate, and nothing short of running it can tell the two apart.
const MACRO_DEPTH: usize = 20;

/// The vim state that outlives a keystroke: registers, macros, marks.
///
/// One struct rather than six fields on [`Editor`], because it is *this* file
/// that reads every one of them and none of it is the document — `apply` has
/// no business in any of it.
#[derive(Default)]
pub(crate) struct Vim {
    /// `"a`–`"z`, and whatever else was typed after `"`. The **unnamed**
    /// register is still `Editor::register` and is deliberately not in here:
    /// vim fills it on every yank and delete whatever register you named, so
    /// there are genuinely two things and not one map with a default key.
    registers: HashMap<char, (String, bool)>,
    /// The register named for the *next* verb, consumed by it.
    pending: Option<char>,
    macros: HashMap<char, Vec<Key>>,
    /// Which register `q` is currently filling, and what it has so far.
    recording: Option<(char, Vec<Key>)>,
    /// What `@@` repeats — the last macro *replayed*, not the last recorded.
    last_macro: Option<char>,
    /// Current `@` nesting, against [`MACRO_DEPTH`].
    depth: usize,
    /// `ma` — one marker per (buffer, letter).
    ///
    /// Keyed by buffer because vim's lowercase marks are per file and a marker
    /// only resolves in the buffer it was made in: a mark set elsewhere would
    /// otherwise read as this buffer's. ponytail: nothing removes the entry
    /// when a buffer goes away, so a long session leaks a handful of dead
    /// (id, char) pairs. They read as "mark not set", which is the right
    /// answer anyway; move the map onto `Buffer` if that ever stops being true.
    marks: HashMap<(BufferId, char), MarkerId>,
}

impl Vim {
    /// Write a register. **Uppercase appends**, as vim does — `"Ayy` adds to
    /// whatever `"ayy` put there.
    fn write(&mut self, name: char, text: String, linewise: bool) {
        if !name.is_ascii_uppercase() {
            self.registers.insert(name, (text, linewise));
            return;
        }
        let slot = self.registers.entry(name.to_ascii_lowercase()).or_default();
        // A linewise append has to land on its own line: without this, `"Ayy`
        // twice gives one long line rather than the two that were yanked.
        if (slot.1 || linewise) && !slot.0.is_empty() && !slot.0.ends_with('\n') {
            slot.0.push('\n');
        }
        slot.0.push_str(&text);
        slot.1 |= linewise;
    }

    /// Read a register. `"A` reads `"a`: only *writing* distinguishes the case.
    fn read(&self, name: char) -> Option<&(String, bool)> {
        self.registers.get(&name.to_ascii_lowercase())
    }
}

/// How an operator covers the span between the cursor and a motion target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Span {
    /// `[cursor, target)` — `w`, `0`, `b`.
    Exclusive,
    /// `[cursor, target]` — `e`, `$`, `f`.
    Inclusive,
    /// Whole lines between the two. `j`, `G`, `dd`.
    Linewise,
}

struct Motion {
    target: usize,
    span: Span,
}

/// Verbs the editor resolves itself, offered by `M-x` alongside whatever the
/// Lisp image publishes. Keep in step with the match in [`Editor::run_action`].
pub const BUILTIN_COMMANDS: &[&str] = &[
    "find-file",
    "switch-buffer",
    "execute-command",
    "scratch",
    "config",
    "eval-dwim",
    "eval-buffer",
    "eval-region",
    "eval-last-sexp",
    "split-window-right",
    "split-window-below",
    "other-window",
    "delete-window",
    "new-frame",
    "delete-frame",
    "magit-status",
    "magit-stage",
    "magit-unstage",
    "magit-stage-all",
    "magit-unstage-all",
    "magit-commit",
    "magit-commit-finish",
    "magit-push",
    "magit-pull",
    "magit-refresh",
    "dired",
    "dired-up",
    "dired-enter",
    "dired-mark",
    "dired-unmark",
    "dired-toggle-marks",
    "dired-flag-delete",
    "dired-execute",
    "dired-rename",
    "dired-copy",
    "dired-mkdir",
    "dired-toggle-hidden",
    "dired-refresh",
    "ace-window",
    "search-line",
    "search-project",
    "project-find-file",
    // Added with the filesystem-wide picker: `open` types a path to any
    // directory at all, `find-dir` picks one inside the current project.
    "project-open",
    "project-find-dir",
    "project-switch",
    "project-dired",
    "project-compile",
    "project-test",
    "project-root",
    "project-forget",
    "magit-toggle",
    "magit-amend",
    "magit-fetch",
    "magit-stash",
    "magit-stash-pop",
    "magit-rebase-continue",
    "magit-rebase-skip",
    "magit-rebase-abort",
    "terminal",
    "terminal-normal",
    "terminal-close",
    "quit",
];

impl Editor {
    /// Translate one key into zero or more commands.
    ///
    /// A wrapper around [`Editor::dispatch_key`] for one reason: the which-key
    /// panel is a picture of a *half-typed sequence*, so it has to be retired
    /// when the sequence is, and the sequence can end at a dozen places inside
    /// the dispatch. Asking afterwards — "is anything still pending?" — is one
    /// site instead of a dozen, and it is the right question at every one of
    /// them: a binding that fired, an `Esc`, a key that was not a prefix after
    /// all, all leave `pending.keys` empty.
    ///
    /// Only ever *emptied* here. A key that lengthens a sequence leaves the old
    /// rows up until the image sends the new ones, which is what stops the panel
    /// blinking once per keystroke on the Lisp round trip.
    pub fn handle_key(&mut self, key: Key) -> Vec<EditorCommand> {
        let cmds = self.dispatch_key(key);
        if self.pending.keys.is_empty() {
            self.which_key.clear();
        }
        cmds
    }

    fn dispatch_key(&mut self, key: Key) -> Vec<EditorCommand> {
        // `q` ends a recording, and is the one key a macro must never contain —
        // replaying it would start a recording over the top of itself. Checked
        // before anything else looks at the key, and only where `q` could have
        // *started* one: in a prompt or in Insert it is just a letter.
        if key == Key::Char('q') && self.recording_here() {
            return self.stop_recording();
        }
        // Record the keys, not the commands. A replay then re-runs the
        // *decisions* — the second `dw` deletes whatever word the cursor is on
        // now, which is the whole reason anyone records a macro.
        //
        // Nothing is recorded during a replay: `@a` typed while recording must
        // go in as `@a` and not as its expansion, or the macro grows by its own
        // length every time it runs.
        if self.vim.depth == 0 {
            if let Some((_, keys)) = self.vim.recording.as_mut() {
                keys.push(key);
            }
        }

        // Ace labels are up: this keystroke picks a window and nothing else.
        if let Some(labels) = self.ace.take() {
            let picked = match key {
                Key::Char(c) => labels.iter().find(|(l, _)| *l == c).map(|(_, id)| *id),
                _ => None,
            };
            return match picked {
                Some(id) => vec![EditorCommand::FocusWindow(id)],
                // Anything else cancels, rather than falling through and doing
                // something surprising with a key you aimed at a label.
                None => vec![EditorCommand::Message(String::new())],
            };
        }
        if self.prompt.is_some() {
            return self.prompt_key(key);
        }
        if self.mode == Mode::Dashboard {
            return self.dashboard_key(key);
        }
        if self.mode == Mode::Insert {
            return self.insert_key(key);
        }
        if self.mode == Mode::Terminal {
            return self.terminal_key(key);
        }
        self.normal_key(key)
    }

    // --- Terminal --------------------------------------------------------

    /// In a terminal the shell owns the keyboard, so this is the one mode whose
    /// keymap is consulted *instead of* the Evil grammar rather than before it:
    /// `j` has to type a `j`, and `d` has to type a `d`.
    ///
    /// Two ways an editor binding still fires. The Terminal keymap is checked
    /// first and always wins, so anything at all can be reclaimed by binding it
    /// in `"terminal"`. Failing that, keys a terminal has no use for — the ones
    /// carrying Command, plus the two modified Enters — fall through to the
    /// *Normal* keymap, which is what keeps `M-x`, `C-M-j`, `M-o` and the window
    /// splits alive inside a shell.
    ///
    /// Ctrl is deliberately not in that set. `C-c`, `C-a`, `C-d`, `C-r` and
    /// `C-w` are the shell's, and a `C-c` that stopped here would mean never
    /// being able to interrupt a running program.
    fn terminal_key(&mut self, key: Key) -> Vec<EditorCommand> {
        let token = key.token();
        if let Some(action) = self.keymap.get(&(Mode::Terminal, token.clone())).cloned() {
            return self.run_action(&action);
        }
        if key.is_editor_key() {
            if let Some(action) = self.keymap.get(&(Mode::Normal, token)).cloned() {
                return self.run_action(&action);
            }
        }
        vec![EditorCommand::TermKey(key)]
    }

    /// `⌘⌫` — kill the word before point, as one undo step.
    ///
    /// Reuses `b`'s notion of a word, so what it removes is exactly what `b`
    /// would have jumped over. Nothing at the start of the buffer, and nothing
    /// where `b` would not move, so it can never delete a zero-width range and
    /// leave a checkpoint behind for it.
    fn delete_word_backward(&mut self) -> Vec<EditorCommand> {
        let end = self.buffer.cursor;
        let start = word_backward(&self.buffer, end);
        if start >= end {
            return vec![];
        }
        vec![
            EditorCommand::Checkpoint,
            EditorCommand::DeleteRange(start, end),
        ]
    }

    // --- Insert ----------------------------------------------------------

    fn insert_key(&mut self, key: Key) -> Vec<EditorCommand> {
        // Single-key bindings are live in Insert mode too — that is how `M-+`
        // or `C-s` keep working while you type. Multi-key sequences are Normal
        // mode only: waiting on a second key would swallow text you meant to
        // insert.
        if let Some(cmd) = self.keymap.get(&(Mode::Insert, key.token())) {
            let action = cmd.clone();
            return self.run_action(&action);
        }
        match key {
            Key::Esc | Key::Ctrl('c') => vec![EditorCommand::SetMode(Mode::Normal)],
            Key::Char(c) => vec![EditorCommand::InsertChar(c)],
            Key::Tab => vec![EditorCommand::InsertText(
                " ".repeat(self.settings.tab_width),
            )],
            Key::Enter => vec![EditorCommand::InsertNewline],
            Key::Backspace => vec![EditorCommand::DeleteBackward],
            Key::MetaBackspace => self.delete_word_backward(),
            Key::Left => vec![EditorCommand::MoveCursor(Direction::Left)],
            Key::Right => vec![EditorCommand::MoveCursor(Direction::Right)],
            Key::Up => vec![EditorCommand::MoveCursor(Direction::Up)],
            Key::Down => vec![EditorCommand::MoveCursor(Direction::Down)],
            Key::Ctrl(_) | Key::Meta(_) | Key::CtrlMeta(_) => vec![],
            Key::CtrlEnter => vec![EditorCommand::SplitWindow(frame::Split::Columns)],
            Key::CtrlMetaEnter => vec![EditorCommand::SplitWindow(frame::Split::Rows)],
        }
    }

    // --- Normal / Visual --------------------------------------------------

    fn normal_key(&mut self, key: Key) -> Vec<EditorCommand> {
        // 1. Literal-argument keys consume the next keystroke whole.
        if let Some(find) = self.pending.find.take() {
            let n = self.pending.count();
            let op = self.pending.op.take();
            self.pending.clear();
            let Key::Char(target) = key else {
                return vec![];
            };
            return match self.find_char(find, target, n) {
                Some(m) => self.resolve(op, m),
                None => vec![EditorCommand::Message(format!("not found: {target}"))],
            };
        }
        if self.pending.replace {
            self.pending.clear();
            let Key::Char(c) = key else { return vec![] };
            let at = self.buffer.cursor;
            let (line, _) = self.buffer.cursor_line_col();
            // Nothing under the cursor on a blank line or an empty buffer —
            // vim does nothing, and deleting here would eat the newline and
            // pull the following line up.
            if at >= self.buffer.line_end(line) {
                return vec![];
            }
            return vec![
                EditorCommand::Checkpoint,
                EditorCommand::DeleteRange(at, at + 1),
                EditorCommand::InsertChar(c),
                EditorCommand::MoveTo(at),
            ];
        }
        if let Some(kind) = self.pending.literal.take() {
            return self.literal_key(kind, key);
        }

        // 2. Esc always unwinds.
        if key == Key::Esc || key == Key::Ctrl('g') {
            let was_visual = matches!(self.mode, Mode::Visual | Mode::VisualLine);
            self.pending.clear();
            // A register named for a verb that never came must not attach
            // itself to whatever you do next.
            self.vim.pending = None;
            return if was_visual {
                vec![EditorCommand::SetMode(Mode::Normal)]
            } else {
                vec![]
            };
        }

        // 3. Counts, before any key lands in the sequence.
        if let Key::Char(c @ '0'..='9') = key {
            if c != '0' || self.pending.count.is_some() {
                let d = c.to_digit(10).unwrap() as usize;
                // Capped, not wrapping: holding a digit at key-repeat rate
                // overflows a usize in well under a second, and dev builds
                // have overflow checks on.
                let count = self.pending.count.unwrap_or(0).saturating_mul(10) + d;
                self.pending.count = Some(count.min(1_000_000));
                return vec![];
            }
        }

        // 4. User keymap wins over the built-in grammar.
        self.pending.keys.push(key.token());
        let seq = self.pending.keys.join(" ");
        // A binding made for this buffer's major or minor modes wins over the
        // same key bound globally — `(define-key "org-mode" "<tab>" ...)` only
        // applies in org buffers.
        if let Some(cmd) = self.mode_binding(&seq) {
            self.pending.clear();
            return self.run_action(&cmd);
        }
        // A mode-local *prefix* outranks a global exact binding, for exactly the
        // reason a mode-local binding does: `C-c` is bound everywhere to
        // `eval-dwim`, and a Lisp buffer wants the whole `C-c C-e` family under
        // it. Without this the global binding fires first and the sequence can
        // never be typed at all.
        let mode_prefix = self.mode_prefix(&seq);
        if !mode_prefix {
            if let Some(cmd) = self.keymap_lookup(&seq) {
                // Same namespace as a dashboard action: a built-in verb if it names
                // one, otherwise a Lisp call. Without this, binding a key to
                // `find-file` would reach the Lisp primitive of that name, which
                // wants a path argument and has no way to prompt for one.
                let action = cmd.clone();
                self.pending.clear();
                return self.run_action(&action);
            }
        }
        if mode_prefix || self.keymap_prefix(&seq) {
            // A longer binding may still match — and this is the one moment
            // Lisp cannot see for itself, so which-key is told about it. The
            // `fboundp` guard means a config that never loaded which-key gets
            // silence rather than an undefined-function report per prefix key.
            return vec![EditorCommand::CallLisp(format!(
                "(when (fboundp 'which-key) (which-key {seq:?}))"
            ))];
        }

        let cmds = self.builtin(&seq, key);
        // `builtin` returns None only to ask for more keys.
        match cmds {
            Some(cmds) => {
                self.pending.clear();
                self.without_insert(cmds)
            }
            None => vec![],
        }
    }

    /// Drop a request to enter Insert in a generated buffer.
    ///
    /// The grammar now runs in dired, magit and the dashboard, which is what
    /// makes their motions and operators work — but `i` there would park you in
    /// a mode where every keystroke is refused by `apply`, with the modeline
    /// claiming INSERT. The text is re-rendered from state and an edit could
    /// never survive anyway, so the honest answer is to say so and stay put.
    fn without_insert(&mut self, cmds: Vec<EditorCommand>) -> Vec<EditorCommand> {
        if !self.buffer.kind.is_generated()
            || !cmds
                .iter()
                .any(|c| matches!(c, EditorCommand::SetMode(Mode::Insert)))
        {
            return cmds;
        }
        let name = self.buffer.name();
        vec![EditorCommand::Message(format!("{name} is not a file"))]
    }

    /// The built-in grammar. `None` means "incomplete, wait for more keys".
    fn builtin(&mut self, seq: &str, key: Key) -> Option<Vec<EditorCommand>> {
        let n = self.pending.count();
        let op = self.pending.op;
        // Every visual mode, block included — an explicit list here silently
        // turned `d` in block mode into a pending operator instead.
        let visual = self.mode.is_visual();

        // A run of `j`/`k` remembers the column it started from; anything else
        // is a new horizontal position and forgets it.
        //
        // The *unit* follows the motion: a visual run holds a cell column,
        // because "the same place on the row below" is a grid position once one
        // buffer line can be several rows. One field rather than two, since any
        // key that is not `j`/`k` clears it — a run cannot straddle the two, and
        // the only way to change which unit is in force is to resize the window
        // mid-run, which costs one keystroke of drift.
        let col_now = match self.visual_lines() {
            true => self.cursor_vcol(),
            false => self.buffer.cursor_line_col().1,
        };
        if matches!(seq, "j" | "k" | "<down>" | "<up>") {
            self.desired_col.get_or_insert(col_now);
        } else {
            self.desired_col = None;
        }

        // `⌘⌫` kills a word backwards wherever you are, so it does the same
        // thing in Normal mode as in Insert rather than being dead in half the
        // editor. Before the motions, which have no claim on it.
        if seq == "M-<bs>" {
            self.pending.clear();
            return Some(self.delete_word_backward());
        }

        // Motions first: they compose with a pending operator.
        if let Some(m) = self.motion(seq, n) {
            return Some(self.resolve(op, m));
        }

        // Prefixes that need another key.
        if matches!(seq, "g" | "Z") {
            return None;
        }
        if matches!(seq, "f" | "F" | "t" | "T") {
            // Clear only the key sequence: `count` and `op` have to survive the
            // literal target key so `d2fx` still works.
            self.pending.find = seq.chars().next();
            self.pending.keys.clear();
            return None;
        }
        // The other keys whose argument is the next keystroke rather than a
        // motion: a register, a mark, a macro. Same rule as `f` and for the
        // same reason — ``d`a`` and `"a2dd` both have to survive the letter in
        // the middle, so only the key sequence is cleared.
        //
        // Before the operator-abort below, which is what would otherwise eat
        // the `` ` `` of ``d`a`` as "not a motion, give up".
        if matches!(seq, "\"" | "m" | "`" | "'" | "q" | "@") {
            self.pending.literal = seq.chars().next();
            self.pending.keys.clear();
            return None;
        }

        // Doubled operator = linewise over `count` lines: dd, yy, cc.
        if let Some(op) = op {
            if seq.len() == 1 && seq.starts_with(op.char()) {
                let (line, _) = self.buffer.cursor_line_col();
                let last = (line + n - 1).min(self.buffer.len_lines().saturating_sub(1));
                let m = Motion {
                    target: self.buffer.line_start(last),
                    span: Span::Linewise,
                };
                return Some(self.resolve(Some(op), m));
            }
            // Any other key aborts the operator.
            self.pending.op = None;
            return Some(vec![]);
        }

        let cursor = self.buffer.cursor;
        let (line, _) = self.buffer.cursor_line_col();
        let cmds = match seq {
            // --- operators ---
            "d" | "c" | "y" if !visual => {
                self.pending.op = Some(match seq {
                    "d" => Op::Delete,
                    "c" => Op::Change,
                    _ => Op::Yank,
                });
                self.pending.keys.clear(); // `count` survives: `2dw` == `d2w`
                return None;
            }
            // In visual mode the operator applies to the selection immediately.
            "d" | "x" if visual => self.op_selection(Op::Delete),
            "c" | "s" if visual => self.op_selection(Op::Change),
            "y" if visual => self.op_selection(Op::Yank),

            // --- entering insert ---
            // `SetMode` comes first in every one of these: in Normal mode the
            // cursor is clamped to the last character of the line, and `a`/`A`/
            // `o` all need to sit one past it.
            // The Checkpoint is what makes one insert session one undo step;
            // without it `u` skips the whole session and lands on whatever was
            // checkpointed before it.
            "i" => vec![
                EditorCommand::Checkpoint,
                EditorCommand::SetMode(Mode::Insert),
            ],
            "a" => vec![
                EditorCommand::Checkpoint,
                EditorCommand::SetMode(Mode::Insert),
                EditorCommand::MoveTo((cursor + 1).min(self.buffer.len_chars())),
            ],
            "I" => vec![
                EditorCommand::Checkpoint,
                EditorCommand::SetMode(Mode::Insert),
                EditorCommand::MoveTo(self.buffer.first_non_blank(line)),
            ],
            "A" => vec![
                EditorCommand::Checkpoint,
                EditorCommand::SetMode(Mode::Insert),
                EditorCommand::MoveTo(self.buffer.line_end(line)),
            ],
            "o" => vec![
                EditorCommand::Checkpoint,
                EditorCommand::SetMode(Mode::Insert),
                EditorCommand::MoveTo(self.buffer.line_end(line)),
                EditorCommand::InsertNewline,
            ],
            "O" => vec![
                EditorCommand::Checkpoint,
                EditorCommand::SetMode(Mode::Insert),
                EditorCommand::MoveTo(self.buffer.line_start(line)),
                EditorCommand::InsertNewline,
                EditorCommand::MoveTo(self.buffer.line_start(line)),
            ],

            // --- single-key edits ---
            // These all go through `operate` so they inherit its empty-range
            // guard: on a blank line `start == end`, and yanking that would
            // clobber the register with "" while deleting nothing.
            "x" => {
                let end = (cursor + n).min(self.buffer.line_end(line));
                self.operate(Op::Delete, cursor, end, false)
            }
            "D" => {
                let end = self.buffer.line_end(line);
                self.operate(Op::Delete, cursor, end, false)
            }
            "C" => {
                let end = self.buffer.line_end(line);
                self.operate(Op::Change, cursor, end, false)
            }
            "r" => {
                self.pending.replace = true;
                self.pending.keys.clear();
                return None;
            }
            "J" => self.join_lines(n),
            "p" => self.paste_cmds(true),
            "P" => self.paste_cmds(false),
            "u" => vec![EditorCommand::Undo],
            "C-r" => vec![EditorCommand::Redo],

            // --- modes ---
            "v" => vec![EditorCommand::SetMode(if self.mode == Mode::Visual {
                Mode::Normal
            } else {
                Mode::Visual
            })],
            "V" => vec![EditorCommand::SetMode(if self.mode == Mode::VisualLine {
                Mode::Normal
            } else {
                Mode::VisualLine
            })],
            "C-v" => vec![EditorCommand::SetMode(if self.mode == Mode::VisualBlock {
                Mode::Normal
            } else {
                Mode::VisualBlock
            })],

            // --- scrolling ---
            "C-d" => return Some(self.scroll_half(true)),
            "C-u" => return Some(self.scroll_half(false)),

            // --- prompts and meta ---
            ":" => {
                self.open_prompt(PromptKind::Ex);
                // vim types the range for you when there is a selection, and
                // that is also how anyone discovers `:'<,'>s` exists.
                if self.mode.is_visual() {
                    if let Some(p) = self.prompt.as_mut() {
                        p.text = "'<,'>".into();
                    }
                }
                vec![]
            }
            "/" => {
                self.open_prompt(PromptKind::Search);
                vec![]
            }
            "n" => self.search_from(self.buffer.cursor + 1, true),
            "N" => self.search_from(self.buffer.cursor, false),
            // Window splits, reachable in Normal and Visual as well as Insert.
            "C-<ret>" => vec![EditorCommand::SplitWindow(frame::Split::Columns)],
            "C-M-<ret>" => vec![EditorCommand::SplitWindow(frame::Split::Rows)],
            "C-w" => vec![EditorCommand::FocusNextWindow],
            "Z Z" => vec![EditorCommand::Quit],
            "Z Q" => vec![EditorCommand::Quit],
            "g h" => vec![EditorCommand::ShowDashboard],

            _ => {
                let _ = key;
                vec![]
            }
        };
        Some(cmds)
    }

    // --- motions ---------------------------------------------------------

    /// A binding from the buffer's minor modes, then its major mode.
    ///
    /// Minor modes are checked first, and most recently enabled first, which is
    /// the Emacs precedence: a minor mode is something you switched on *for*
    /// this buffer, so it should be able to override the major mode's idea of a
    /// key.
    fn mode_binding(&self, seq: &str) -> Option<String> {
        self.buffer
            .minor_modes
            .iter()
            .rev()
            .chain(std::iter::once(&self.buffer.major_mode))
            .find_map(|m| self.mode_keymap.get(&(m.clone(), seq.to_string())).cloned())
    }

    /// The keymaps a lookup tries, nearest first.
    ///
    /// One entry for an editing mode. Two for dired, magit and the dashboard,
    /// which *layer over* Normal: their own binding wins, and anything they do
    /// not claim falls through, so `M-x` and the leader key work in a listing
    /// exactly as they do in a file.
    pub(crate) fn keymaps(&self) -> impl Iterator<Item = Mode> {
        std::iter::once(self.mode).chain(self.mode.layers_over_normal().then_some(Mode::Normal))
    }

    fn keymap_lookup(&self, seq: &str) -> Option<String> {
        self.keymaps()
            .find_map(|m| self.keymap.get(&(m, seq.to_string())).cloned())
    }

    /// True when some binding in reach is still waiting on more keys — so a
    /// leader sequence typed in dired waits for the rest of itself rather than
    /// giving up after `SPC`.
    fn keymap_prefix(&self, seq: &str) -> bool {
        let with_space = format!("{seq} ");
        self.keymaps().any(|mode| {
            self.keymap
                .keys()
                .any(|(m, k)| *m == mode && k.starts_with(&with_space))
        })
    }

    /// True when some mode binding is still waiting on more keys.
    fn mode_prefix(&self, seq: &str) -> bool {
        let with_space = format!("{seq} ");
        self.mode_keymap.keys().any(|(m, k)| {
            k.starts_with(&with_space)
                && (*m == self.buffer.major_mode || self.buffer.minor_modes.contains(m))
        })
    }

    /// The column a vertical run is aiming for: the one it started from, not
    /// wherever a short line clamped it to along the way.
    fn held_col(&self, col: usize) -> usize {
        self.desired_col.unwrap_or(col)
    }

    /// One buffer line down (`down`) or up from `line`, stepping *over* every
    /// line a fold is hiding.
    ///
    /// This is the whole of folding as far as the command loop is concerned. The
    /// renderer does not draw a hidden line, so `j` must not land on one either
    /// — a cursor on a row that is not drawn is a cursor nobody can see, and the
    /// next keystroke would edit text nobody can see. One predicate,
    /// [`zemacs_core::fold_hiding`](crate::fold_hiding), answers for both sides.
    ///
    /// Answers `line` itself when there is no visible line that way: the end of
    /// the document, and equally a fold that runs to it. `j` on the last line
    /// already stays put, so a fold reaching the end behaves the same.
    ///
    /// Free on a buffer with no overlays, which is nearly all of them — the
    /// predicate is a scan of an empty slice.
    pub(crate) fn step_line(&self, line: usize, down: bool) -> usize {
        let buf = &self.buffer;
        let mut l = line;
        loop {
            l = match (down, l) {
                (true, l) if l + 1 < buf.len_lines() => l + 1,
                (false, l) if l > 0 => l - 1,
                _ => return line,
            };
            if crate::fold_hiding(buf.overlays(), buf.line_start(l)).is_none() {
                return l;
            }
        }
    }

    /// `j` and `k`, by **visual** line when the window wraps — the config's
    /// `evil-next-visual-line` — and by buffer line otherwise.
    ///
    /// Which one is in force is decided by the window, not by the key: with
    /// truncation on, or before anything has drawn, every buffer line is exactly
    /// one row and the two answers are identical.
    ///
    /// ponytail: an operator still gets the buffer-line target, so `dj` deletes
    /// two whole lines even on a wrapped one — which is what vim does and what
    /// every test here asserts. Emacs' `evil-next-visual-line` is an *exclusive*
    /// motion and would instead delete to the same column one row down; adopting
    /// that means changing this `Span` as well as this target, and it changes
    /// what `dj` means, which is a much louder change than the cursor moving.
    fn vertical(&self, down: bool, n: usize) -> Motion {
        let buf = &self.buffer;
        let (line, col) = buf.cursor_line_col();
        let target = match self.pending.op.is_none() && self.visual_lines() {
            true => self.visual_target(down, n, self.held_col(self.cursor_vcol())),
            false => {
                let mut l = line;
                for _ in 0..n {
                    l = self.step_line(l, down);
                }
                buf.line_start(l) + self.held_col(col).min(buf.line_len(l))
            }
        };
        Motion {
            target,
            span: Span::Linewise,
        }
    }

    fn motion(&self, seq: &str, n: usize) -> Option<Motion> {
        let buf = &self.buffer;
        let (line, col) = buf.cursor_line_col();
        let cur = buf.cursor;
        let m = match seq {
            "h" | "<left>" => Motion {
                target: buf.line_start(line) + col.saturating_sub(n),
                span: Span::Exclusive,
            },
            "l" | "<right>" | "SPC" => Motion {
                target: (cur + n).min(buf.line_end(line)),
                span: Span::Exclusive,
            },
            // Vertical motions hold the column. Targeting the line start would
            // send `j` to column 0, which is wrong for the cursor and invisible
            // to an operator (a linewise span only reads the *line*).
            "j" | "<down>" => self.vertical(true, n),
            "k" | "<up>" => self.vertical(false, n),
            "0" => Motion {
                target: buf.line_start(line),
                span: Span::Exclusive,
            },
            "^" => Motion {
                target: buf.first_non_blank(line),
                span: Span::Exclusive,
            },
            "$" => Motion {
                target: buf.line_end((line + n - 1).min(buf.len_lines().saturating_sub(1))),
                span: Span::Inclusive,
            },
            "w" => Motion {
                target: (0..n).fold(cur, |p, _| word_forward(buf, p)),
                span: Span::Exclusive,
            },
            "b" => Motion {
                target: (0..n).fold(cur, |p, _| word_backward(buf, p)),
                span: Span::Exclusive,
            },
            "e" => Motion {
                target: (0..n).fold(cur, |p, _| word_end(buf, p)),
                span: Span::Inclusive,
            },
            "{" => Motion {
                target: buf.line_start(paragraph(buf, line, false)),
                span: Span::Linewise,
            },
            "}" => Motion {
                target: buf.line_start(paragraph(buf, line, true)),
                span: Span::Linewise,
            },
            "G" => {
                let last = buf.len_lines().saturating_sub(1);
                let target = match self.pending.count {
                    Some(c) => buf.first_non_blank((c - 1).min(last)),
                    None => buf.first_non_blank(last),
                };
                Motion {
                    target,
                    span: Span::Linewise,
                }
            }
            "g g" => {
                let target = buf.first_non_blank(self.pending.count.unwrap_or(1).saturating_sub(1));
                Motion {
                    target,
                    span: Span::Linewise,
                }
            }
            "g e" => Motion {
                target: buf.len_chars().saturating_sub(1),
                span: Span::Inclusive,
            },
            _ => return None,
        };
        Some(m)
    }

    fn find_char(&self, kind: char, target: char, n: usize) -> Option<Motion> {
        let buf = &self.buffer;
        let (line, _) = buf.cursor_line_col();
        let (lo, hi) = (buf.line_start(line), buf.line_end(line));
        let forward = kind == 'f' || kind == 't';
        let mut pos = buf.cursor;
        for _ in 0..n {
            pos = if forward {
                (pos + 1..hi).find(|&i| buf.char_at(i) == Some(target))?
            } else {
                (lo..pos).rev().find(|&i| buf.char_at(i) == Some(target))?
            };
        }
        let target_pos = match kind {
            't' => pos.saturating_sub(1),
            'T' => pos + 1,
            _ => pos,
        };
        Some(Motion {
            target: target_pos,
            span: if forward {
                Span::Inclusive
            } else {
                Span::Exclusive
            },
        })
    }

    /// Either move the cursor (no operator) or apply the operator to the span.
    fn resolve(&mut self, op: Option<Op>, m: Motion) -> Vec<EditorCommand> {
        let Some(op) = op else {
            return vec![EditorCommand::MoveTo(m.target)];
        };
        let cur = self.buffer.cursor;
        let (start, end) = match m.span {
            Span::Exclusive => (cur.min(m.target), cur.max(m.target)),
            Span::Inclusive => (
                cur.min(m.target),
                (cur.max(m.target) + 1).min(self.buffer.len_chars()),
            ),
            Span::Linewise => {
                let a = self.buffer.text.char_to_line(cur.min(self.buffer.len_chars()));
                let b = self
                    .buffer
                    .text
                    .char_to_line(m.target.min(self.buffer.len_chars()));
                let (first, last) = (a.min(b), a.max(b));
                (
                    self.buffer.line_start(first),
                    (self.buffer.line_end(last) + 1).min(self.buffer.len_chars()),
                )
            }
        };
        let linewise = m.span == Span::Linewise;
        self.operate(op, start, end, linewise)
    }

    fn op_selection(&mut self, op: Op) -> Vec<EditorCommand> {
        if self.mode == Mode::VisualBlock {
            return self.op_block(op);
        }
        let Some((start, end)) = self.selection() else {
            return vec![];
        };
        let linewise = self.mode == Mode::VisualLine;
        let mut cmds = vec![EditorCommand::SetMode(Mode::Normal)];
        cmds.extend(self.operate(op, start, end, linewise));
        cmds
    }

    /// A block operator is several disjoint edits, so the ranges are deleted
    /// **bottom-up**: every command is computed against the pre-edit buffer,
    /// and removing a later range cannot move an earlier one.
    fn op_block(&mut self, op: Op) -> Vec<EditorCommand> {
        let ranges = self.selection_ranges();
        if ranges.is_empty() {
            return vec![EditorCommand::SetMode(Mode::Normal)];
        }
        let text = ranges
            .iter()
            .map(|&(s, e)| self.buffer.slice_string(s, e))
            .collect::<Vec<_>>()
            .join("\n");
        let top = ranges[0].0;
        if let Some(name) = self.vim.pending.take() {
            self.vim.write(name, text.clone(), false);
        }

        let mut cmds = vec![EditorCommand::SetMode(Mode::Normal)];
        // The register holds the block's text; `linewise` is false because
        // pasting a block back as whole lines is not what was copied.
        cmds.push(EditorCommand::SetRegister {
            text,
            linewise: false,
        });
        if op != Op::Yank {
            cmds.push(EditorCommand::Checkpoint);
            for &(s, e) in ranges.iter().rev() {
                cmds.push(EditorCommand::DeleteRange(s, e));
            }
        }
        cmds.push(EditorCommand::MoveTo(top));
        if op == Op::Change {
            cmds.push(EditorCommand::SetMode(Mode::Insert));
        }
        cmds
    }

    fn operate(&mut self, op: Op, start: usize, end: usize, linewise: bool) -> Vec<EditorCommand> {
        if start >= end {
            return vec![];
        }
        // `"ayy`. Done here rather than through a command because the unnamed
        // register is written by the `Yank` below *as well* — vim fills `""` on
        // every yank and delete whatever register you named, so this is an
        // extra copy and not a redirection.
        if let Some(name) = self.vim.pending.take() {
            let text = self.buffer.slice_string(start, end);
            self.vim.write(name, text, linewise);
        }
        let yank = EditorCommand::Yank {
            start,
            end,
            linewise,
        };
        match op {
            Op::Yank => vec![yank, EditorCommand::MoveTo(start)],
            Op::Delete => {
                // A linewise delete that reaches the end of the buffer has no
                // trailing newline to remove, so take the *preceding* one
                // instead — otherwise `dd` on the last line leaves a blank one.
                let from = if linewise && end == self.buffer.len_chars() && start > 0 {
                    start - 1
                } else {
                    start
                };
                vec![
                    EditorCommand::Checkpoint,
                    yank,
                    EditorCommand::DeleteRange(from, end),
                    EditorCommand::MoveTo(from),
                ]
            }
            Op::Change => {
                // `cc`/`cj` keep the line, clearing its contents.
                let (start, end) = if linewise {
                    (start, end.saturating_sub(1))
                } else {
                    (start, end)
                };
                vec![
                    EditorCommand::Checkpoint,
                    EditorCommand::SetMode(Mode::Insert),
                    EditorCommand::Yank {
                        start,
                        end,
                        linewise,
                    },
                    EditorCommand::DeleteRange(start, end),
                    EditorCommand::MoveTo(start),
                ]
            }
        }
    }

    /// `J` joins two lines, `{n}J` joins `n`.
    ///
    /// Built as one replacement rather than a loop of deletes: every command in
    /// the returned Vec is computed against the *pre-edit* buffer, so an
    /// iterative version would recompute the same offsets each time round and
    /// join only once however large the count.
    fn join_lines(&self, n: usize) -> Vec<EditorCommand> {
        let (line, _) = self.buffer.cursor_line_col();
        let last = (line + n.max(2) - 1).min(self.buffer.len_lines().saturating_sub(1));
        if last <= line {
            return vec![];
        }
        let start = self.buffer.line_start(line);
        let end = self.buffer.line_end(last);
        let first = self.buffer.slice_string(start, self.buffer.line_end(line));
        let joined = (line + 1..=last)
            .map(|l| {
                self.buffer
                    .slice_string(self.buffer.first_non_blank(l), self.buffer.line_end(l))
            })
            .fold(first.clone(), |acc, rest| format!("{acc} {rest}"));
        vec![
            EditorCommand::Checkpoint,
            EditorCommand::DeleteRange(start, end),
            EditorCommand::InsertText(joined),
            // vim leaves the cursor on the first join seam.
            EditorCommand::MoveTo(start + first.chars().count()),
        ]
    }

    fn scroll_half(&mut self, down: bool) -> Vec<EditorCommand> {
        let half = (self.viewport_lines / 2).max(1);
        let (line, col) = self.buffer.cursor_line_col();
        let target = if down {
            (line + half).min(self.buffer.len_lines().saturating_sub(1))
        } else {
            line.saturating_sub(half)
        };
        self.scroll = if down {
            self.scroll + half
        } else {
            self.scroll.saturating_sub(half)
        };
        vec![EditorCommand::MoveTo(
            self.buffer.line_start(target) + col.min(self.buffer.line_len(target)),
        )]
    }

    // --- registers, macros, marks ----------------------------------------

    /// The second half of `"x`, `mx`, `` `x ``, `'x`, `qx` and `@x`: the key is
    /// data, not a command.
    fn literal_key(&mut self, kind: char, key: Key) -> Vec<EditorCommand> {
        let n = self.pending.count();
        let op = self.pending.op;
        let Key::Char(c) = key else {
            self.pending.clear();
            return vec![];
        };
        match kind {
            // A register names the *next* verb, so this is the one literal that
            // leaves `count` and `op` alone: `"a2dd` and `2"add` are the same
            // two lines into the same register.
            '"' => {
                self.vim.pending = Some(c);
                self.pending.keys.clear();
                vec![]
            }
            'm' => {
                self.pending.clear();
                self.set_mark(c)
            }
            // A mark is a motion, which is why it goes through `resolve`:
            // ``d`a`` deletes to the exact position and `d'a` deletes lines.
            '`' | '\'' => {
                self.pending.clear();
                match self.mark_motion(kind == '\'', c) {
                    Some(m) => self.resolve(op, m),
                    None => vec![EditorCommand::Message(format!("mark not set: {c}"))],
                }
            }
            'q' => {
                self.pending.clear();
                self.vim.recording = Some((c, Vec::new()));
                vec![EditorCommand::Message(format!("recording @{c}"))]
            }
            '@' => {
                self.pending.clear();
                self.replay(c, n)
            }
            _ => {
                self.pending.clear();
                vec![]
            }
        }
    }

    /// True where a `q` would end a recording rather than mean a letter.
    fn recording_here(&self) -> bool {
        self.vim.recording.is_some()
            && self.prompt.is_none()
            && self.ace.is_none()
            && self.pending.literal.is_none()
            && self.pending.find.is_none()
            && !self.pending.replace
            && (self.mode == Mode::Normal || self.mode.is_visual())
    }

    fn stop_recording(&mut self) -> Vec<EditorCommand> {
        let Some((name, keys)) = self.vim.recording.take() else {
            return vec![];
        };
        let n = keys.len();
        self.vim.macros.insert(name, keys);
        vec![EditorCommand::Message(format!("recorded {n} keys into @{name}"))]
    }

    /// `@a`, and `@@` for whatever ran last.
    ///
    /// The keys go back through `handle_key`, and the commands are applied
    /// **here** rather than handed to the caller: every motion is computed
    /// against the document as it is *now*, so the second `dw` of a replay has
    /// to see what the first one did. Returning them all in one batch would
    /// compute every key against the text as it stood before the macro started,
    /// and a two-line macro would delete the same word twice.
    ///
    /// This is the one place `handle_key` mutates the document, and it is not a
    /// crack in the rule — it is still `Editor::apply` doing the writing, just
    /// with the replay driving the loop instead of the app. Commands core
    /// cannot carry out itself travel back up as usual, so a macro can still
    /// open a file or call Lisp.
    fn replay(&mut self, name: char, count: usize) -> Vec<EditorCommand> {
        let name = match name {
            '@' => match self.vim.last_macro {
                Some(n) => n,
                None => return vec![EditorCommand::Message("no macro to repeat".into())],
            },
            n => n,
        };
        let Some(keys) = self.vim.macros.get(&name).cloned() else {
            return vec![EditorCommand::Message(format!("empty macro: @{name}"))];
        };
        if self.vim.depth >= MACRO_DEPTH {
            return vec![EditorCommand::Message(format!("@{name} nested too deeply"))];
        }
        self.vim.last_macro = Some(name);
        self.vim.depth += 1;
        let mut out = Vec::new();
        for _ in 0..count {
            for key in keys.iter().copied() {
                for cmd in self.handle_key(key) {
                    if cmd.needs_app() {
                        out.push(cmd);
                    } else {
                        self.apply(cmd);
                    }
                }
            }
        }
        self.vim.depth -= 1;
        out
    }

    /// `ma`. A *marker*, so the mark still names its character after an edit
    /// above it — which is the whole difference between this and remembering an
    /// offset.
    fn set_mark(&mut self, name: char) -> Vec<EditorCommand> {
        let slot = (self.buffer.id, name);
        // Replacing a mark frees the marker it used, or `ma` in a loop leaves
        // one dead marker per press in the buffer for `splice` to walk.
        if let Some(old) = self.vim.marks.remove(&slot) {
            self.delete_marker(old);
        }
        let id = self.make_marker(self.buffer.cursor, Insertion::Stay);
        self.vim.marks.insert(slot, id);
        vec![EditorCommand::Message(format!("mark {name} set"))]
    }

    /// `` `a `` is the exact position, `'a` is the line — vim's distinction,
    /// and the reason they are two keys rather than one.
    ///
    /// ponytail: only the letters. Vim's `` `` `` (where you were), `'<`/`'>`
    /// (the last selection) and the uppercase file-marks need a jump list, a
    /// selection history and a cross-buffer marker table respectively — three
    /// features, none of them this one.
    fn mark_motion(&self, linewise: bool, name: char) -> Option<Motion> {
        let at = self
            .vim
            .marks
            .get(&(self.buffer.id, name))
            .copied()
            .and_then(|id| self.marker_position(id))?;
        Some(match linewise {
            true => {
                let line = self.buffer.text.char_to_line(at.min(self.buffer.len_chars()));
                Motion {
                    target: self.buffer.first_non_blank(line),
                    span: Span::Linewise,
                }
            }
            false => Motion {
                target: at,
                span: Span::Exclusive,
            },
        })
    }

    /// `p`, and `"ap` out of a named register.
    ///
    /// A named paste loads the register into the unnamed one, pastes, and puts
    /// back what was there: vim leaves `""` untouched by `"ap`, and
    /// `EditorCommand::Paste` has no register to paste from otherwise.
    /// ponytail: the day `Paste` grows a register field — it lives in the one
    /// file this file cannot freely edit — these four commands become one.
    fn paste_cmds(&mut self, after: bool) -> Vec<EditorCommand> {
        let Some(name) = self.vim.pending.take() else {
            return vec![EditorCommand::Checkpoint, EditorCommand::Paste { after }];
        };
        let Some((text, linewise)) = self.vim.read(name).cloned() else {
            return vec![EditorCommand::Message(format!("register {name} is empty"))];
        };
        vec![
            EditorCommand::Checkpoint,
            EditorCommand::SetRegister { text, linewise },
            EditorCommand::Paste { after },
            EditorCommand::SetRegister {
                text: self.register.clone(),
                linewise: self.register_linewise,
            },
        ]
    }

    // --- search ----------------------------------------------------------

    /// Where the pattern next matches, wrapping at the end of the buffer.
    /// Char offsets in and out; the engine works in bytes, so the conversion
    /// happens here and nowhere else.
    ///
    /// ponytail: the pattern is compiled and the rope flattened on every call,
    /// and incremental search calls it once per keystroke rather than once per
    /// `n`. Both are a whole-buffer allocation for what is usually a match a
    /// few characters away. Cache the compiled pattern and feed the engine the
    /// rope's chunks when a large file starts to feel it.
    fn search_pos(&self, pat: &str, from: usize, forward: bool) -> Option<usize> {
        let re = compile(pat, false)?;
        let hay = self.buffer.text.to_string();
        let start = self
            .buffer
            .text
            .char_to_byte(from.min(self.buffer.len_chars()));
        let hit = if forward {
            (re.find_at(&hay, start).map(|m| m.start())).or_else(|| re.find(&hay).map(|m| m.start()))
        } else {
            // The last match that begins before us — `find_iter` is
            // left-to-right and non-overlapping, so `take_while` is exactly
            // "everything behind the cursor".
            let backwards = || re.find_iter(&hay).map(|m| m.start());
            (backwards().take_while(|&s| s < start).last()).or_else(|| backwards().last())
        };
        hit.map(|b| self.buffer.text.byte_to_char(b))
    }

    fn search_from(&mut self, from: usize, forward: bool) -> Vec<EditorCommand> {
        if self.last_search.is_empty() {
            return vec![EditorCommand::Message("no previous search".into())];
        }
        let pat = self.last_search.clone();
        if compile(&pat, false).is_none() {
            return vec![EditorCommand::Message(format!("bad pattern: {pat}"))];
        }
        match self.search_pos(&pat, from, forward) {
            Some(at) => vec![EditorCommand::MoveTo(at)],
            None => vec![EditorCommand::Message(format!("pattern not found: {pat}"))],
        }
    }

    // --- prompt line -----------------------------------------------------

    fn prompt_key(&mut self, key: Key) -> Vec<EditorCommand> {
        let Some(p) = self.prompt.as_mut() else {
            return vec![];
        };
        match key {
            Key::Esc | Key::Ctrl('g') => return self.cancel_prompt(),
            Key::Backspace => {
                if p.text.pop().is_none() {
                    return self.cancel_prompt();
                }
                p.refilter();
            }
            Key::Char(c) => {
                p.text.push(c);
                p.refilter();
            }
            // `C-j`/`C-k` alongside `C-n`/`C-p`: both spellings are muscle
            // memory depending on which completion UI you came from.
            Key::Ctrl('n') | Key::Ctrl('j') | Key::Down => p.next(),
            Key::Ctrl('p') | Key::Ctrl('k') | Key::Up => p.prev(),
            Key::Tab => p.complete(),
            Key::Enter => return self.accept_prompt(),
            _ => return vec![],
        }
        self.preview()
    }

    /// Enter: what the prompt was asking for decides what happens.
    fn accept_prompt(&mut self) -> Vec<EditorCommand> {
        let Some(p) = self.prompt.take() else {
            return vec![];
        };
        match p.kind {
            PromptKind::Ex => self.ex_command(&p.text),
            PromptKind::Search => {
                // An empty pattern reuses the last one, as vim does: a stray
                // `/` RET must not throw away what `n` was following.
                let origin = p.origin.unwrap_or(self.buffer.cursor);
                if !p.text.is_empty() {
                    self.last_search = p.text;
                }
                // From the origin, not from the cursor — the incremental
                // preview has already moved the cursor onto the match, and
                // searching from *there* would land on the one after it.
                self.search_from(origin + 1, true)
            }
            PromptKind::Command => {
                let name = p.value();
                if name.is_empty() {
                    vec![]
                } else {
                    // Through `run_action`, the same path a keybinding takes:
                    // a built-in verb if it names one, otherwise a Lisp call.
                    // Sending `(name)` straight to the image would mean `M-x
                    // new-frame` looked up a Lisp function that does not exist.
                    self.run_action(&name)
                }
            }
            PromptKind::File => {
                let path = p.value();
                if path.is_empty() {
                    vec![]
                } else {
                    vec![EditorCommand::OpenFile(PathBuf::from(expand_tilde(&path)))]
                }
            }
            PromptKind::Buffer => match p.matches.get(p.selected) {
                Some(&i) => vec![EditorCommand::SwitchBuffer(i)],
                None => vec![],
            },
            // Items are one per line, in order, so the item index *is* the
            // line number — no parsing back out of the rendered text.
            PromptKind::Line => match p.matches.get(p.selected) {
                Some(&line) => vec![EditorCommand::MoveTo(self.buffer.first_non_blank(line))],
                None => vec![],
            },
            // A project file, or a project root — which opens as a directory,
            // and a directory is dired. One prompt, both gestures.
            PromptKind::ProjectFile => match p.current() {
                Some(path) => vec![EditorCommand::OpenFile(path.into())],
                None => vec![],
            },
            // `path:line:text`, ripgrep's own format. Opening the file is the
            // app's job, and so is the jump — core cannot know how long the
            // file will be until it has been read.
            PromptKind::Grep => match p.current() {
                Some(hit) => vec![EditorCommand::OpenAt(hit.to_string())],
                None => vec![],
            },
            // lisp-api: the one kind whose destination is not fixed here. The
            // answer goes back to the image as a call, which is the *only* way
            // core can reach Lisp — and it is a continuation rather than a
            // return value because the Lisp thread must never sit waiting on the
            // user. See `docs/threading.org`.
            //
            // `value()`, so a highlighted candidate wins over the raw text and a
            // `completing-read` with nothing matching still answers what was
            // typed — Emacs' `require-match nil`, which is the useful default.
            PromptKind::Lisp { id, .. } => vec![EditorCommand::CallLisp(format!(
                "(%prompt-reply {id} {})",
                crate::query::lisp_string(&p.value())
            ))],
        }
    }

    /// lisp-api: drop the prompt without an answer — Escape, `C-g`, or
    /// backspacing past the start of an empty one.
    ///
    /// A previewing prompt has been dragging the cursor around, so it goes back
    /// where it started rather than being left wherever the last candidate was.
    /// A *Lisp* prompt has to be told as well: its continuation is parked in a
    /// table in the image, and a cancel that said nothing would leave the
    /// closure there forever and the caller waiting for a call that never comes.
    /// `NIL` is the answer, which is what `(when answer ...)` in a handler reads
    /// as "the user backed out".
    fn cancel_prompt(&mut self) -> Vec<EditorCommand> {
        let Some(p) = self.prompt.take() else {
            return vec![];
        };
        let mut out: Vec<EditorCommand> =
            p.origin.into_iter().map(EditorCommand::MoveTo).collect();
        if let PromptKind::Lisp { id, .. } = p.kind {
            out.push(EditorCommand::CallLisp(format!("(%prompt-reply {id} nil)")));
        }
        out
    }

    /// Move the cursor to the highlighted line while the prompt is still open —
    /// consult's preview, so narrowing shows you where you would land.
    fn preview(&mut self) -> Vec<EditorCommand> {
        let Some(p) = self.prompt.as_ref() else {
            return vec![];
        };
        if !p.kind.previews() {
            return vec![];
        }
        // Incremental search: there is no candidate list to highlight, so what
        // the cursor follows is the match itself, recomputed on every
        // keystroke. Falling back to the origin rather than staying put is
        // deliberate — a pattern that has stopped matching should look like it
        // has, and typing one more character then un-typing it must be a
        // round trip.
        if p.kind == PromptKind::Search {
            let origin = p.origin.unwrap_or(self.buffer.cursor);
            let pat = p.text.clone();
            let at = (!pat.is_empty())
                .then(|| self.search_pos(&pat, origin + 1, true))
                .flatten();
            return vec![EditorCommand::MoveTo(at.unwrap_or(origin))];
        }
        match p.matches.get(p.selected) {
            Some(&line) => vec![EditorCommand::MoveTo(self.buffer.first_non_blank(line))],
            None => vec![],
        }
    }

    /// Send source to the Lisp image, refusing to send nothing.
    fn eval_text(&self, src: String) -> Vec<EditorCommand> {
        if src.trim().is_empty() {
            vec![EditorCommand::Message("nothing to evaluate".into())]
        } else {
            vec![EditorCommand::CallLisp(src)]
        }
    }

    /// Open one of the completing prompts.
    pub fn open_prompt(&mut self, kind: PromptKind) {
        let (label, items) = match kind {
            PromptKind::Command => {
                // Built-in verbs are commands too — `M-x new-frame` should work
                // without anyone having defined it in Lisp. They come first
                // because core resolves them first.
                let mut items: Vec<String> =
                    BUILTIN_COMMANDS.iter().map(|s| s.to_string()).collect();
                let extra: Vec<String> = self
                    .commands
                    .iter()
                    .filter(|c| !items.contains(c))
                    .cloned()
                    .collect();
                items.extend(extra);
                ("M-x ", items)
            }
            PromptKind::Buffer => ("Buffer: ", self.buffer_names()),
            // The app fills these in as the path is typed; core does no IO.
            PromptKind::File => ("Find file: ", Vec::new()),
            // Likewise, but from ripgrep rather than from a directory read.
            PromptKind::Grep => ("Search project: ", Vec::new()),
            PromptKind::ProjectFile => ("Project file: ", Vec::new()),
            PromptKind::Ex => (":", Vec::new()),
            PromptKind::Search => ("/", Vec::new()),
            // One item per line, in order, so `matches[selected]` is the line
            // number itself. The number is in the text purely to read.
            PromptKind::Line => {
                let n = self.buffer.len_lines();
                let width = n.to_string().len();
                let items = (0..n)
                    .map(|l| {
                        let text = self
                            .buffer
                            .slice_string(self.buffer.line_start(l), self.buffer.line_end(l));
                        format!("{:>width$}  {}", l + 1, text.trim_end())
                    })
                    .collect();
                ("Line: ", items)
            }
            // lisp-api: not opened here. Its label comes from Lisp and its
            // candidates arrive afterwards, so it is a command
            // (`ReadFromMinibuffer`) rather than a kind this function can build.
            // Reaching this arm means `(open-prompt "...")` was handed a kind
            // only `read-string` can make, which the shim refuses by name.
            PromptKind::Lisp { .. } => ("", Vec::new()),
        };
        let mut prompt = Prompt::new(kind, label, items);
        prompt.origin = kind.previews().then_some(self.buffer.cursor);
        self.prompt = Some(prompt);
    }

    /// `:[range]s/pat/rep/[flags]` — `None` when the line is not a substitute.
    ///
    /// The whole substitution is one `Checkpoint`, one `DeleteRange` and one
    /// `InsertText` over the affected lines: the shape `rs_replace_region`
    /// uses, and the reason `u` reverses a `:%s` in one press instead of once
    /// per match. Building the new text first and splicing it in once is also
    /// the only version that is *correct* — every command in a returned batch
    /// is computed against the pre-edit buffer, so a loop of per-match deletes
    /// would aim every one of them at stale offsets.
    fn substitute(&mut self, line: &str) -> Option<Vec<EditorCommand>> {
        // The range. Nothing is the current line, `%` the whole buffer, and
        // `'<,'>` — which `:` in visual mode types for you — the selection.
        // ponytail: no `1,5`, no `.`/`$`, no marks and no offsets. Each is a
        // small parser and none of them is reachable from a keystroke today.
        let whole = (0, self.buffer.len_lines().saturating_sub(1));
        // `None` here is only ever "`'<,'>` with nothing selected", which falls
        // back to the current line rather than silently doing the whole buffer.
        let (rest, lines) = match line {
            l if l.starts_with('%') => (&l[1..], Some(whole)),
            l if l.starts_with("'<,'>") => (&l[5..], self.selection_lines()),
            l => (l, Some(self.line_range(1))),
        };
        let mut head = rest.chars();
        if head.next() != Some('s') {
            return None;
        }
        // Vim takes any non-alphanumeric as the delimiter, which is what makes
        // `:s#a/b#c#` writable. Requiring one is also what keeps `:set` and
        // `:split` out of here.
        let delim = head
            .next()
            .filter(|c| !c.is_alphanumeric() && !c.is_whitespace())?;
        let parts = split_delim(&rest[1 + delim.len_utf8()..], delim);

        let mut global = false;
        let mut fold = false;
        for f in parts.get(2).map_or("", |s| s.as_str()).chars() {
            match f {
                'g' => global = true,
                'i' => fold = true,
                // ponytail: `c` (confirm) and `n` (count only) both want a
                // prompt whose answer comes *back*, which core has no shape
                // for. Refused rather than ignored — a silently dropped `c`
                // is a whole-buffer replace nobody agreed to.
                other => {
                    return Some(vec![EditorCommand::Message(format!(
                        "unsupported :s flag: {other}"
                    ))])
                }
            }
        }

        // An empty pattern means the last search, as in vim — `/foo` then
        // `:s//bar/` is the idiom, and it is free.
        let pat = match parts.first().map_or("", |s| s.as_str()) {
            "" => self.last_search.clone(),
            p => p.to_string(),
        };
        if pat.is_empty() {
            return Some(vec![EditorCommand::Message("no previous search".into())]);
        }
        let Some(re) = compile(&pat, fold) else {
            return Some(vec![EditorCommand::Message(format!("bad pattern: {pat}"))]);
        };
        self.last_search = pat.clone();
        let rep = vim_replacement(parts.get(1).map_or("", |s| s.as_str()));

        let (first, last) = lines.unwrap_or_else(|| self.line_range(1));
        let (start, end) = (self.buffer.line_start(first), self.buffer.line_end(last));
        let region = self.buffer.slice_string(start, end);

        // Line by line, because `:s` is a line-oriented command: without `g` it
        // is the *first* match on each line, and `^`/`$` anchor to the line and
        // not to the region.
        let mut count = 0usize;
        let mut changed = None;
        let mut out: Vec<String> = Vec::new();
        for text in region.split('\n') {
            let hits = re.find_iter(text).count();
            if hits == 0 {
                out.push(text.to_string());
                continue;
            }
            count += if global { hits } else { 1 };
            changed = Some(out.len());
            let new = match global {
                true => re.replace_all(text, rep.as_str()),
                false => re.replace(text, rep.as_str()),
            };
            out.push(new.into_owned());
        }
        let Some(changed) = changed else {
            return Some(vec![EditorCommand::Message(format!(
                "pattern not found: {pat}"
            ))]);
        };
        // vim leaves the cursor on the last line it touched, which for `:%s` is
        // the difference between "it worked" and being thrown to the top of the
        // file. Counted in the *new* text, since that is what will be there.
        let at = start + out[..changed].iter().map(|l| l.chars().count() + 1).sum::<usize>();

        let mut cmds = Vec::new();
        if self.mode.is_visual() {
            cmds.push(EditorCommand::SetMode(Mode::Normal));
        }
        cmds.extend([
            EditorCommand::Checkpoint,
            EditorCommand::DeleteRange(start, end),
            // `DeleteRange` leaves the cursor at `start` — except on an empty
            // range, which it refuses outright. Say it, rather than relying on
            // it, or `:s/^/# /` on a blank line inserts wherever point was.
            EditorCommand::MoveTo(start),
            EditorCommand::InsertText(out.join("\n")),
            EditorCommand::MoveTo(at),
            EditorCommand::Message(match count {
                1 => "1 substitution".to_string(),
                n => format!("{n} substitutions"),
            }),
        ]);
        Some(cmds)
    }

    /// `count` lines starting at the cursor's, as an inclusive line range.
    fn line_range(&self, count: usize) -> (usize, usize) {
        let (line, _) = self.buffer.cursor_line_col();
        let last = self.buffer.len_lines().saturating_sub(1);
        (line, (line + count.max(1) - 1).min(last))
    }

    /// The inclusive line range of the visual selection, if there is one.
    fn selection_lines(&self) -> Option<(usize, usize)> {
        let (a, b) = self.selection()?;
        let to_line = |c: usize| self.buffer.text.char_to_line(c.min(self.buffer.len_chars()));
        Some((to_line(a), to_line(b.saturating_sub(1))))
    }

    fn ex_command(&mut self, line: &str) -> Vec<EditorCommand> {
        let line = line.trim();
        // Substitute before the split below: `s/a/b/g` has no whitespace in it,
        // so the generic parse would take the whole line for a command name.
        if let Some(cmds) = self.substitute(line) {
            return cmds;
        }
        let (cmd, arg) = match line.split_once(char::is_whitespace) {
            Some((c, a)) => (c, a.trim()),
            None => (line, ""),
        };
        match cmd {
            "" => vec![],
            "q" | "q!" | "quit" => vec![EditorCommand::Quit],
            "w" => vec![EditorCommand::SaveFile(
                (!arg.is_empty()).then(|| PathBuf::from(arg)),
            )],
            "wq" | "x" => vec![
                EditorCommand::SaveFile((!arg.is_empty()).then(|| PathBuf::from(arg))),
                EditorCommand::Quit,
            ],
            "e" | "edit" => {
                if arg.is_empty() {
                    vec![EditorCommand::Message("usage: :e <path>".into())]
                } else {
                    vec![EditorCommand::OpenFile(PathBuf::from(expand_tilde(arg)))]
                }
            }
            "dashboard" => vec![EditorCommand::ShowDashboard],
            "lisp" | "eval" => {
                if arg.is_empty() {
                    vec![EditorCommand::Message("usage: :lisp <form>".into())]
                } else {
                    vec![EditorCommand::CallLisp(arg.to_string())]
                }
            }
            other => vec![EditorCommand::Message(format!("unknown command: :{other}"))],
        }
    }

    // --- dashboard -------------------------------------------------------

    fn dashboard_key(&mut self, key: Key) -> Vec<EditorCommand> {
        // Single-key bindings work here too, so `(define-key "dashboard" ...)`
        // means something. Item hotkeys below win only if nothing is bound.
        if let Some(cmd) = self.keymap.get(&(Mode::Dashboard, key.token())) {
            let action = cmd.clone();
            return self.run_action(&action);
        }
        let n = self.dashboard.len();
        match key {
            Key::Char('j') | Key::Down | Key::Tab if n > 0 => {
                self.dashboard.selected = (self.dashboard.selected + 1) % n;
                vec![]
            }
            Key::Char('k') | Key::Up if n > 0 => {
                self.dashboard.selected = (self.dashboard.selected + n - 1) % n;
                vec![]
            }
            Key::Enter | Key::Char('l') => {
                let action = self
                    .dashboard
                    .entries()
                    .get(self.dashboard.selected)
                    .map(|i| i.action.clone());
                action.map(|a| self.run_action(&a)).unwrap_or_default()
            }
            // An item hotkey, but only when nothing is already part-typed:
            // mid-sequence, `f` belongs to `SPC f f` and not to the dashboard's
            // "Find file" item.
            Key::Char(c)
                if self.pending.keys.is_empty()
                    && self.dashboard.entries().iter().any(|i| i.key == c) =>
            {
                let action = self
                    .dashboard
                    .entries()
                    .iter()
                    .find(|i| i.key == c)
                    .map(|i| i.action.clone());
                action.map(|a| self.run_action(&a)).unwrap_or_default()
            }
            Key::Esc => vec![EditorCommand::SetMode(Mode::Normal)],
            // Everything the dashboard does not claim is an ordinary Normal
            // key. Without this the startup screen was a dead end: no `M-x`, no
            // `M-o`, no `M-RET` to split it, because a `Char` arm swallowed
            // every letter and every other key answered with nothing.
            key => self.normal_key(key),
        }
    }

    /// A named command: a few built-in verbs, anything else is a Lisp call.
    /// Shared by dashboard items and key bindings, so the two name the same
    /// things.
    ///
    /// Mode-neutral by design — a key bound in Visual mode must not silently
    /// drop you into Normal. The dashboard leaves itself via the commands that
    /// load a buffer (`config`, `open:`) or via `scratch`.
    ///
    /// lisp-api: `pub`, so the image can *call* a name it can already *bind*.
    /// It answers a batch rather than one command — and some of that batch may
    /// need the app layer — which is why the Lisp side of it lives in `rs_do`
    /// beside `paste` rather than being an arm of `command_for`.
    pub fn run_action(&mut self, action: &str) -> Vec<EditorCommand> {
        match action {
            "quit" => vec![EditorCommand::Quit],
            "scratch" => vec![EditorCommand::SetMode(Mode::Normal)],
            "find-file" => {
                self.open_prompt(PromptKind::File);
                vec![]
            }
            // `M-x`: run any Lisp function by name, with completion.
            "execute-command" | "M-x" => {
                self.open_prompt(PromptKind::Command);
                vec![]
            }
            "switch-buffer" => {
                self.open_prompt(PromptKind::Buffer);
                vec![]
            }
            // Named after the result, not the divider: "horizontal split" means
            // opposite things in vim and Emacs, "right"/"below" means one thing.
            "split-window-right" => vec![EditorCommand::SplitWindow(frame::Split::Columns)],
            "split-window-below" => vec![EditorCommand::SplitWindow(frame::Split::Rows)],
            "delete-window" => vec![EditorCommand::CloseWindow],
            "other-window" => vec![EditorCommand::FocusNextWindow],
            // Lisp evaluation. `CallLisp` carries *source*, not a function
            // name, so core can hand the image any slice of the live buffer —
            // no round trip through disk, and nothing to save first.
            "eval-buffer" => self.eval_text(self.buffer.text.to_string()),
            "eval-region" => match self.selection() {
                Some((a, b)) => {
                    let src = self.buffer.slice_string(a, b);
                    let mut cmds = vec![EditorCommand::SetMode(Mode::Normal)];
                    cmds.extend(self.eval_text(src));
                    cmds
                }
                None => vec![EditorCommand::Message("no selection".into())],
            },
            "eval-last-sexp" => match self.buffer.last_top_level_form(self.buffer.cursor + 1) {
                Some((a, b)) => self.eval_text(self.buffer.slice_string(a, b)),
                None => vec![EditorCommand::Message("no complete form before point".into())],
            },
            // What `C-c` is bound to: the selection if there is one, else the
            // form under point, else the whole buffer.
            "eval-dwim" => {
                // In a commit message, `C-c` means "finish the commit" — the
                // buffer decides, so one binding covers both without taking
                // C-c away from everywhere else.
                if self.buffer.kind == crate::BufferKind::CommitMessage {
                    self.run_action("magit-commit-finish")
                } else if self.mode.is_visual() {
                    self.run_action("eval-region")
                } else if self.buffer.last_top_level_form(self.buffer.cursor + 1).is_some() {
                    self.run_action("eval-last-sexp")
                } else {
                    self.run_action("eval-buffer")
                }
            }
            // Magit and dired. Core knows the verbs and nothing else; the app
            // runs git and touches the filesystem.
            other if other.starts_with("magit-") => {
                vec![EditorCommand::Git(other["magit-".len()..].to_string())]
            }
            other if other.starts_with("dired-") => {
                vec![EditorCommand::Dired(other["dired-".len()..].to_string())]
            }
            "dired" => vec![EditorCommand::Dired("open".into())],
            // `terminal-normal` and `terminal-insert` go to the app like every
            // other verb: stepping out of the shell means loading the scrollback
            // into the buffer, and core has no scrollback to load.
            other if other.starts_with("project-") => {
                vec![EditorCommand::Project(other["project-".len()..].to_string())]
            }
            other if other.starts_with("terminal-") => {
                vec![EditorCommand::Term(other["terminal-".len()..].to_string())]
            }
            "terminal" | "term" | "shell" => vec![EditorCommand::Term("open".into())],
            // `consult-line`: pick a line by fuzzy match, with live preview.
            "search-line" | "consult-line" | "goto-line" => {
                self.open_prompt(PromptKind::Line);
                vec![]
            }
            // `consult-ripgrep`: the same gesture across the whole project.
            // Candidates arrive from a subprocess, which the app runs.
            "search-project" | "consult-ripgrep" | "ripgrep" | "grep" => {
                self.open_prompt(PromptKind::Grep);
                vec![]
            }
            // `ace-window`: with two windows there is nothing to choose, so it
            // just switches — which is what ace-window itself does.
            "ace-window" => {
                if self.frame().windows.len() <= 2 {
                    vec![EditorCommand::FocusNextWindow]
                } else {
                    self.ace = Some(self.frame().ace_labels());
                    vec![EditorCommand::Message("window: press a label".into())]
                }
            }
            // `M-x org-mode` sets the major mode, the way Emacs does. Anything
            // ending in `-mode` that is not a Lisp command means this.
            other if other.ends_with("-mode") && !self.commands.iter().any(|c| c == other) => {
                vec![EditorCommand::SetMajorMode(other.to_string())]
            }
            "new-frame" => vec![EditorCommand::NewFrame],
            "delete-frame" => vec![EditorCommand::CloseFrame],
            "config" => vec![EditorCommand::OpenFile(PathBuf::from("@init"))],
            other if other.starts_with("open:") => vec![EditorCommand::OpenFile(PathBuf::from(
                expand_tilde(&other[5..]),
            ))],
            other => vec![EditorCommand::CallLisp(format!("({other})"))],
        }
    }
}

// --- regex ---------------------------------------------------------------

/// The characters vim and PCRE disagree about, and they disagree *only* about
/// the backslash: in vim's default magic level `(` `)` `{` `}` `|` `+` `?` are
/// literal text and `\(` `\)` … are the operators. Exactly backwards.
const SWAPPED: &str = "(){}|+?";

/// A vim pattern, translated into the dialect the `regex` crate speaks.
///
/// Worth doing rather than telling everyone to write PCRE, because the whole
/// value of `/` is that it takes what your fingers already know: `\(foo\|bar\)`
/// has to mean a group, and `foo(1)` has to find a literal `foo(1)`. Swapping
/// the backslash on [`SWAPPED`], mapping `\<`/`\>` onto `\b` and honouring
/// `\c` is all of the difference that gets used.
///
/// Returns the pattern and whether it asked for case folding.
///
/// ponytail: the ceiling is everything vim spells with a backslash that has no
/// PCRE spelling at all — `\zs`/`\ze` (match boundaries inside a match),
/// `\{-}` (non-greedy), `\%(`, `\%V`, `\&`, and the `\v`/`\V`/`\M` magic-level
/// switches. Each passes straight through and therefore fails to compile,
/// which is the honest failure: you get "bad pattern" and not a silently
/// different match. Add them here, one at a time; there is nowhere else they
/// could go. Also passing through unchanged, and meaning the wrong thing:
/// `~` (vim's "the last replacement") is a literal tilde here.
fn vim_regex(pat: &str) -> (String, bool) {
    let mut out = String::with_capacity(pat.len() + 8);
    let mut fold = false;
    let mut chars = pat.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                None => out.push_str("\\\\"),
                Some('<' | '>') => out.push_str("\\b"),
                Some('=') => out.push('?'),
                Some('c') => fold = true,
                Some('C') => {}
                Some(e) if SWAPPED.contains(e) => out.push(e),
                // `\.`, `\*`, `\[`, `\\`, and the classes `\w` `\s` `\d` `\b`,
                // all of which already mean the same thing in both.
                Some(e) => {
                    out.push('\\');
                    out.push(e);
                }
            },
            c if SWAPPED.contains(c) => {
                out.push('\\');
                out.push(c);
            }
            // `.` `*` `[` `]` `^` `$` need no help: same character, same job.
            c => out.push(c),
        }
    }
    (out, fold)
}

fn compile(pat: &str, ignore_case: bool) -> Option<Regex> {
    let (src, fold) = vim_regex(pat);
    let src = match fold || ignore_case {
        true => format!("(?i){src}"),
        false => src,
    };
    Regex::new(&src).ok()
}

/// Vim writes `\1` for a capture and `&` for the whole match; the `regex` crate
/// writes `${1}` and `${0}`. Braced, so `\1x` is group 1 followed by an `x`
/// rather than a group named `1x`.
fn vim_replacement(rep: &str) -> String {
    let mut out = String::with_capacity(rep.len());
    let mut chars = rep.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(d @ '0'..='9') => out.push_str(&format!("${{{d}}}")),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(e) => out.push(e),
                None => {}
            },
            '&' => out.push_str("${0}"),
            // A literal `$` in the replacement, which is the engine's sigil.
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out
}

/// Split on unescaped `delim`, dropping the backslash from `\<delim>` only.
///
/// Every other escape has to reach the engine intact — `\/` is a literal slash
/// in `:s/a\/b/c/`, but `\.` is still "any dot" and must not become one.
fn split_delim(s: &str, delim: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut escaped = false;
    for c in s.chars() {
        let tail = parts.last_mut().expect("never empty");
        match (escaped, c) {
            (true, c) => {
                if c != delim {
                    tail.push('\\');
                }
                tail.push(c);
                escaped = false;
            }
            (false, '\\') => escaped = true,
            (false, c) if c == delim => parts.push(String::new()),
            (false, c) => tail.push(c),
        }
    }
    if escaped {
        parts.last_mut().expect("never empty").push('\\');
    }
    parts
}

fn expand_tilde(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => format!("{}/{rest}", home.to_string_lossy()),
            None => p.to_string(),
        },
        None => p.to_string(),
    }
}

// --- word motions --------------------------------------------------------

/// `-` counts as a word char: this editor is mostly used on Lisp.
fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' || c == '-' {
        1
    } else {
        2
    }
}

fn word_forward(buf: &crate::Buffer, pos: usize) -> usize {
    let n = buf.len_chars();
    let mut i = pos;
    let start_class = buf.char_at(i).map(class).unwrap_or(0);
    if start_class != 0 {
        while i < n && buf.char_at(i).map(class) == Some(start_class) {
            i += 1;
        }
    }
    while i < n && buf.char_at(i).map(class) == Some(0) {
        i += 1;
    }
    i.min(n)
}

fn word_backward(buf: &crate::Buffer, pos: usize) -> usize {
    let mut i = pos;
    if i == 0 {
        return 0;
    }
    i -= 1;
    while i > 0 && buf.char_at(i).map(class) == Some(0) {
        i -= 1;
    }
    let c = buf.char_at(i).map(class).unwrap_or(0);
    while i > 0 && buf.char_at(i - 1).map(class) == Some(c) {
        i -= 1;
    }
    i
}

fn word_end(buf: &crate::Buffer, pos: usize) -> usize {
    let n = buf.len_chars();
    let mut i = pos + 1;
    while i < n && buf.char_at(i).map(class) == Some(0) {
        i += 1;
    }
    let c = buf.char_at(i).map(class).unwrap_or(0);
    while i + 1 < n && buf.char_at(i + 1).map(class) == Some(c) {
        i += 1;
    }
    i.min(n.saturating_sub(1))
}

/// Next/previous blank line — `{` and `}`.
fn paragraph(buf: &crate::Buffer, line: usize, forward: bool) -> usize {
    let last = buf.len_lines().saturating_sub(1);
    let blank = |l: usize| buf.line_len(l) == 0;
    if forward {
        ((line + 1)..=last).find(|&l| blank(l)).unwrap_or(last)
    } else {
        (0..line).rev().find(|&l| blank(l)).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::{feed, fresh};
    use crate::*;

    fn keys(s: &str) -> Vec<Key> {
        s.chars().map(Key::Char).collect()
    }

    /// In a terminal the shell owns the keyboard. Every one of these would
    /// otherwise be an editor command, and `d` deleting a word instead of
    /// reaching the shell is the whole bug this guards.
    #[test]
    fn terminal_mode_hands_keys_to_the_shell() {
        let mut ed = fresh("");
        ed.apply(EditorCommand::SetMode(Mode::Terminal));
        for key in [
            Key::Char('d'),
            Key::Char('j'),
            Key::Char(':'),
            Key::Esc,
            Key::Enter,
            // C-c above all: reaching the editor instead would mean never being
            // able to interrupt a running program.
            Key::Ctrl('c'),
        ] {
            assert_eq!(
                ed.handle_key(key),
                vec![EditorCommand::TermKey(key)],
                "{key:?} must go to the shell"
            );
        }
        assert_eq!(ed.mode, Mode::Terminal, "and none of them leaves the mode");
    }

    /// ...but there has to be a way out, and it comes from the Terminal keymap
    /// so `init.lisp` chooses it.
    #[test]
    fn the_terminal_keymap_is_the_way_out() {
        let mut ed = fresh("");
        ed.apply(EditorCommand::BindKey {
            mode: "terminal".into(),
            keys: "C-M-t".into(),
            command: "terminal-normal".into(),
        });
        ed.apply(EditorCommand::SetMode(Mode::Terminal));
        // The app does the mode change, because stepping out means loading the
        // scrollback into the buffer and core has no scrollback to load.
        assert_eq!(
            ed.handle_key(Key::CtrlMeta('t')),
            vec![EditorCommand::Term("normal".into())]
        );

        // A *Ctrl* binding from another mode must not be consulted here — that
        // is what stops a global `C-c` stealing SIGINT from the shell.
        ed.apply(EditorCommand::BindKey {
            mode: "normal".into(),
            keys: "C-c".into(),
            command: "eval-dwim".into(),
        });
        assert_eq!(
            ed.handle_key(Key::Ctrl('c')),
            vec![EditorCommand::TermKey(Key::Ctrl('c'))]
        );
    }

    /// ...but a Command-based binding *is*, so the editor stays reachable from
    /// inside a shell. This is the difference between the two halves of the
    /// policy, and the reason it is Command and not Ctrl.
    #[test]
    fn command_keys_fall_through_to_the_normal_keymap_in_a_terminal() {
        let mut ed = fresh("");
        for (keys, command) in [("M-x", "execute-command"), ("C-M-j", "switch-buffer")] {
            ed.apply(EditorCommand::BindKey {
                mode: "normal".into(),
                keys: keys.into(),
                command: command.into(),
            });
        }
        ed.apply(EditorCommand::SetMode(Mode::Terminal));

        // `M-x` opens its prompt rather than typing an `x` at the shell.
        ed.handle_key(Key::Meta('x'));
        assert!(ed.prompt.is_some(), "M-x must still work inside a terminal");
        ed.apply(EditorCommand::SetMode(Mode::Terminal));
        ed.prompt = None;

        ed.handle_key(Key::CtrlMeta('j'));
        assert!(
            ed.prompt.is_some(),
            "C-M-j must reach the buffer switcher, not the shell"
        );
        ed.prompt = None;

        // The plain keys are untouched by all of this: they still type.
        ed.apply(EditorCommand::SetMode(Mode::Terminal));
        assert_eq!(
            ed.handle_key(Key::Char('x')),
            vec![EditorCommand::TermKey(Key::Char('x'))]
        );
    }

    #[test]
    fn a_terminal_buffer_is_read_only_and_named() {
        let mut ed = fresh("");
        ed.show_special(BufferKind::Terminal, "$ ls\n");
        assert_eq!(ed.buffer.name(), "*terminal*");
        assert!(ed.buffer.kind.is_generated());
        assert_eq!(ed.mode, Mode::Terminal);
        // Typing at it does not edit the flattened grid: the shell owns what is
        // on screen, and an edit would be silently overwritten next frame.
        ed.apply(EditorCommand::InsertText("nope".into()));
        assert_eq!(ed.buffer.text.to_string(), "$ ls\n");
    }

    #[test]
    fn dw_deletes_a_word() {
        let mut ed = fresh("hello world");
        feed(&mut ed, &keys("dw"));
        assert_eq!(ed.buffer.text.to_string(), "world");
    }

    #[test]
    fn count_multiplies_motion() {
        let mut ed = fresh("one two three four");
        feed(&mut ed, &keys("3w"));
        assert_eq!(ed.buffer.cursor, 14);
    }

    #[test]
    fn d2w_deletes_two_words() {
        let mut ed = fresh("one two three");
        feed(&mut ed, &keys("d2w"));
        assert_eq!(ed.buffer.text.to_string(), "three");
    }

    #[test]
    fn dd_deletes_the_line() {
        let mut ed = fresh("aaa\nbbb\nccc");
        feed(&mut ed, &keys("jdd"));
        assert_eq!(ed.buffer.text.to_string(), "aaa\nccc");
    }

    #[test]
    fn yy_then_p_duplicates_a_line() {
        let mut ed = fresh("aaa\nbbb");
        feed(&mut ed, &keys("yyp"));
        assert_eq!(ed.buffer.text.to_string(), "aaa\naaa\nbbb");
    }

    #[test]
    fn gg_and_goto_line_jump() {
        let mut ed = fresh("a\nb\nc\nd");
        feed(&mut ed, &keys("G"));
        assert_eq!(ed.buffer.cursor_line_col().0, 3);
        feed(&mut ed, &keys("gg"));
        assert_eq!(ed.buffer.cursor_line_col().0, 0);
        feed(&mut ed, &keys("2G"));
        assert_eq!(ed.buffer.cursor_line_col().0, 1);
    }

    #[test]
    fn dollar_and_zero() {
        let mut ed = fresh("hello");
        feed(&mut ed, &keys("$"));
        assert_eq!(ed.buffer.cursor_line_col().1, 4);
        feed(&mut ed, &keys("0"));
        assert_eq!(ed.buffer.cursor_line_col().1, 0);
    }

    #[test]
    fn find_char_moves_to_it() {
        let mut ed = fresh("alpha beta");
        feed(&mut ed, &keys("fb"));
        assert_eq!(ed.buffer.cursor, 6);
    }

    #[test]
    fn o_opens_a_line_below() {
        let mut ed = fresh("aaa\nbbb");
        feed(&mut ed, &keys("ox"));
        assert_eq!(ed.buffer.text.to_string(), "aaa\nx\nbbb");
        assert_eq!(ed.mode, Mode::Insert);
    }

    #[test]
    fn r_replaces_one_char() {
        let mut ed = fresh("cat");
        feed(&mut ed, &keys("rb"));
        assert_eq!(ed.buffer.text.to_string(), "bat");
    }

    #[test]
    fn visual_d_deletes_selection() {
        let mut ed = fresh("abcdef");
        feed(&mut ed, &keys("vlld"));
        assert_eq!(ed.buffer.text.to_string(), "def");
        assert_eq!(ed.mode, Mode::Normal);
    }

    #[test]
    fn visual_line_d_deletes_whole_lines() {
        let mut ed = fresh("aaa\nbbb\nccc");
        feed(&mut ed, &keys("Vjd"));
        assert_eq!(ed.buffer.text.to_string(), "ccc");
    }

    #[test]
    fn search_moves_to_match() {
        let mut ed = fresh("alpha\nbeta\ngamma");
        feed(&mut ed, &[Key::Char('/')]);
        feed(&mut ed, &keys("gam"));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.cursor_line_col().0, 2);
    }

    fn bind_and_press(ed: &mut Editor, keys: &str, command: &str) -> Vec<EditorCommand> {
        ed.apply(EditorCommand::BindKey {
            mode: "normal".into(),
            keys: keys.into(),
            command: command.into(),
        });
        keys.split_whitespace()
            .map(|t| match t {
                "SPC" => Key::Char(' '),
                t if t.len() > 2 && t.starts_with("M-") => {
                    Key::Meta(t[2..].chars().next().unwrap())
                }
                t if t.len() > 2 && t.starts_with("C-") => {
                    Key::Ctrl(t[2..].chars().next().unwrap())
                }
                other => Key::Char(other.chars().next().unwrap()),
            })
            .collect::<Vec<_>>()
            .iter()
            .flat_map(|&k| ed.handle_key(k))
            // Every key of a sequence but the last is a prefix, and a prefix
            // now hands what is pending to which-key. That is not what any of
            // these tests is asking about.
            .filter(|c| !matches!(c, EditorCommand::CallLisp(s) if s.contains("which-key")))
            .collect()
    }

    /// The shipped config's `C-c r` — the binding TODO.org asked for and that
    /// was impossible until a mode-local prefix started outranking a global
    /// exact binding. Written against the real pair of bindings, because what
    /// makes it work is precisely that they collide.
    #[test]
    fn org_c_c_r_previews_rather_than_evaluating() {
        let mut ed = fresh("(the-line)");
        ed.apply(EditorCommand::BindKey {
            mode: "normal".into(),
            keys: "C-c".into(),
            command: "eval-dwim".into(),
        });
        ed.apply(EditorCommand::BindKey {
            mode: "org-mode".into(),
            keys: "C-c r".into(),
            command: "org-latex-preview".into(),
        });

        // Not an org buffer: `C-c` is still whole, and still evaluates — which
        // it does by handing the *line* to Lisp, so that is what to look for.
        let cmds = ed.handle_key(Key::Ctrl('c'));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::CallLisp(s) if s.contains("(the-line)"))),
            "C-c outside org-mode must still evaluate the line, got {cmds:#?}"
        );

        ed.buffer.major_mode = "org-mode".into();
        // Now `C-c` is a prefix and must *not* fire the global binding. The one
        // thing it may emit is which-key's own report that a prefix is pending.
        let cmds = ed.handle_key(Key::Ctrl('c'));
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, EditorCommand::CallLisp(s) if s.contains("(the-line)"))),
            "C-c in org-mode must wait for the next key, got {cmds:#?}"
        );
        // ...and the sequence completes to the org command.
        let cmds = ed.handle_key(Key::Char('r'));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::CallLisp(s) if s.contains("org-latex-preview"))),
            "C-c r in org-mode must preview, got {cmds:#?}"
        );
    }

    /// A listing is a buffer you browse, not a room with the door shut: the
    /// leader key, `M-x` and the window verbs all have to survive it.
    #[test]
    fn normal_bindings_reach_dired_magit_and_the_dashboard() {
        for mode in [Mode::Dired, Mode::Magit, Mode::Dashboard] {
            let mut ed = fresh("a\nb\nc\n");
            ed.apply(EditorCommand::BindKey {
                mode: "normal".into(),
                keys: "M-x".into(),
                command: "execute-command".into(),
            });
            ed.apply(EditorCommand::BindKey {
                mode: "normal".into(),
                keys: "SPC f f".into(),
                command: "find-file".into(),
            });
            ed.apply(EditorCommand::SetMode(mode));

            // `execute-command` puts the picker up rather than answering with
            // a command, so the prompt is what "it fired" looks like.
            ed.handle_key(Key::Meta('x'));
            assert!(
                matches!(&ed.prompt, Some(p) if p.kind == PromptKind::Command),
                "M-x must open the command picker in {mode:?}"
            );
            ed.handle_key(Key::Esc);

            // And a *sequence*, which needs the prefix scan to look in the
            // Normal keymap too rather than giving up after `SPC`.
            for key in [Key::Char(' '), Key::Char('f')] {
                assert!(
                    ed.handle_key(key).iter().all(|c| matches!(
                        c,
                        EditorCommand::CallLisp(s) if s.contains("which-key")
                    )),
                    "{mode:?} must still be waiting mid-sequence"
                );
            }
            ed.handle_key(Key::Char('f'));
            assert!(
                matches!(&ed.prompt, Some(p) if p.kind == PromptKind::File),
                "SPC f f must complete in {mode:?}"
            );
        }
    }

    /// The other half: the grammar reaching a listing must not let you into a
    /// mode where every keystroke is refused.
    #[test]
    fn insert_is_refused_in_a_generated_buffer() {
        let mut ed = fresh("a\nb\n");
        ed.show_special(crate::BufferKind::Dired, "a\nb\n");
        let before = ed.mode;
        let cmds = ed.handle_key(Key::Char('i'));
        assert_eq!(ed.mode, before, "`i` must not enter Insert in dired");
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, EditorCommand::SetMode(Mode::Insert))),
            "got {cmds:#?}"
        );
    }

    #[test]
    fn user_keymap_beats_builtin() {
        // `d` alone is the delete operator; bound, it must reach Lisp instead.
        let mut ed = fresh("abc");
        let out = bind_and_press(&mut ed, "d", "my-command");
        assert_eq!(out, vec![EditorCommand::CallLisp("(my-command)".into())]);
        assert_eq!(ed.buffer.text.to_string(), "abc");
    }

    #[test]
    fn a_bound_key_resolves_builtin_verbs_too() {
        // Binding to `find-file` must open the prompt, not call a Lisp function
        // of that name — the primitive wants a path and cannot ask for one.
        let mut ed = fresh("abc");
        let out = bind_and_press(&mut ed, "SPC f f", "find-file");
        assert!(!out.iter().any(|c| matches!(c, EditorCommand::CallLisp(_))));
        assert_eq!(
            ed.prompt.as_ref().map(|p| p.kind),
            Some(crate::PromptKind::File)
        );

        let mut ed = fresh("abc");
        assert!(bind_and_press(&mut ed, "SPC q q", "quit").contains(&EditorCommand::Quit));
    }

    // --- regressions found by review -------------------------------------

    #[test]
    fn undo_after_open_does_not_resurrect_the_previous_buffer() {
        let mut ed = fresh("FIRST\n");
        ed.load("SECOND\n", Some("/tmp/b.rs".into()), None);
        feed(&mut ed, &keys("u"));
        assert_eq!(ed.buffer.text.to_string(), "SECOND\n");
    }

    #[test]
    fn undo_reverses_one_insert_session() {
        let mut ed = fresh("hello");
        feed(&mut ed, &[Key::Char('i'), Key::Char('X'), Key::Char('Y'), Key::Esc]);
        assert_eq!(ed.buffer.text.to_string(), "XYhello");
        feed(&mut ed, &keys("u"));
        assert_eq!(ed.buffer.text.to_string(), "hello");
    }

    #[test]
    fn r_on_a_blank_line_does_nothing() {
        let mut ed = fresh("aaa\n\nbbb");
        feed(&mut ed, &keys("jrX"));
        assert_eq!(ed.buffer.text.to_string(), "aaa\n\nbbb");
        let mut empty = fresh("");
        feed(&mut empty, &keys("rZ"));
        assert_eq!(empty.buffer.text.to_string(), "");
    }

    #[test]
    fn counted_j_joins_that_many_lines() {
        let mut ed = fresh("aaa\nbbb\nccc\nddd");
        feed(&mut ed, &keys("3J"));
        assert_eq!(ed.buffer.text.to_string(), "aaa bbb ccc\nddd");
        let mut two = fresh("aaa\n   bbb");
        feed(&mut two, &keys("J"));
        assert_eq!(two.buffer.text.to_string(), "aaa bbb");
    }

    #[test]
    fn linewise_paste_on_the_last_line_opens_a_line() {
        let mut ed = fresh("aaa\nbbb");
        feed(&mut ed, &keys("yyjp"));
        assert_eq!(ed.buffer.text.to_string(), "aaa\nbbb\naaa");
    }

    #[test]
    fn deleting_a_blank_line_char_keeps_the_register() {
        let mut ed = fresh("aaa\n\nbbb");
        feed(&mut ed, &keys("yyjxp"));
        assert_eq!(ed.buffer.text.to_string(), "aaa\n\naaa\nbbb");
    }

    #[test]
    fn dd_on_the_last_line_leaves_no_phantom() {
        let mut ed = fresh("aaa\nbbb\nccc");
        feed(&mut ed, &keys("Gdd"));
        assert_eq!(ed.buffer.text.to_string(), "aaa\nbbb");
    }

    #[test]
    fn scroll_never_walks_a_short_file_off_screen() {
        let mut ed = fresh("a\nb\nc\nd\ne");
        ed.viewport_lines = 33;
        feed(&mut ed, &[Key::Ctrl('d')]);
        assert_eq!(ed.scroll, 0);
    }

    #[test]
    fn a_long_count_does_not_overflow() {
        let mut ed = fresh("abc");
        feed(&mut ed, &keys("99999999999999999999999j"));
        assert_eq!(ed.buffer.cursor_line_col().0, 0);
    }

    #[test]
    fn insert_and_dashboard_bindings_fire() {
        let mut ed = fresh("abc");
        ed.apply(EditorCommand::BindKey {
            mode: "insert".into(),
            keys: "M-+".into(),
            command: "text-scale-increase".into(),
        });
        feed(&mut ed, &keys("i"));
        assert_eq!(ed.mode, Mode::Insert);
        let out = ed.handle_key(Key::Meta('+'));
        assert_eq!(
            out,
            vec![EditorCommand::CallLisp("(text-scale-increase)".into())]
        );
        // and typing still inserts
        assert_eq!(ed.handle_key(Key::Char('z')), vec![EditorCommand::InsertChar('z')]);

        let mut ed = Editor::new();
        ed.apply(EditorCommand::BindKey {
            mode: "dashboard".into(),
            keys: "z".into(),
            command: "lisp-version".into(),
        });
        assert_eq!(
            ed.handle_key(Key::Char('z')),
            vec![EditorCommand::CallLisp("(lisp-version)".into())]
        );
    }

    #[test]
    fn meta_keys_tokenize_as_emacs_writes_them() {
        assert_eq!(Key::Meta('+').token(), "M-+");
        assert_eq!(Key::Meta('-').token(), "M--");
        assert_eq!(normalize_keys("M-+"), "M-+");
        assert_eq!(normalize_keys("M--"), "M--");
    }

    #[test]
    fn m_x_completes_and_calls_a_lisp_command() {
        let mut ed = fresh("abc");
        ed.commands = vec![
            "text-scale-increase".into(),
            "find-file".into(),
            "switch-buffer".into(),
        ];
        let out = bind_and_press(&mut ed, "M-x", "execute-command");
        assert!(out.is_empty());
        assert_eq!(ed.prompt.as_ref().map(|p| p.kind), Some(PromptKind::Command));

        // fuzzy-match down to one command, then run it
        feed(&mut ed, &keys("tsi"));
        assert_eq!(
            ed.prompt.as_ref().and_then(|p| p.current()),
            Some("text-scale-increase")
        );
        let out: Vec<_> = ed.handle_key(Key::Enter);
        assert_eq!(
            out,
            vec![EditorCommand::CallLisp("(text-scale-increase)".into())]
        );
        assert!(ed.prompt.is_none());
    }

    #[test]
    fn m_x_offers_builtin_verbs_as_well_as_lisp_commands() {
        let mut ed = fresh("");
        ed.commands = vec!["reload-config".into()];
        ed.open_prompt(PromptKind::Command);
        let items = &ed.prompt.as_ref().unwrap().items;
        // built-in verbs are runnable by name without any Lisp defining them
        for verb in ["new-frame", "split-window-right", "eval-buffer", "quit"] {
            assert!(items.iter().any(|i| i == verb), "{verb} missing from M-x");
        }
        assert!(items.iter().any(|i| i == "reload-config"));

        feed(&mut ed, &keys("nfr"));
        assert_eq!(ed.prompt.as_ref().unwrap().current(), Some("new-frame"));
        for cmd in ed.handle_key(Key::Enter) {
            ed.apply(cmd);
        }
        assert_eq!(ed.frames.len(), 2);
    }

    #[test]
    fn m_x_can_run_a_command_that_was_never_registered() {
        let mut ed = fresh("");
        ed.open_prompt(PromptKind::Command);
        feed(&mut ed, &keys("my-thing"));
        assert_eq!(
            ed.handle_key(Key::Enter),
            vec![EditorCommand::CallLisp("(my-thing)".into())]
        );
    }

    #[test]
    fn prompt_navigation_accepts_both_spellings() {
        let mut ed = fresh("");
        // A prefix no built-in verb matches, so the list is just these three.
        ed.commands = vec!["qzz-aaa".into(), "qzz-bbb".into(), "qzz-ccc".into()];
        ed.open_prompt(PromptKind::Command);
        feed(&mut ed, &keys("qzz-"));
        assert_eq!(ed.prompt.as_ref().unwrap().matches.len(), 3);
        assert_eq!(ed.prompt.as_ref().unwrap().current(), Some("qzz-aaa"));

        for (key, want) in [
            (Key::Ctrl('j'), "qzz-bbb"),
            (Key::Ctrl('n'), "qzz-ccc"),
            (Key::Down, "qzz-aaa"),
            (Key::Ctrl('k'), "qzz-ccc"),
            (Key::Ctrl('p'), "qzz-bbb"),
            (Key::Up, "qzz-aaa"),
        ] {
            ed.handle_key(key);
            assert_eq!(
                ed.prompt.as_ref().unwrap().current(),
                Some(want),
                "after {key:?}"
            );
        }
    }

    #[test]
    fn tab_completes_and_descends_directories() {
        let mut ed = fresh("");
        ed.open_prompt(PromptKind::File);
        // what the app would supply for the typed directory
        ed.prompt
            .as_mut()
            .unwrap()
            .set_items(vec!["src/core/".into(), "src/main.rs".into()]);

        ed.handle_key(Key::Tab);
        // the input became the directory, so the app will list *it* next
        assert_eq!(ed.prompt.as_ref().unwrap().text, "src/core/");

        // descending: new listing for that directory
        ed.prompt
            .as_mut()
            .unwrap()
            .set_items(vec!["src/core/lib.rs".into(), "src/core/evil.rs".into()]);
        ed.handle_key(Key::Tab);
        assert_eq!(ed.prompt.as_ref().unwrap().text, "src/core/lib.rs");

        // Tab again is stable: completing narrowed it to a single match, so
        // there is nothing to cycle to.
        ed.handle_key(Key::Tab);
        assert_eq!(ed.prompt.as_ref().unwrap().text, "src/core/lib.rs");
    }

    #[test]
    fn tab_cycles_when_completing_leaves_several_matches() {
        let mut ed = fresh("");
        ed.open_prompt(PromptKind::File);
        ed.prompt
            .as_mut()
            .unwrap()
            .set_items(vec!["notes.md".into(), "notes.md.bak".into()]);

        ed.handle_key(Key::Tab);
        assert_eq!(ed.prompt.as_ref().unwrap().text, "notes.md");
        // both still match, so Tab moves to the next one
        ed.handle_key(Key::Tab);
        assert_eq!(ed.prompt.as_ref().unwrap().current(), Some("notes.md.bak"));
    }

    #[test]
    fn mouse_scroll_moves_the_view_not_the_cursor() {
        let text: String = (0..100).map(|i| format!("line{i}\n")).collect();
        let mut ed = fresh(&text);
        ed.viewport_lines = 10;

        // cursor stays put while it is still on screen
        ed.apply(EditorCommand::ScrollLines(3));
        assert_eq!(ed.scroll, 3);
        assert_eq!(ed.buffer.cursor_line_col().0, 3);

        ed.apply(EditorCommand::ScrollLines(20));
        assert_eq!(ed.scroll, 23);
        // dragged along only as far as staying on screen requires
        assert_eq!(ed.buffer.cursor_line_col().0, 23);

        // and it clamps at both ends
        ed.apply(EditorCommand::ScrollLines(-1000));
        assert_eq!(ed.scroll, 0);
        ed.apply(EditorCommand::ScrollLines(10_000));
        assert_eq!(ed.scroll, ed.buffer.len_lines() - 10);
    }

    #[test]
    fn scrolling_a_file_shorter_than_the_window_does_nothing() {
        let mut ed = fresh("a\nb\nc");
        ed.viewport_lines = 40;
        ed.apply(EditorCommand::ScrollLines(5));
        assert_eq!(ed.scroll, 0);
    }

    #[test]
    fn m_x_works_on_the_dashboard() {
        // The mode the editor opens in — a binding missing here reads as M-x
        // being broken, because it is the first thing you would try.
        let mut ed = Editor::new();
        ed.apply(EditorCommand::BindKey {
            mode: "dashboard".into(),
            keys: "M-x".into(),
            command: "execute-command".into(),
        });
        ed.handle_key(Key::Meta('x'));
        assert_eq!(ed.prompt.as_ref().map(|p| p.kind), Some(PromptKind::Command));
    }

    #[test]
    fn vertical_motions_keep_the_column() {
        // Regression: `j` and `k` targeted the line *start*, so every vertical
        // motion silently snapped the cursor to column 0.
        let mut ed = fresh("abcdef\nghijkl\nmnopqr");
        feed(&mut ed, &keys("lll"));
        assert_eq!(ed.buffer.cursor_line_col(), (0, 3));
        feed(&mut ed, &keys("j"));
        assert_eq!(ed.buffer.cursor_line_col(), (1, 3));
        feed(&mut ed, &keys("j"));
        assert_eq!(ed.buffer.cursor_line_col(), (2, 3));
        feed(&mut ed, &keys("kk"));
        assert_eq!(ed.buffer.cursor_line_col(), (0, 3));

        // a short line clamps to its end rather than overshooting...
        let mut short = fresh("abcdef\nxy\nabcdef");
        feed(&mut short, &keys("llllj"));
        assert_eq!(short.buffer.cursor_line_col(), (1, 1));
        // ...and passing through it does not forget how far right we were
        feed(&mut short, &keys("j"));
        assert_eq!(short.buffer.cursor_line_col(), (2, 4));
        // any horizontal move sets a new column to hold
        feed(&mut short, &keys("hkk"));
        assert_eq!(short.buffer.cursor_line_col(), (0, 3));

        // and `dj` is still linewise — the column must not leak into the span
        let mut del = fresh("aaa\nbbb\nccc");
        feed(&mut del, &keys("ldj"));
        assert_eq!(del.buffer.text.to_string(), "ccc");
    }

    // --- folding ----------------------------------------------------------

    /// `ed` with `[start, end)` folded, the way `(fold-region ...)` does it.
    fn folded(ed: &mut Editor, start: usize, end: usize) {
        let id = ed.make_overlay(start, end);
        ed.apply(EditorCommand::Overlay(crate::OverlayEdit::Fold(id, true)));
    }

    /// A folded line does not occupy a row, and the *whole* consequence of that
    /// for the command loop is that `j` must not land on one — a cursor on a row
    /// the renderer skipped is a cursor nobody can see.
    #[test]
    fn j_and_k_step_over_a_folded_line() {
        // "* one\nbody\nmore\n* two", line starts 0, 6, 11, 16.
        let mut ed = fresh("* one\nbody\nmore\n* two");
        folded(&mut ed, 0, 15); // the heading's line stays; its body goes
        feed(&mut ed, &keys("j"));
        assert_eq!(ed.buffer.cursor_line_col(), (3, 0), "over both hidden lines");
        feed(&mut ed, &keys("k"));
        assert_eq!(ed.buffer.cursor_line_col(), (0, 0), "and back over them");
        // A count steps in visible lines, since that is what is on screen.
        let mut two = fresh("a\nb\nc\nd\ne");
        folded(&mut two, 0, 3); // hides "b"
        feed(&mut two, &keys("2j"));
        assert_eq!(two.buffer.cursor_line_col(), (3, 0));

        // A fold running to the end of the document leaves `j` where it is,
        // exactly as `j` on the last line already does.
        let mut tail = fresh("a\nb\nc");
        folded(&mut tail, 0, 5);
        feed(&mut tail, &keys("jjj"));
        assert_eq!(tail.buffer.cursor_line_col(), (0, 0));

        // `dj` over a closed fold takes the whole fold with it, which is vim's
        // answer too: the motion is linewise and its target is past the fold.
        let mut del = fresh("* one\nbody\nmore\n* two");
        folded(&mut del, 0, 15);
        feed(&mut del, &keys("dj"));
        assert_eq!(del.buffer.text.to_string(), "");
    }

    /// The backstop behind the motion: whatever puts point inside a fold — a
    /// fold made around it, an undo, a Lisp `goto-char` — it comes back out onto
    /// the line that is still drawn.
    #[test]
    fn a_fold_made_around_point_puts_it_on_the_head_line() {
        let mut ed = fresh("* one\nbody\nmore\n* two");
        feed(&mut ed, &keys("jj"));
        assert_eq!(ed.buffer.cursor_line_col(), (2, 0));
        folded(&mut ed, 0, 15);
        // `apply` ran `clamp_cursor`, which is where the escape lives.
        assert_eq!(ed.buffer.cursor_line_col(), (0, 0));
    }

    // --- visual lines -----------------------------------------------------

    /// A window ten cells wide with wrapping on, which is all it takes for core
    /// to start counting rows instead of lines: `wrap_cols` is what the renderer
    /// parks there every frame.
    fn wrapped(text: &str, cols: usize) -> Editor {
        let mut ed = fresh(text);
        ed.settings.line_overflow = LineOverflow::Wrap;
        ed.wrap_cols = cols;
        ed
    }

    /// The config's `evil-next-visual-line`: `j` in the middle of a long line
    /// lands on the row below, not on the next paragraph.
    #[test]
    fn j_and_k_move_by_visual_line_when_the_window_wraps() {
        // Line 0 is 25 cells, so in a 10-cell window it is three rows:
        // "0123456789" | "abcdefghij" | "ABCDE". Line 1 is one row.
        let mut ed = wrapped("0123456789abcdefghijABCDE\nzz", 10);
        feed(&mut ed, &keys("lll"));
        assert_eq!(ed.buffer.cursor_line_col(), (0, 3));
        // Same buffer line, one row down: character 13, not the line below.
        feed(&mut ed, &keys("j"));
        assert_eq!(ed.buffer.cursor_line_col(), (0, 13));
        feed(&mut ed, &keys("j"));
        assert_eq!(ed.buffer.cursor_line_col(), (0, 23));
        // Off the end of the last row and onto the next buffer line, which is
        // short — so the column clamps to it, and then the Normal-mode clamp
        // pulls it onto the last character rather than past it.
        feed(&mut ed, &keys("j"));
        assert_eq!(ed.buffer.cursor_line_col(), (1, 1));
        // Nothing below: `j` stops rather than wrapping or panicking.
        feed(&mut ed, &keys("jjj"));
        assert_eq!(ed.buffer.cursor_line_col(), (1, 1));
    }

    /// The whole point of a held column, in the unit visual motion works in: a
    /// run down and back up through rows of different lengths must end on the
    /// character it started on.
    #[test]
    fn a_run_down_and_back_up_through_a_wrapped_line_returns_to_the_same_character() {
        // Row layout in a 10-cell window:
        //   line 0: "0123456789" | "abcd"   (a short second row)
        //   line 1: "xy"                    (a short line)
        //   line 2: "0123456789" | "ABCDEFG"
        let mut ed = wrapped("0123456789abcd\nxy\n0123456789ABCDEFG", 10);
        feed(&mut ed, &keys("llllll")); // cell column 6 of row 0
        let start = ed.buffer.cursor;
        assert_eq!(ed.buffer.cursor_line_col(), (0, 6));

        // Down through a short row and a short line, both of which clamp...
        feed(&mut ed, &keys("j"));
        assert_eq!(ed.buffer.cursor_line_col(), (0, 13)); // last cell of "abcd"
        feed(&mut ed, &keys("j"));
        assert_eq!(ed.buffer.cursor_line_col(), (1, 1)); // last cell of "xy"
        feed(&mut ed, &keys("j"));
        assert_eq!(ed.buffer.cursor_line_col(), (2, 6)); // column 6 again
        feed(&mut ed, &keys("j"));
        assert_eq!(ed.buffer.cursor_line_col(), (2, 16));

        // ...and back up to exactly where it started.
        feed(&mut ed, &keys("kkkk"));
        assert_eq!(ed.buffer.cursor, start);

        // A horizontal key ends the run, so the next `j` holds the new column.
        feed(&mut ed, &keys("hj"));
        assert_eq!(ed.buffer.cursor_line_col(), (0, 13));
    }

    /// Wide characters and visual lines are the same arithmetic, so a run must
    /// hold a *cell* column: two CJK characters are one row of four cells, and
    /// holding the character column would drift left one column per row.
    #[test]
    fn a_visual_run_holds_cells_rather_than_characters() {
        // Four cells wide: "日本" is one full row, "abcd" is another.
        let mut ed = wrapped("日本語漢\nabcdefgh", 4);
        feed(&mut ed, &keys("l")); // on 本, cell column 2
        assert_eq!(ed.buffer.cursor_line_col(), (0, 1));
        feed(&mut ed, &keys("j"));
        assert_eq!(ed.buffer.cursor_line_col(), (0, 3), "row 2 of the same line");
        feed(&mut ed, &keys("j"));
        // Cell column 2 of "abcd" is 'c' — not character 2 of the CJK line.
        assert_eq!(ed.buffer.cursor_line_col(), (1, 2));
        feed(&mut ed, &keys("kk"));
        assert_eq!(ed.buffer.cursor_line_col(), (0, 1));
    }

    /// Truncation, and a window nothing has drawn yet, both mean one row per
    /// line — so every existing motion is untouched, and so is every operator.
    #[test]
    fn buffer_lines_are_still_the_rule_without_wrapping() {
        let mut off = fresh("0123456789abcdefghij\nzz");
        off.wrap_cols = 10; // drawn, but truncating
        feed(&mut off, &keys("lllj"));
        assert_eq!(off.buffer.cursor_line_col(), (1, 1));

        let mut undrawn = fresh("0123456789abcdefghij\nzz");
        undrawn.settings.line_overflow = LineOverflow::Wrap; // never rendered
        feed(&mut undrawn, &keys("lllj"));
        assert_eq!(undrawn.buffer.cursor_line_col(), (1, 1));

        // And an operator keeps buffer-line semantics even where `j` alone
        // would move one row: `dj` is two whole lines, as in vim.
        let mut del = wrapped("0123456789abcd\nxy\nzz", 10);
        feed(&mut del, &keys("dj"));
        assert_eq!(del.buffer.text.to_string(), "zz");
    }

    #[test]
    fn visual_block_selects_a_rectangle_not_a_span() {
        let mut ed = fresh("abcdef\nghijkl\nmnopqr");
        feed(&mut ed, &keys("l")); // column 1
        feed(&mut ed, &[Key::Ctrl('v')]);
        assert_eq!(ed.mode, Mode::VisualBlock);
        feed(&mut ed, &keys("ljj")); // columns 1..=2, lines 0..=2

        let ranges = ed.selection_ranges();
        assert_eq!(ranges.len(), 3, "one range per line");
        let text: Vec<String> = ranges
            .iter()
            .map(|&(s, e)| ed.buffer.slice_string(s, e))
            .collect();
        assert_eq!(text, ["bc", "hi", "no"]);
    }

    #[test]
    fn visual_block_delete_removes_the_column() {
        let mut ed = fresh("abcdef\nghijkl\nmnopqr");
        feed(&mut ed, &keys("l"));
        feed(&mut ed, &[Key::Ctrl('v')]);
        feed(&mut ed, &keys("ljjd"));
        assert_eq!(ed.buffer.text.to_string(), "adef\ngjkl\nmpqr");
        assert_eq!(ed.mode, Mode::Normal);
    }

    #[test]
    fn visual_block_yank_keeps_the_lines_separate() {
        let mut ed = fresh("abcdef\nghijkl");
        feed(&mut ed, &[Key::Ctrl('v')]);
        feed(&mut ed, &keys("ljy"));
        assert_eq!(ed.buffer.text.to_string(), "abcdef\nghijkl", "yank edits nothing");
        // paste it back at the end to prove what was captured
        feed(&mut ed, &keys("G$p"));
        assert!(ed.buffer.text.to_string().contains("ab\ngh"));
    }

    #[test]
    fn a_block_skips_lines_too_short_to_reach_it() {
        // vim does not select a line that ends before the block's left edge.
        let mut ed = fresh("aaaaa\nbb\nccccc");
        feed(&mut ed, &keys("lll")); // column 3
        feed(&mut ed, &[Key::Ctrl('v')]);
        feed(&mut ed, &keys("jj"));
        let text: Vec<String> = ed
            .selection_ranges()
            .iter()
            .map(|&(s, e)| ed.buffer.slice_string(s, e))
            .collect();
        assert_eq!(text, ["a", "c"], "the short line contributes nothing");
    }

    #[test]
    fn switching_visual_modes_keeps_the_anchor() {
        let mut ed = fresh("abcdef\nghijkl");
        feed(&mut ed, &keys("v"));
        feed(&mut ed, &keys("ll"));
        feed(&mut ed, &[Key::Ctrl('v')]); // reshape, do not restart
        assert_eq!(ed.mode, Mode::VisualBlock);
        let text: Vec<String> = ed
            .selection_ranges()
            .iter()
            .map(|&(s, e)| ed.buffer.slice_string(s, e))
            .collect();
        assert_eq!(text, ["abc"]);
    }

    #[test]
    fn generated_buffers_refuse_edits() {
        for kind in [crate::BufferKind::Dashboard, crate::BufferKind::Magit] {
            let mut ed = fresh("");
            ed.show_special(kind, "Head:     main\nM src/lib.rs\n");
            let before = ed.buffer.text.to_string();

            // every route into the text is refused, including entering Insert
            feed(&mut ed, &[Key::Char('i'), Key::Char('X')]);
            feed(&mut ed, &keys("xdd"));
            feed(&mut ed, &keys("p"));
            ed.apply(EditorCommand::InsertText("nope".into()));
            ed.apply(EditorCommand::Undo);

            assert_eq!(ed.buffer.text.to_string(), before, "{kind:?} was edited");
            assert_ne!(ed.mode, Mode::Insert, "{kind:?} let us into Insert");
            assert!(ed.status.contains("read-only"), "{kind:?}: {}", ed.status);
            assert!(!ed.buffer.modified);
        }
    }

    #[test]
    fn ordinary_buffers_are_still_editable() {
        let mut ed = fresh("hello");
        feed(&mut ed, &[Key::Char('i'), Key::Char('X')]);
        assert_eq!(ed.buffer.text.to_string(), "Xhello");
    }

    #[test]
    fn c_c_finishes_a_commit_message_but_still_evaluates_elsewhere() {
        let mut ed = fresh("(+ 1 2)");
        // ordinary buffer: C-c evaluates
        assert!(matches!(
            ed.run_action("eval-dwim").as_slice(),
            [EditorCommand::CallLisp(_)]
        ));

        // commit message: the same key finishes the commit instead
        ed.show_special(crate::BufferKind::CommitMessage, "\n# comment\n");
        assert_eq!(
            ed.run_action("eval-dwim"),
            vec![EditorCommand::Git("commit-finish".into())]
        );
        assert_eq!(ed.buffer.name(), "COMMIT_EDITMSG");
        // and it is an ordinary editable buffer, not a mode of its own
        assert_eq!(ed.mode, Mode::Normal);
        feed(&mut ed, &[Key::Char('i'), Key::Char('h'), Key::Char('i')]);
        assert!(ed.buffer.text.to_string().starts_with("hi"));
    }

    #[test]
    fn focusing_another_frame_does_not_swap_their_buffers() {
        // The live buffer belongs to the focused window, so moving focus
        // between frames has to park it and adopt the new frame's window. A
        // bare `focus_frame = i` drags the buffer along, and clicking between
        // two frames swaps what they were showing.
        let mut ed = Editor::new();
        ed.load("FIRST\n", Some("/tmp/a.rs".into()), None);
        assert_eq!(ed.buffer.name(), "a.rs");

        ed.apply(EditorCommand::NewFrame); // frame 1, on the dashboard
        assert_eq!(ed.buffer.name(), "*dashboard*");

        ed.apply(EditorCommand::FocusFrame(0));
        assert_eq!(ed.buffer.name(), "a.rs", "frame 0 kept its file");
        assert_eq!(ed.buffer.text.to_string(), "FIRST\n");

        ed.apply(EditorCommand::FocusFrame(1));
        assert_eq!(ed.buffer.name(), "*dashboard*", "frame 1 kept its dashboard");

        // out of range is ignored rather than panicking
        ed.apply(EditorCommand::FocusFrame(99));
        assert_eq!(ed.focus_frame, 1);
    }

    #[test]
    fn consult_line_previews_and_jumps() {
        let mut ed = fresh("alpha\nbeta\ngamma\ndelta");
        let start = ed.buffer.cursor;
        ed.run_action("search-line");
        assert_eq!(ed.prompt.as_ref().map(|p| p.kind), Some(PromptKind::Line));

        // narrowing moves the cursor to the candidate — consult's preview
        feed(&mut ed, &keys("gam"));
        assert_eq!(ed.buffer.cursor_line_col().0, 2);

        // Enter lands there for good
        for cmd in ed.handle_key(Key::Enter) {
            ed.apply(cmd);
        }
        assert!(ed.prompt.is_none());
        assert_eq!(ed.buffer.cursor_line_col().0, 2);

        // and cancelling puts the cursor back where it started
        ed.run_action("search-line");
        feed(&mut ed, &keys("delt"));
        assert_eq!(ed.buffer.cursor_line_col().0, 3);
        let origin = ed.prompt.as_ref().unwrap().origin.unwrap();
        for cmd in ed.handle_key(Key::Esc) {
            ed.apply(cmd);
        }
        assert_eq!(ed.buffer.cursor, origin);
        assert_ne!(ed.buffer.cursor, start.max(1).min(0)); // sanity: it moved and came back
    }

    #[test]
    fn ace_window_switches_with_two_and_labels_with_more() {
        let mut ed = fresh("x");
        // one window: nothing to choose
        assert_eq!(
            ed.run_action("ace-window"),
            vec![EditorCommand::FocusNextWindow]
        );
        assert!(ed.ace.is_none());

        ed.apply(EditorCommand::SplitWindow(crate::frame::Split::Columns));
        assert_eq!(
            ed.run_action("ace-window"),
            vec![EditorCommand::FocusNextWindow],
            "two windows still just toggles"
        );

        // three: labels come up and the next key picks one
        ed.apply(EditorCommand::SplitWindow(crate::frame::Split::Rows));
        ed.run_action("ace-window");
        let labels = ed.ace.clone().expect("labels are up");
        assert_eq!(labels.len(), 3);
        let (key, want) = labels[2];
        assert_eq!(
            ed.handle_key(Key::Char(key)),
            vec![EditorCommand::FocusWindow(want)]
        );
        assert!(ed.ace.is_none(), "labels are consumed");

        // a key that is not a label cancels rather than doing something else
        ed.run_action("ace-window");
        let out = ed.handle_key(Key::Char('Z'));
        assert!(!out.iter().any(|c| matches!(c, EditorCommand::FocusWindow(_))));
        assert!(ed.ace.is_none());
        assert_eq!(ed.buffer.text.to_string(), "x", "and edits nothing");
    }

    #[test]
    fn a_buffer_gets_a_major_mode_from_its_file_and_fires_the_hook() {
        let mut ed = fresh("");
        assert_eq!(ed.buffer.major_mode, crate::FUNDAMENTAL);

        ed.pending_hooks.clear();
        ed.load("* Heading\n", Some("/tmp/notes.org".into()), Some("org".into()));
        assert_eq!(ed.buffer.major_mode, "org-mode");
        assert!(ed.pending_hooks.contains(&"org-mode-hook".to_string()));

        // and a file with no known language falls back rather than guessing
        ed.load("plain\n", Some("/tmp/x.unknown".into()), None);
        assert_eq!(ed.buffer.major_mode, crate::FUNDAMENTAL);
    }

    #[test]
    fn a_major_mode_binding_only_applies_in_that_mode() {
        let mut ed = fresh("hello world");
        ed.apply(EditorCommand::BindKey {
            mode: "org-mode".into(),
            keys: "<tab>".into(),
            command: "org-cycle".into(),
        });
        // not an org buffer: Tab is not hijacked
        assert!(!ed
            .handle_key(Key::Tab)
            .contains(&EditorCommand::CallLisp("(org-cycle)".into())));

        ed.load("* h\n", Some("/tmp/a.org".into()), Some("org".into()));
        assert_eq!(
            ed.handle_key(Key::Tab),
            vec![EditorCommand::CallLisp("(org-cycle)".into())]
        );
    }

    #[test]
    fn a_minor_mode_overrides_the_major_one() {
        let mut ed = fresh("x");
        ed.apply(EditorCommand::SetMajorMode("org-mode".into()));
        for (mode, cmd) in [("org-mode", "org-thing"), ("my-minor", "minor-thing")] {
            ed.apply(EditorCommand::BindKey {
                mode: mode.into(),
                keys: "g z".into(),
                command: cmd.into(),
            });
        }
        let press = |ed: &mut Editor| {
            ed.handle_key(Key::Char('g'));
            ed.handle_key(Key::Char('z'))
        };
        assert_eq!(
            press(&mut ed),
            vec![EditorCommand::CallLisp("(org-thing)".into())]
        );

        ed.apply(EditorCommand::SetMinorMode("my-minor".into(), true));
        assert_eq!(
            press(&mut ed),
            vec![EditorCommand::CallLisp("(minor-thing)".into())]
        );
        // ...and switching it off hands the key back
        ed.apply(EditorCommand::SetMinorMode("my-minor".into(), false));
        assert_eq!(
            press(&mut ed),
            vec![EditorCommand::CallLisp("(org-thing)".into())]
        );
    }

    #[test]
    fn m_x_org_mode_sets_the_major_mode() {
        let mut ed = fresh("x");
        assert_eq!(
            ed.run_action("org-mode"),
            vec![EditorCommand::SetMajorMode("org-mode".into())]
        );
        // but a Lisp command whose name ends in -mode still reaches Lisp
        ed.commands = vec!["my-cute-mode".into()];
        assert_eq!(
            ed.run_action("my-cute-mode"),
            vec![EditorCommand::CallLisp("(my-cute-mode)".into())]
        );
    }

    #[test]
    fn line_number_and_overflow_settings_are_configurable() {
        let mut ed = fresh("");
        ed.apply(EditorCommand::SetLineOverflow("wrap".into()));
        assert_eq!(ed.settings.line_overflow, crate::LineOverflow::Wrap);
        ed.apply(EditorCommand::SetLineOverflow("truncate".into()));
        assert_eq!(ed.settings.line_overflow, crate::LineOverflow::Truncate);
        ed.apply(EditorCommand::SetLineOverflow("sideways".into()));
        assert!(ed.status.contains("unknown line overflow"));

        assert!(!ed.settings.relative_line_numbers);
        ed.apply(EditorCommand::SetRelativeLineNumbers(true));
        assert!(ed.settings.relative_line_numbers);
    }

    #[test]
    fn magit_verbs_become_git_commands_and_keep_motions() {
        let mut ed = fresh("");
        assert_eq!(
            ed.run_action("magit-status"),
            vec![EditorCommand::Git("status".into())]
        );
        assert_eq!(
            ed.run_action("magit-commit-finish"),
            vec![EditorCommand::Git("commit-finish".into())]
        );

        // The status buffer gets its own mode, so `s`/`u`/`c` can be staging
        // rather than substitute/undo/change...
        ed.show_special(crate::BufferKind::Magit, "line one\nline two\nline three");
        assert_eq!(ed.mode, Mode::Magit);
        assert_eq!(ed.buffer.name(), "*magit*");
        ed.apply(EditorCommand::BindKey {
            mode: "magit".into(),
            keys: "s".into(),
            command: "magit-stage".into(),
        });
        assert_eq!(
            ed.handle_key(Key::Char('s')),
            vec![EditorCommand::Git("stage".into())]
        );

        // ...while the vim motions still work, because the keymap is consulted
        // before the built-in grammar rather than replacing it.
        feed(&mut ed, &keys("j"));
        assert_eq!(ed.buffer.cursor_line_col().0, 1);
        feed(&mut ed, &keys("G"));
        assert_eq!(ed.buffer.cursor_line_col().0, 2);
    }

    #[test]
    fn refreshing_the_status_buffer_reuses_it_and_holds_the_line() {
        let mut ed = fresh("");
        ed.show_special(crate::BufferKind::Magit, "a\nb\nc\n");
        feed(&mut ed, &keys("jj"));
        assert_eq!(ed.buffer.cursor_line_col().0, 2);
        let id = ed.buffer.id;

        // staging something re-renders; the cursor must not jump to the top
        ed.show_special(crate::BufferKind::Magit, "a\nb\nc\nd\n");
        assert_eq!(ed.buffer.id, id, "one *magit*, not one per refresh");
        assert_eq!(ed.buffer.cursor_line_col().0, 2);
        assert_eq!(
            ed.buffer_names().iter().filter(|n| *n == "*magit*").count(),
            1
        );
    }

    #[test]
    fn dashboard_is_a_buffer_you_can_switch_to() {
        let mut ed = Editor::new();
        assert!(ed.buffer_names().contains(&"*dashboard*".to_string()));
        assert!(ed.buffer_names().contains(&"*scratch*".to_string()));

        // leaving it and coming back via the switcher restores dashboard mode
        ed.load("code\n", Some("/tmp/a.rs".into()), None);
        assert_eq!(ed.mode, Mode::Normal);
        let at = ed
            .buffer_names()
            .iter()
            .position(|n| n == "*dashboard*")
            .expect("dashboard in the buffer list");
        ed.apply(EditorCommand::SwitchBuffer(at));
        assert_eq!(ed.mode, Mode::Dashboard);
        assert_eq!(ed.buffer.name(), "*dashboard*");
    }

    #[test]
    fn opening_a_file_from_the_dashboard_keeps_the_dashboard() {
        // The dashboard buffer is empty and pathless, so a naive "is this a
        // throwaway scratch?" check would recycle it and lose it.
        let mut ed = Editor::new();
        ed.load("code\n", Some("/tmp/a.rs".into()), None);
        assert!(ed.buffer_names().contains(&"*dashboard*".to_string()));
    }

    #[test]
    fn ctrl_enter_splits_side_by_side_and_ctrl_meta_enter_stacks() {
        let mut ed = fresh("hello");
        for cmd in ed.handle_key(Key::CtrlEnter) {
            ed.apply(cmd);
        }
        let area = crate::Rect::new(0, 0, 800, 600);
        let panes = ed.frame().panes(area);
        assert_eq!(panes.len(), 2);
        assert!(panes[0].rect.x < panes[1].rect.x, "side by side");
        assert_eq!(panes[0].rect.y, panes[1].rect.y);

        for cmd in ed.handle_key(Key::CtrlMetaEnter) {
            ed.apply(cmd);
        }
        let panes = ed.frame().panes(area);
        assert_eq!(panes.len(), 3);
    }

    #[test]
    fn a_split_shows_the_same_buffer_and_windows_scroll_independently() {
        let mut ed = fresh("a\nb\nc\nd\ne\nf\ng\nh");
        ed.viewport_lines = 3;
        let first = ed.frame().current;
        ed.apply(EditorCommand::SplitWindow(crate::frame::Split::Columns));
        let second = ed.frame().current;
        assert_ne!(first, second);
        assert_eq!(
            ed.frame().window(first).unwrap().buffer,
            ed.frame().window(second).unwrap().buffer,
        );

        // scroll in the focused window, then look at the other one
        ed.apply(EditorCommand::ScrollLines(4));
        let moved = ed.scroll;
        assert!(moved > 0);
        ed.apply(EditorCommand::FocusNextWindow);
        assert_eq!(ed.frame().current, first);
        assert_eq!(ed.scroll, 0, "the other window kept its own scroll");
    }

    #[test]
    fn closing_the_last_window_is_refused() {
        let mut ed = fresh("x");
        ed.apply(EditorCommand::CloseWindow);
        assert_eq!(ed.frame().windows.len(), 1);
        assert!(ed.status.contains("cannot close"));

        ed.apply(EditorCommand::SplitWindow(crate::frame::Split::Rows));
        ed.apply(EditorCommand::CloseWindow);
        assert_eq!(ed.frame().windows.len(), 1);
    }

    #[test]
    fn new_frame_opens_on_the_dashboard() {
        let mut ed = Editor::new();
        ed.load("code\n", Some("/tmp/a.rs".into()), None);
        assert_eq!(ed.frames.len(), 1);

        let out = bind_and_press(&mut ed, "SPC n f", "new-frame");
        for cmd in out {
            ed.apply(cmd);
        }
        assert_eq!(ed.frames.len(), 2);
        assert_eq!(ed.focus_frame, 1);
        assert_eq!(ed.mode, Mode::Dashboard);
        assert_eq!(ed.buffer.name(), "*dashboard*");

        // and the first frame still has the file
        assert!(ed.buffer_names().contains(&"a.rs".to_string()));
    }

    fn lisp_of(cmds: &[EditorCommand]) -> Option<String> {
        cmds.iter().find_map(|c| match c {
            EditorCommand::CallLisp(s) => Some(s.clone()),
            _ => None,
        })
    }

    #[test]
    fn eval_last_sexp_sends_the_form_under_point() {
        let mut ed = fresh("(message \"a\")\n(+ 1 2)\n");
        feed(&mut ed, &keys("G$"));
        let out = ed.run_action("eval-last-sexp");
        assert_eq!(lisp_of(&out).as_deref(), Some("(+ 1 2)"));

        // from the first line it picks the first form, not the last
        feed(&mut ed, &keys("gg$"));
        let out = ed.run_action("eval-last-sexp");
        assert_eq!(lisp_of(&out).as_deref(), Some("(message \"a\")"));
    }

    #[test]
    fn the_sexp_scan_ignores_parens_in_strings_and_comments() {
        let b = Buffer::from_str("(a \")\") ; )))\n");
        assert_eq!(b.last_top_level_form(b.len_chars()), Some((0, 7)));
        assert_eq!(b.slice_string(0, 7), "(a \")\")");

        // an unclosed form is not offered
        let open = Buffer::from_str("(a (b ");
        assert_eq!(open.last_top_level_form(open.len_chars()), None);

        // escaped quote inside a string does not end it
        let esc = Buffer::from_str("(f \"x\\\"(\") ");
        assert!(esc.last_top_level_form(esc.len_chars()).is_some());
    }

    #[test]
    fn eval_buffer_and_region_send_the_right_text() {
        let mut ed = fresh("(one)\n(two)\n");
        let out = ed.run_action("eval-buffer");
        assert_eq!(lisp_of(&out).as_deref(), Some("(one)\n(two)\n"));

        // visual selection wins
        feed(&mut ed, &keys("ggv$"));
        let out = ed.run_action("eval-region");
        assert_eq!(lisp_of(&out).as_deref(), Some("(one)"));

        let mut empty = fresh("   \n");
        assert!(lisp_of(&empty.run_action("eval-buffer")).is_none());
    }

    #[test]
    fn eval_dwim_picks_selection_then_form_then_buffer() {
        // no parens anywhere: falls back to the whole buffer
        let mut plain = fresh("just text\n");
        assert_eq!(
            lisp_of(&plain.run_action("eval-dwim")).as_deref(),
            Some("just text\n")
        );

        // a complete form under point wins over the buffer
        let mut forms = fresh("(a)\n(b)\n");
        feed(&mut forms, &keys("G$"));
        assert_eq!(lisp_of(&forms.run_action("eval-dwim")).as_deref(), Some("(b)"));

        // and a selection wins over everything
        feed(&mut forms, &keys("ggv$"));
        assert_eq!(lisp_of(&forms.run_action("eval-dwim")).as_deref(), Some("(a)"));
    }

    #[test]
    fn switch_buffer_lists_and_switches() {
        let mut ed = fresh("");
        ed.load("FIRST\n", Some("/tmp/a.rs".into()), None);
        ed.load("SECOND\n", Some("/tmp/b.rs".into()), None);
        // `*scratch*` is a real buffer and stays in the list.
        assert_eq!(ed.buffer_names()[..2], ["b.rs", "a.rs"]);
        assert!(ed.buffer_names().contains(&"*scratch*".to_string()));

        let out = bind_and_press(&mut ed, "SPC j j", "switch-buffer");
        assert!(out.is_empty());
        assert_eq!(ed.prompt.as_ref().map(|p| p.kind), Some(PromptKind::Buffer));

        feed(&mut ed, &keys("a.r"));
        for cmd in ed.handle_key(Key::Enter) {
            ed.apply(cmd);
        }
        assert_eq!(ed.buffer.text.to_string(), "FIRST\n");
        assert_eq!(ed.buffer_names()[..2], ["a.rs", "b.rs"]);
    }

    #[test]
    fn reopening_a_file_switches_instead_of_duplicating() {
        let mut ed = fresh("");
        ed.load("FIRST\n", Some("/tmp/a.rs".into()), None);
        ed.load("SECOND\n", Some("/tmp/b.rs".into()), None);
        ed.load("FIRST\n", Some("/tmp/a.rs".into()), None);
        assert_eq!(ed.buffer_names()[..2], ["a.rs", "b.rs"]);
        assert_eq!(ed.buffer.text.to_string(), "FIRST\n");
    }

    #[test]
    fn undo_history_travels_with_its_buffer() {
        let mut ed = fresh("");
        ed.load("aaa", Some("/tmp/a.rs".into()), None);
        feed(&mut ed, &keys("x")); // "aa"
        ed.load("bbb", Some("/tmp/b.rs".into()), None);
        feed(&mut ed, &keys("x")); // "bb"

        // undo in b affects only b
        feed(&mut ed, &keys("u"));
        assert_eq!(ed.buffer.text.to_string(), "bbb");

        ed.switch_buffer(1);
        assert_eq!(ed.buffer.text.to_string(), "aa");
        feed(&mut ed, &keys("u"));
        assert_eq!(ed.buffer.text.to_string(), "aaa");
    }

    #[test]
    fn completion_style_is_configurable() {
        let mut ed = fresh("");
        ed.apply(EditorCommand::SetCompletionStyle("telescope".into()));
        assert_eq!(ed.settings.completion_style, crate::CompletionStyle::Center);
        ed.apply(EditorCommand::SetCompletionStyle("consult".into()));
        assert_eq!(ed.settings.completion_style, crate::CompletionStyle::Bottom);
    }

    #[test]
    fn ex_open_and_quit() {
        let mut ed = fresh("");
        feed(&mut ed, &[Key::Char(':')]);
        feed(&mut ed, &keys("q"));
        feed(&mut ed, &[Key::Enter]);
        assert!(ed.should_quit);
    }

    // --- registers, macros, marks ----------------------------------------

    #[test]
    fn a_named_register_keeps_its_own_text_and_uppercase_appends() {
        let mut ed = fresh("aaa\nbbb\nccc");
        feed(&mut ed, &keys("\"ayy")); // register a: "aaa\n"
        feed(&mut ed, &keys("j\"Ayy")); // uppercase appends: "aaa\nbbb\n"
        assert_eq!(ed.register, "bbb\n", "the unnamed one gets every yank too");

        feed(&mut ed, &keys("G\"ap"));
        assert_eq!(ed.buffer.text.to_string(), "aaa\nbbb\nccc\naaa\nbbb");
        // ...and pasting a *named* register does not disturb the unnamed one,
        // so the plain `p` that follows still pastes what was yanked last.
        assert_eq!(ed.register, "bbb\n");
        feed(&mut ed, &keys("p"));
        assert_eq!(ed.buffer.text.to_string(), "aaa\nbbb\nccc\naaa\nbbb\nbbb");
    }

    #[test]
    fn a_named_register_survives_a_yank_into_another_one() {
        let mut ed = fresh("aaa\nbbb");
        feed(&mut ed, &keys("\"ayy")); // a: "aaa\n"
        feed(&mut ed, &keys("j\"byy")); // b: "bbb"
        feed(&mut ed, &keys("\"aP"));
        assert_eq!(ed.buffer.text.to_string(), "aaa\naaa\nbbb");
        // an empty register pastes nothing rather than the last yank
        let before = ed.buffer.text.to_string();
        feed(&mut ed, &keys("\"zp"));
        assert_eq!(ed.buffer.text.to_string(), before);
        assert!(ed.status.contains("register z is empty"));
    }

    #[test]
    fn a_macro_records_keys_and_replays_the_decisions() {
        let mut ed = fresh("one two\nthree four\nfive six");
        // `qq dw j0 q` — kill the first word of a line and drop to the next.
        feed(&mut ed, &keys("qqdwj0q"));
        assert_eq!(ed.buffer.text.to_string(), "two\nthree four\nfive six");

        // The replay deletes *this* line's first word, not the one recorded —
        // which is the difference between recording keys and recording commands.
        feed(&mut ed, &keys("@q"));
        assert_eq!(ed.buffer.text.to_string(), "two\nfour\nfive six");
        // `@@` repeats it without naming the register again
        feed(&mut ed, &keys("@@"));
        assert_eq!(ed.buffer.text.to_string(), "two\nfour\nsix");
    }

    /// The recorder sits ahead of the prompt branch, so a macro can contain a
    /// whole ex command. `q` typed *into* a prompt is a letter, not a stop key.
    #[test]
    fn a_macro_can_contain_an_ex_command() {
        let mut ed = fresh("foo\nfoo\nfoo\n");
        feed(&mut ed, &keys("qs"));
        feed(&mut ed, &[Key::Char(':')]);
        feed(&mut ed, &keys("s/foo/bar/"));
        feed(&mut ed, &[Key::Enter]);
        feed(&mut ed, &keys("jq"));
        assert_eq!(ed.buffer.text.to_string(), "bar\nfoo\nfoo\n");
        assert!(ed.prompt.is_none());

        feed(&mut ed, &keys("@s"));
        assert_eq!(ed.buffer.text.to_string(), "bar\nbar\nfoo\n");
    }

    #[test]
    fn a_counted_replay_runs_the_macro_that_many_times() {
        let mut ed = fresh("abcdef");
        feed(&mut ed, &keys("qzxq")); // record a single `x`
        assert_eq!(ed.buffer.text.to_string(), "bcdef");
        feed(&mut ed, &keys("3@z"));
        assert_eq!(ed.buffer.text.to_string(), "ef");
    }

    /// The obvious way to write a macro that never ends, and it would be a
    /// *stack* overflow rather than a hang — `@` re-enters `handle_key`.
    #[test]
    fn a_macro_that_replays_itself_stops_instead_of_overflowing() {
        let text = "x".repeat(60);
        let mut ed = fresh(&text);
        feed(&mut ed, &keys("qax@aq")); // record: delete a char, then run @a
        feed(&mut ed, &keys("@a"));

        assert_eq!(
            ed.buffer.len_chars(),
            // one delete per level, and the deepest level refuses instead of
            // recursing. The first `x` is the one typed while recording.
            text.len() - super::MACRO_DEPTH - 1,
        );
        assert!(
            ed.messages.iter().any(|m| m.contains("nested too deeply")),
            "and it says why it stopped: {:?}",
            ed.messages
        );
    }

    /// The point of building marks on markers rather than on offsets: `ma`
    /// still names its character after the text above it changes length.
    #[test]
    fn a_mark_survives_an_edit_above_it() {
        let mut ed = fresh("alpha\nbeta\ngamma");
        feed(&mut ed, &keys("jjma"));
        assert_eq!(ed.buffer.cursor, 11);

        feed(&mut ed, &keys("ggdd")); // "alpha\n" goes, six characters above it
        feed(&mut ed, &[Key::Char('`'), Key::Char('a')]);
        assert_eq!(ed.buffer.cursor, 5);
        assert_eq!(ed.buffer.slice_string(5, 10), "gamma", "the same character");

        // `'a` is the line, not the position — vim's distinction between the
        // two keys, and the reason both exist.
        feed(&mut ed, &keys("$"));
        feed(&mut ed, &[Key::Char('\''), Key::Char('a')]);
        assert_eq!(ed.buffer.cursor_line_col(), (1, 0));

        // an unset mark says so rather than jumping somewhere arbitrary
        feed(&mut ed, &[Key::Char('`'), Key::Char('z')]);
        assert!(ed.status.contains("mark not set"));
    }

    #[test]
    fn a_mark_is_a_motion_an_operator_can_take() {
        let mut ed = fresh("aaa\nbbb\nccc\nddd");
        feed(&mut ed, &keys("jjma")); // mark line 2
        feed(&mut ed, &keys("gg"));
        feed(&mut ed, &[Key::Char('d'), Key::Char('\''), Key::Char('a')]);
        assert_eq!(ed.buffer.text.to_string(), "ddd", "`d'a` is linewise");
    }

    // --- regex, :s and incremental search --------------------------------

    #[test]
    fn search_is_a_regex_in_vims_dialect() {
        let mut ed = fresh("foo123\nbar(x)\nbaz");
        feed(&mut ed, &[Key::Char('/')]);
        feed(&mut ed, &keys("[0-9]\\+"));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.cursor, 3);

        // `(` is literal at vim's magic level, so this finds text and not a group
        feed(&mut ed, &[Key::Char('/')]);
        feed(&mut ed, &keys("(x)"));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.cursor_line_col(), (1, 3));

        // a pattern the engine cannot compile is refused, not silently literal
        feed(&mut ed, &[Key::Char('/')]);
        feed(&mut ed, &keys("[unclosed"));
        feed(&mut ed, &[Key::Enter]);
        assert!(ed.status.contains("bad pattern"), "{}", ed.status);
    }

    #[test]
    fn the_vim_dialect_swaps_exactly_the_characters_that_disagree() {
        use super::{vim_regex, vim_replacement};
        // unescaped groupers are literal text in vim...
        assert_eq!(vim_regex("foo(1)").0, "foo\\(1\\)");
        // ...and the escaped ones are the operators.
        assert_eq!(vim_regex("\\(a\\|b\\)\\+").0, "(a|b)+");
        assert_eq!(vim_regex("\\<word\\>").0, "\\bword\\b");
        // everything both dialects already agree about passes through
        assert_eq!(vim_regex("^a.*\\.rs$").0, "^a.*\\.rs$");
        assert!(vim_regex("\\cFoo").1, "\\c asks for folding");

        assert_eq!(vim_replacement("[&]"), "[${0}]");
        assert_eq!(vim_replacement("\\2-\\1"), "${2}-${1}");
        assert_eq!(vim_replacement("$5"), "$$5", "a literal dollar stays one");
    }

    /// One `u` for the whole `:%s`, not one per match. The reason the
    /// substitution is a single delete-and-insert over the affected lines.
    #[test]
    fn a_substitution_is_one_undo_step_however_many_matches() {
        let mut ed = fresh("foo foo\nfoo bar\nbaz foo\n");
        feed(&mut ed, &[Key::Char(':')]);
        feed(&mut ed, &keys("%s/foo/X/g"));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.text.to_string(), "X X\nX bar\nbaz X\n");

        feed(&mut ed, &keys("u"));
        assert_eq!(ed.buffer.text.to_string(), "foo foo\nfoo bar\nbaz foo\n");
        feed(&mut ed, &keys("u"));
        assert!(
            ed.status.contains("already at oldest"),
            "four matches, one undo step"
        );
    }

    #[test]
    fn substitute_ranges_and_flags() {
        // no range is the current line, and no `g` is the first match on it
        let mut ed = fresh("aa aa\naa aa\n");
        feed(&mut ed, &[Key::Char(':')]);
        feed(&mut ed, &keys("s/aa/Z/"));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.text.to_string(), "Z aa\naa aa\n");

        // `i` folds case, and a capture comes back as `\1`
        let mut ed = fresh("Hello World\n");
        feed(&mut ed, &[Key::Char(':')]);
        feed(&mut ed, &keys("s/\\(hello\\) \\(world\\)/\\2 \\1/i"));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.text.to_string(), "World Hello\n");

        // a delimiter that is not `/`, so a path needs no escaping
        let mut ed = fresh("/usr/bin\n");
        feed(&mut ed, &[Key::Char(':')]);
        feed(&mut ed, &keys("s#/usr#/opt#"));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.text.to_string(), "/opt/bin\n");

        // no match changes nothing and says so
        feed(&mut ed, &[Key::Char(':')]);
        feed(&mut ed, &keys("%s/nowhere/x/"));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.text.to_string(), "/opt/bin\n");
        assert!(ed.status.contains("not found"));

        // and the ex commands that are not substitutes still parse
        let mut ed = fresh("");
        feed(&mut ed, &[Key::Char(':')]);
        feed(&mut ed, &keys("q"));
        feed(&mut ed, &[Key::Enter]);
        assert!(ed.should_quit);
    }

    #[test]
    fn colon_in_visual_mode_substitutes_over_the_selection() {
        let mut ed = fresh("foo\nfoo\nfoo\n");
        feed(&mut ed, &keys("Vj"));
        feed(&mut ed, &[Key::Char(':')]);
        assert_eq!(
            ed.prompt.as_ref().unwrap().text,
            "'<,'>",
            "vim types the range for you"
        );
        feed(&mut ed, &keys("s/foo/bar/"));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.text.to_string(), "bar\nbar\nfoo\n");
        assert_eq!(ed.mode, Mode::Normal, "and the selection is done with");
    }

    /// `/` moves as you type; Escape puts you back where you started.
    #[test]
    fn incremental_search_moves_as_you_type_and_escape_comes_back() {
        let mut ed = fresh("alpha\nbeta\ngamma\ndelta");
        feed(&mut ed, &[Key::Char('/')]);
        feed(&mut ed, &keys("gam"));
        assert_eq!(ed.buffer.cursor_line_col().0, 2, "the cursor follows along");

        // a pattern that has stopped matching goes back to the origin, so
        // "no match" looks like no match
        feed(&mut ed, &keys("XYZ"));
        assert_eq!(ed.buffer.cursor, 0);
        feed(&mut ed, &[Key::Backspace, Key::Backspace, Key::Backspace]);
        assert_eq!(ed.buffer.cursor_line_col().0, 2, "and un-typing comes back");

        feed(&mut ed, &[Key::Esc]);
        assert_eq!(ed.buffer.cursor, 0, "escape restores the origin");
        assert!(ed.prompt.is_none());

        // RET commits where the preview was — not one match further on — and
        // leaves `n` and `N` walking from there.
        feed(&mut ed, &[Key::Char('/')]);
        feed(&mut ed, &keys("a"));
        assert_eq!(ed.buffer.cursor, 4);
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.cursor, 4);
        feed(&mut ed, &keys("n"));
        assert_eq!(ed.buffer.cursor, 9);
        feed(&mut ed, &keys("N"));
        assert_eq!(ed.buffer.cursor, 4);
    }

    #[test]
    fn dashboard_hotkey_runs_lisp() {
        let mut ed = Editor::new();
        ed.apply(EditorCommand::AddDashboardItem {
            key: 'p',
            label: "Projects".into(),
            action: "zemacs-projects".into(),
        });
        let out = ed.handle_key(Key::Char('p'));
        assert!(out.contains(&EditorCommand::CallLisp("(zemacs-projects)".into())));
    }
}

