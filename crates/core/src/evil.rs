//! The modal ("Evil") key grammar: `[count] operator motion`.
//!
//! `handle_key` is a pure translator — it reads the document to compute motion
//! targets but never mutates it. Everything it decides comes back as
//! [`EditorCommand`]s for [`Editor::apply`].
//!
//! Lookup order for every key, which is what makes the Lisp config authoritative:
//! prompt line → pending literal (`r`, `f`) → **user keymap** → built-in grammar.

use crate::{frame, Direction, Editor, EditorCommand, Key, Mode, Prompt, PromptKind};
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
}

impl Pending {
    pub fn clear(&mut self) {
        self.count = None;
        self.op = None;
        self.keys.clear();
        self.find = None;
        self.replace = false;
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
        if s.is_empty() {
            String::new()
        } else {
            format!("   [{s}]")
        }
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

impl Editor {
    /// Translate one key into zero or more commands.
    pub fn handle_key(&mut self, key: Key) -> Vec<EditorCommand> {
        if self.prompt.is_some() {
            return self.prompt_key(key);
        }
        if self.mode == Mode::Dashboard {
            return self.dashboard_key(key);
        }
        if self.mode == Mode::Insert {
            return self.insert_key(key);
        }
        self.normal_key(key)
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

        // 2. Esc always unwinds.
        if key == Key::Esc || key == Key::Ctrl('g') {
            let was_visual = matches!(self.mode, Mode::Visual | Mode::VisualLine);
            self.pending.clear();
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
        if let Some(cmd) = self.keymap.get(&(self.mode, seq.clone())) {
            // Same namespace as a dashboard action: a built-in verb if it names
            // one, otherwise a Lisp call. Without this, binding a key to
            // `find-file` would reach the Lisp primitive of that name, which
            // wants a path argument and has no way to prompt for one.
            let action = cmd.clone();
            self.pending.clear();
            return self.run_action(&action);
        }
        if self
            .keymap
            .keys()
            .any(|(m, k)| *m == self.mode && k.starts_with(&format!("{seq} ")))
        {
            return vec![]; // a longer binding may still match
        }

        let cmds = self.builtin(&seq, key);
        // `builtin` returns None only to ask for more keys.
        match cmds {
            Some(cmds) => {
                self.pending.clear();
                cmds
            }
            None => vec![],
        }
    }

    /// The built-in grammar. `None` means "incomplete, wait for more keys".
    fn builtin(&mut self, seq: &str, key: Key) -> Option<Vec<EditorCommand>> {
        let n = self.pending.count();
        let op = self.pending.op;
        let visual = matches!(self.mode, Mode::Visual | Mode::VisualLine);

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
            "p" => vec![EditorCommand::Checkpoint, EditorCommand::Paste { after: true }],
            "P" => vec![
                EditorCommand::Checkpoint,
                EditorCommand::Paste { after: false },
            ],
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

            // --- scrolling ---
            "C-d" => return Some(self.scroll_half(true)),
            "C-u" => return Some(self.scroll_half(false)),

            // --- prompts and meta ---
            ":" => {
                self.open_prompt(PromptKind::Ex);
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
            "j" | "<down>" => Motion {
                target: buf.line_start((line + n).min(buf.len_lines().saturating_sub(1))),
                span: Span::Linewise,
            },
            "k" | "<up>" => Motion {
                target: buf.line_start(line.saturating_sub(n)),
                span: Span::Linewise,
            },
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
        let Some((start, end)) = self.selection() else {
            return vec![];
        };
        let linewise = self.mode == Mode::VisualLine;
        let mut cmds = vec![EditorCommand::SetMode(Mode::Normal)];
        cmds.extend(self.operate(op, start, end, linewise));
        cmds
    }

    fn operate(&mut self, op: Op, start: usize, end: usize, linewise: bool) -> Vec<EditorCommand> {
        if start >= end {
            return vec![];
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

    // --- search ----------------------------------------------------------

    fn search_from(&mut self, from: usize, forward: bool) -> Vec<EditorCommand> {
        if self.last_search.is_empty() {
            return vec![EditorCommand::Message("no previous search".into())];
        }
        let hay = self.buffer.text.to_string();
        let pat = self.last_search.clone();
        // char offsets, so map through byte positions once.
        let byte_of = |ci: usize| self.buffer.text.char_to_byte(ci.min(self.buffer.len_chars()));
        let start_b = byte_of(from);
        let hit = if forward {
            hay[start_b..]
                .find(&pat)
                .map(|b| b + start_b)
                .or_else(|| hay.find(&pat))
        } else {
            hay[..start_b]
                .rfind(&pat)
                .or_else(|| hay.rfind(&pat))
        };
        match hit {
            Some(b) => vec![EditorCommand::MoveTo(self.buffer.text.byte_to_char(b))],
            None => vec![EditorCommand::Message(format!("pattern not found: {pat}"))],
        }
    }

    // --- prompt line -----------------------------------------------------

    fn prompt_key(&mut self, key: Key) -> Vec<EditorCommand> {
        let Some(p) = self.prompt.as_mut() else {
            return vec![];
        };
        match key {
            Key::Esc | Key::Ctrl('g') => {
                self.prompt = None;
            }
            Key::Backspace => {
                if p.text.pop().is_none() {
                    self.prompt = None;
                } else {
                    p.refilter();
                }
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
            _ => {}
        }
        vec![]
    }

    /// Enter: what the prompt was asking for decides what happens.
    fn accept_prompt(&mut self) -> Vec<EditorCommand> {
        let Some(p) = self.prompt.take() else {
            return vec![];
        };
        match p.kind {
            PromptKind::Ex => self.ex_command(&p.text),
            PromptKind::Search => {
                self.last_search = p.text;
                self.search_from(self.buffer.cursor + 1, true)
            }
            PromptKind::Command => {
                let name = p.value();
                if name.is_empty() {
                    vec![]
                } else {
                    // Bare `(name)` — same shape a keybinding sends, so a
                    // command works identically however it was invoked.
                    vec![EditorCommand::CallLisp(format!("({name})"))]
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
        }
    }

    /// Open one of the completing prompts.
    pub fn open_prompt(&mut self, kind: PromptKind) {
        let (label, items) = match kind {
            PromptKind::Command => ("M-x ", self.commands.clone()),
            PromptKind::Buffer => ("Buffer: ", self.buffer_names()),
            // The app fills these in as the path is typed; core does no IO.
            PromptKind::File => ("Find file: ", Vec::new()),
            PromptKind::Ex => (":", Vec::new()),
            PromptKind::Search => ("/", Vec::new()),
        };
        self.prompt = Some(Prompt::new(kind, label, items));
    }

    fn ex_command(&mut self, line: &str) -> Vec<EditorCommand> {
        let line = line.trim();
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
            Key::Char(c) => {
                let action = self
                    .dashboard
                    .entries()
                    .iter()
                    .find(|i| i.key == c)
                    .map(|i| i.action.clone());
                action.map(|a| self.run_action(&a)).unwrap_or_default()
            }
            Key::Esc => vec![EditorCommand::SetMode(Mode::Normal)],
            _ => vec![],
        }
    }

    /// A named command: a few built-in verbs, anything else is a Lisp call.
    /// Shared by dashboard items and key bindings, so the two name the same
    /// things.
    ///
    /// Mode-neutral by design — a key bound in Visual mode must not silently
    /// drop you into Normal. The dashboard leaves itself via the commands that
    /// load a buffer (`config`, `open:`) or via `scratch`.
    fn run_action(&mut self, action: &str) -> Vec<EditorCommand> {
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
            .collect()
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
        ed.commands = vec!["alpha".into(), "beta".into(), "gamma".into()];
        ed.open_prompt(PromptKind::Command);
        assert_eq!(ed.prompt.as_ref().unwrap().current(), Some("alpha"));

        for (key, want) in [
            (Key::Ctrl('j'), "beta"),
            (Key::Ctrl('n'), "gamma"),
            (Key::Down, "alpha"),
            (Key::Ctrl('k'), "gamma"),
            (Key::Ctrl('p'), "beta"),
            (Key::Up, "alpha"),
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
