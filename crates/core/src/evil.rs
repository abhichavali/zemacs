//! The modal ("Evil") key grammar: `[count] operator motion`.
//!
//! `handle_key` is a pure translator — it reads the document to compute motion
//! targets but never mutates it. Everything it decides comes back as
//! [`EditorCommand`]s for [`Editor::apply`].
//!
//! With exactly one exception, and it earns its keep: a *replay* — `@` or `.` —
//! applies each key's commands before feeding the next, because both are
//! recordings of *decisions* and every decision after the first has to see what
//! the one before it did. `apply` is still the only writer — see
//! [`Editor::run_keys`], which is that exception in one place for both.
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
    /// `>` and `<`. True shifts right. Always linewise, whatever the motion —
    /// `>w` indents the line the word is on, because indentation is a property
    /// of a line and there is nothing else it could mean.
    Shift(bool),
    /// `gu`, `gU`, `g~`.
    Case(Case),
    /// `gq` and `gw` — re-wrap to the fill column. True leaves point where it
    /// was, which is the whole of the difference between the two.
    Format(bool),
}

/// What `gu`, `gU` and `g~` do to a run of text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Case {
    Lower,
    Upper,
    Toggle,
}

impl Op {
    /// The key that, typed next, doubles this operator into its linewise form:
    /// `dd`, `yy`, `>>`, `guu`. Always the operator's *last* key, which is why
    /// the two-key ones need no special case.
    fn double(self) -> char {
        match self {
            Op::Delete => 'd',
            Op::Change => 'c',
            Op::Yank => 'y',
            Op::Shift(true) => '>',
            Op::Shift(false) => '<',
            Op::Case(Case::Lower) => 'u',
            Op::Case(Case::Upper) => 'U',
            Op::Case(Case::Toggle) => '~',
            Op::Format(false) => 'q',
            Op::Format(true) => 'w',
        }
    }

    /// How the operator reads in the which-key trail.
    fn label(self) -> &'static str {
        match self {
            Op::Delete => "d",
            Op::Change => "c",
            Op::Yank => "y",
            Op::Shift(true) => ">",
            Op::Shift(false) => "<",
            Op::Case(Case::Lower) => "gu",
            Op::Case(Case::Upper) => "gU",
            Op::Case(Case::Toggle) => "g~",
            Op::Format(false) => "gq",
            Op::Format(true) => "gw",
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
    /// `i` or `a` has been typed where a text object can start, and the next
    /// key names the object: `ciw`, `di(`, `ya"`.
    pub object: Option<char>,
}

impl Pending {
    pub fn clear(&mut self) {
        self.count = None;
        self.op = None;
        self.keys.clear();
        self.find = None;
        self.replace = false;
        self.literal = None;
        self.object = None;
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
            s.push_str(op.label());
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
        if let Some(o) = self.object {
            s.push(o);
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

/// How many places `C-o` can walk back through. vim's is 100 too.
const JUMP_LIMIT: usize = 100;

/// The motions that count as a *jump* — the ones that leave the neighbourhood,
/// and therefore the ones `C-o` should be able to undo. vim's own list, minus
/// the searches and the marks, which are not in the motion table and file
/// themselves where they are handled.
const JUMPS: &[&str] = &[
    "G", "g g", "{", "}", "(", ")", "H", "M", "L", "%", "[ [", "] ]", "n", "N",
];

/// Where `gq` wraps when nothing has said otherwise. `text_width` is a
/// *centring* width and is 0 far more often than not, so this is the fallback
/// rather than the setting.
const FILL_COLUMN: usize = 79;

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
    /// The last `f`/`t` and its target, for `;` and `,`.
    last_find: Option<(char, char)>,
    /// The last visual selection, for `gv`. Written by `set_mode` in the parent
    /// module, which is the one place that knows a selection has *ended*.
    pub(crate) last_selection: Option<(usize, usize)>,
    /// The keys of the last change, for `.`.
    ///
    /// Keys and not commands, exactly as a macro is, and for the same reason:
    /// `.` after `ciwfoo` has to change *this* word to `foo`, which means
    /// re-deciding what the word is. Storing the commands would re-run the
    /// offsets the original change was computed against and edit whatever
    /// happens to be there now.
    change: Vec<Key>,
    /// The keys since the grammar was last at rest. Promoted to `change` when
    /// it comes to rest again *having mutated something*; discarded when it
    /// turns out to have been a motion.
    candidate: Vec<Key>,
    /// Whether `candidate` has mutated the document yet.
    dirty: bool,
    /// Set by `.` itself, so the repeat does not record `.` as the new change.
    just_repeated: bool,
    /// True while `.` is running, so the replay does not record itself as the
    /// new last change and `..` stays a repeat rather than a fixpoint.
    repeating: bool,
    /// `ma` — one marker per (buffer, letter).
    ///
    /// Keyed by buffer because vim's lowercase marks are per file and a marker
    /// only resolves in the buffer it was made in: a mark set elsewhere would
    /// otherwise read as this buffer's. ponytail: nothing removes the entry
    /// when a buffer goes away, so a long session leaks a handful of dead
    /// (id, char) pairs. They read as "mark not set", which is the right
    /// answer anyway; move the map onto `Buffer` if that ever stops being true.
    marks: HashMap<(BufferId, char), MarkerId>,
    /// Where you were before each jump, oldest first — `C-o`, `C-i` and
    /// `` `` ``. Markers for the marks' reason: an edit above a remembered line
    /// must not send `C-o` a few characters adrift. Buffer-stamped because a
    /// marker only resolves in the buffer it was made in, and an entry that no
    /// longer does is stepped over rather than landed on.
    jumps: Vec<(BufferId, MarkerId)>,
    /// How far back `C-o` has walked. `jumps.len()` is "at the newest", which
    /// is where every jump leaves it.
    jump_at: usize,
    /// `R`. Insert mode overwrites while this is on.
    ///
    /// ponytail: a flag rather than a `Mode::Replace`, so the modeline says
    /// INSERT and `<bs>` deletes rather than putting back what was overwritten.
    /// The mode is the honest version and costs an arm in `label`, `from_name`,
    /// `set_mode`, the modeline's colour table, `query`'s name table and the
    /// renderer's cursor shape — six files for a word and a backspace.
    replacing: bool,
    /// A block insert (`I`/`A` in `C-v`) under way: where the typing began, and
    /// the other lines it has still to be copied onto.
    block: Option<(MarkerId, Vec<MarkerId>)>,
    /// What `".` holds — the text of the last insert session — and whether one
    /// is open, so the next keystroke in Insert knows to start a new one.
    inserted: String,
    typing: bool,
    /// What `":` holds: the last `:` line, without its colon.
    last_ex: String,
    /// The operator waiting on a `/` — `d/foo`. Parked because the pattern
    /// arrives through a *prompt*, so the verb has to outlive the keystroke
    /// that named it by however long it takes to type the pattern.
    search_op: Option<Op>,
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

    /// The registers vim fills for you, on every yank and delete, whatever
    /// register you named: `"0` is the last yank, `"1`–`"9` the last nine
    /// line-sized deletes with the newest at `"1`, and `"-` anything deleted
    /// that was smaller than a line.
    ///
    /// The whole reason `"0p` still pastes what you yanked after a `dd` has
    /// been and gone, which is the single most-missed thing about registers.
    fn record(&mut self, deleted: bool, text: &str, linewise: bool) {
        let slot = |n: u8| char::from(b'0' + n);
        if !deleted {
            self.registers.insert('0', (text.to_string(), linewise));
        } else if !linewise && !text.contains('\n') {
            self.registers.insert('-', (text.to_string(), false));
        } else {
            for n in (1..9u8).rev() {
                if let Some(v) = self.registers.get(&slot(n)).cloned() {
                    self.registers.insert(slot(n + 1), v);
                }
            }
            self.registers.insert('1', (text.to_string(), linewise));
        }
    }
}

/// Registers that swallow or that come from somewhere else, and therefore
/// never reach the map: the black hole, and the two spellings of the system
/// clipboard — which *is* the unnamed register here (see `Editor::register`).
fn special_register(name: char) -> bool {
    matches!(name, '_' | '+' | '*')
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
    "zoom-in",
    "zoom-out",
    "zoom-reset",
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
    "dired-delete",
    "dired-unmark-all",
    "dired-copy-filename",
    "dired-rename",
    "dired-copy",
    "dired-mkdir",
    "dired-create-file",
    "dired-toggle-hidden",
    "dired-refresh",
    "ace-window",
    "search-line",
    "search-project",
    // `project-forget` is the last project verb core owns, and it is not really
    // one: it drops the app's file-list cache, which is the only thing under
    // `project-` that a config has no reason to bend. Every other name —
    // `root`, `dired`, `compile`, `test`, `find-file`, `find-dir`, `switch`,
    // `open`, `make`, `clone` — is a `defun` in `runtime/plugins/project.lisp`,
    // and a name core owns can never reach the image, so their *absence from
    // this list* is what makes them reachable. `M-x` still offers all of them —
    // `refresh-commands` publishes every zero-argument function in the ZEMACS
    // package — so the only thing that changed is which side answers.
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
        // A keystroke always redraws. Most of what it does never reaches
        // `apply` — the prompt's text, the pending sequence, the visual
        // selection, a mode change — so the draw loop learns about it here or
        // not at all. See [`Editor::generation`].
        self.touch();
        // corfu, and *before* the dispatch rather than after it, which is the
        // opposite of which-key below and is explained on `retire_completion`:
        // what makes a completion popup stale is the mode and the cursor, and
        // this key's effect on those is still sitting in a command list.
        self.retire_completion();
        // A menu opened by the pointer is dismissed by the keyboard, which is
        // what every menu on every platform does. Unconditional and not a key
        // test: there is no keystroke that means "keep the menu up", since the
        // menu is not on the keyboard's path at all.
        self.context_menu = None;
        // ...and so is a box the pointer opened. Here rather than in a hook for
        // `retire_completion`'s reason turned inside out: a completion popup is
        // retired by a *predicate* because whether it is still true is a question
        // about the cursor, and this one has no such question to ask — nothing
        // about the editor can tell you the pointer is still on the mark. So the
        // rule is the blunt one every platform uses, and it has to fire *before*
        // the dispatch: a key that scrolls moves the text out from under a
        // pointer that has not moved, and a box left up would then be describing
        // whatever line slid under it.
        self.tooltip = None;
        let cmds = self.dispatch_key(key);
        self.note_change(key, &cmds);
        if self.pending.keys.is_empty() {
            self.which_key.clear();
        }
        cmds
    }

    /// Note `key` against `.`, and decide whether the change it belongs to has
    /// finished.
    ///
    /// Called before the key is interpreted, because whether a change is *under
    /// way* is answered by the mode we are in now and whether one has *ended*
    /// by the mode we are in after — so the recording brackets the dispatch
    /// rather than sitting inside it.
    ///
    /// What counts as a change is decided by what the key produced: a batch
    /// that mutates the document starts one, and it runs until the grammar is
    /// back in Normal mode with nothing pending. That is what folds an insert
    /// session into the change that opened it, so `.` after `cwfoo<esc>` types
    /// `foo` over the next word instead of only re-entering Insert.
    fn note_change(&mut self, key: Key, cmds: &[EditorCommand]) {
        if self.vim.repeating || std::mem::take(&mut self.vim.just_repeated) {
            return;
        }
        // `u` and `C-r` are not changes to repeat: `.` after an undo means the
        // edit you undid, which is what every vim user expects and is why this
        // is not simply "the last thing that touched the buffer".
        if matches!(
            cmds.first(),
            Some(EditorCommand::Undo | EditorCommand::Redo)
        ) {
            self.vim.candidate.clear();
            self.vim.dirty = false;
            return;
        }
        // Every key joins the candidate, because whether it turns out to be
        // part of a change is not knowable when it arrives: the `d` of `dw`
        // mutates nothing and produces no commands at all. Recording only from
        // the first mutation is what made `.` repeat the *motion*.
        self.vim.candidate.push(key);
        if cmds.iter().any(EditorCommand::mutates_document) {
            self.vim.dirty = true;
        }
        // The mode this batch *lands* in, which is not `self.mode`: the commands
        // have not been applied yet, so the `SetMode(Insert)` that `ciw` just
        // produced is still in the list rather than in the editor. Reading the
        // live mode here ended the change the instant it began — and, for the
        // Esc that closes one, never ended it at all.
        let after = cmds
            .iter()
            .rev()
            .find_map(|c| match c {
                EditorCommand::SetMode(m) => Some(*m),
                _ => None,
            })
            .unwrap_or(self.mode);
        // At rest: Normal mode with the grammar holding nothing. That is what
        // folds an insert session into the change that opened it — `cwfoo<esc>`
        // stays incomplete until the Esc, so `.` types `foo` over the next word
        // rather than merely re-entering Insert.
        let resting = after == Mode::Normal
            && self.pending.op.is_none()
            && self.pending.object.is_none()
            && self.pending.find.is_none()
            && self.pending.literal.is_none()
            && self.pending.count.is_none()
            && !self.pending.replace
            && self.pending.keys.is_empty();
        if resting {
            if self.vim.dirty {
                self.vim.change = std::mem::take(&mut self.vim.candidate);
            }
            self.vim.candidate.clear();
            self.vim.dirty = false;
        }
    }

    /// Feed `keys` back through [`Editor::handle_key`], `n` times over,
    /// applying what each one produced before the next arrives.
    ///
    /// The one place in this file that mutates the document, and it is the
    /// engine of *both* `.` and `@` — one function rather than the two copies
    /// it was, because they are the same mechanism twice: a macro and a repeat
    /// are recordings of **decisions**, so every key after the first has to see
    /// what the one before it did. The second `dw` of a replay deletes the word
    /// under the cursor *now*. Handing the whole batch back to the caller
    /// instead would compute every offset against the text as it stood before
    /// the replay started, and a two-line macro would delete the same word
    /// twice.
    ///
    /// Still not a crack in "`apply` is the only writer": it is `apply` doing
    /// the writing, with the replay driving the loop instead of the app.
    /// Commands core cannot carry out itself travel back up as usual, so a
    /// macro can still open a file or call Lisp.
    fn run_keys(&mut self, keys: &[Key], n: usize) -> Vec<EditorCommand> {
        let mut out = Vec::new();
        for _ in 0..n {
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
        out
    }

    /// `.` — the last change, again, `n` times.
    fn repeat_change(&mut self, n: usize) -> Vec<EditorCommand> {
        let keys = self.vim.change.clone();
        if keys.is_empty() {
            return vec![EditorCommand::Message("no change to repeat".into())];
        }
        if self.vim.repeating {
            return vec![];
        }
        // The `.` itself is still sitting in the pending key sequence — this is
        // reached from inside `builtin`, which clears it only on the way out.
        // Replaying on top of that made the first key of the change read as
        // `. d` rather than `d`, and matched nothing at all.
        self.pending.clear();
        self.vim.repeating = true;
        let out = self.run_keys(&keys, n);
        self.vim.repeating = false;
        self.vim.just_repeated = true;
        out
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
        // avy: the image has labels up and asked for this keystroke. `take`, so
        // the grab is spent whatever the key turns out to be — a key that names
        // no label is a *cancel*, and the image has to be told about it or it is
        // left holding a screenful of overlays nothing will remove. Ace's rule
        // ("anything else cancels") one layer up, where the labels live.
        //
        // Nothing is resolved here. The key is spelled the way `key-bindings`
        // spells one and handed straight over; see `EditorCommand::GrabKey` for
        // why core deliberately keeps no label table of its own.
        if let Some(f) = self.grab_key.take() {
            return vec![EditorCommand::CallLisp(format!(
                "({f} {})",
                crate::query::lisp_string(&key.token())
            ))];
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
    /// Three ways an editor binding still fires. The buffer's own modes are
    /// asked first, which is the precedence `normal_key` already gives them:
    /// `"terminal"` is every session there will ever be, `ai-mode` is *this*
    /// one, and the narrower map wins. Then the Terminal keymap, so anything at
    /// all can still be reclaimed by binding it in `"terminal"`. Failing both,
    /// keys a terminal has no use for — the ones carrying Command, plus the two
    /// modified Enters — fall through to the *Normal* keymap, which is what
    /// keeps `M-x`, `C-M-j`, `M-o` and the window splits alive inside a shell.
    ///
    /// Ctrl is deliberately not in that last set. `C-c`, `C-a`, `C-d`, `C-r` and
    /// `C-w` are the shell's, and a `C-c` that stopped here would mean never
    /// being able to interrupt a running program.
    ///
    /// Exact bindings only, with none of `normal_key`'s `mode_prefix` dance. A
    /// prefix *waits*, and waiting costs nothing in a file buffer but the whole
    /// keystroke here: the first key of a `C-c C-e` would be held back from a
    /// program running right now, and released — late, and after whatever the
    /// second key turned out to be — or dropped. There is no ordering of that
    /// which is safe, so a session binding is one chord, and the multi-key ones
    /// live in the Normal buffer you step out to.
    fn terminal_key(&mut self, key: Key) -> Vec<EditorCommand> {
        let token = key.token();
        if let Some(action) = self
            .mode_binding(&token)
            .or_else(|| self.keymap_lookup(&token))
        {
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
        let start = word_backward(&self.buffer, end, false);
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
        // The first key of a session starts `".` over. Here rather than at the
        // dozen places that enter Insert, because "the session has begun" is
        // exactly "a key arrived while we are in it".
        if !self.vim.typing {
            self.vim.typing = true;
            self.vim.inserted.clear();
        }
        if let Key::Char(c) = key {
            self.vim.inserted.push(c);
        }
        // Bindings are live in Insert mode too — that is how `M-+` or `C-s`
        // keep working while you type.
        //
        // Multi-key sequences are live as well, but only ones that *begin* with
        // a key which is not text. `C-x C-s` has to save while you are typing,
        // and pausing after `C-x` costs nothing because `C-x` was never going
        // into the buffer. A sequence beginning with a printable character
        // stays Normal-mode only, for the reason this used to refuse all of
        // them: waiting there swallows the letter you meant to type.
        if !self.pending.keys.is_empty() || !matches!(key, Key::Char(_)) {
            self.pending.keys.push(key.token());
            let seq = self.pending.keys.join(" ");
            // The same two questions Normal mode asks, through the same two
            // readers: [`Editor::keymaps`] is the only statement anywhere of
            // which maps a key is looked up in, and Insert layers over nothing,
            // so it answers this one with Insert alone.
            if let Some(cmd) = self.keymap_lookup(&seq) {
                self.pending.clear();
                return self.run_action(&cmd);
            }
            if self.keymap_prefix(&seq) {
                return vec![]; // more of the sequence to come
            }
            // Not a binding and not a prefix. A sequence that got somewhere and
            // then died is dropped whole — `C-x z` is a typo, not two commands —
            // but a *first* key that simply is not bound falls through to be
            // handled as itself, which is how Esc and the arrows still work.
            let dead = self.pending.keys.len() > 1;
            self.pending.clear();
            if dead {
                return vec![];
            }
        }
        // `TAB` and `RET` mean the popup while there is one, and mean themselves
        // otherwise — which is corfu's binding and *not* something the image
        // could have done for itself.
        //
        // The comment this replaces was right about why: a Lisp command bound to
        // `RET` would have to insert the newline itself when no popup was up,
        // and it would land a queue turn late — `a<RET>b` coming out as `ab`
        // and a newline (`docs/threading.org`). Asked *here* the question has no
        // fallback in it at all. Core knows synchronously whether a popup is on
        // screen, because core is what decides; the branch that is not the popup
        // falls through to the tab and the newline below, on the same keystroke.
        if self.completion().is_some_and(|c| !c.accepting) {
            match key {
                Key::Tab => return self.run_action("lsp-complete-next"),
                Key::BackTab => return self.run_action("lsp-complete-previous"),
                Key::Enter => {
                    // Latched before the round trip: see
                    // [`Completion::accepting`].
                    self.completion_accepting();
                    return self.run_action("lsp-complete-accept");
                }
                _ => {}
            }
        }
        match key {
            Key::Esc | Key::Ctrl('c') => self.leave_insert(),
            // `R`: the character under point goes, this one takes its place.
            // At the end of a line there is nothing to replace, so it is an
            // ordinary insert — which is what vim does and what stops `R` from
            // eating the newline and pulling the next line up.
            Key::Char(c) if self.vim.replacing => {
                let at = self.buffer.cursor;
                let (line, _) = self.buffer.cursor_line_col();
                match at < self.buffer.line_end(line) {
                    true => vec![
                        EditorCommand::DeleteRange(at, at + 1),
                        EditorCommand::MoveTo(at),
                        EditorCommand::InsertChar(c),
                    ],
                    false => vec![EditorCommand::InsertChar(c)],
                }
            }
            Key::Char(c) => vec![EditorCommand::InsertChar(c)],
            Key::Tab => vec![EditorCommand::InsertText(
                " ".repeat(self.settings.tab_width),
            )],
            // A shifted key means what the unshifted one means once the keymap
            // above has declined it, and that is the whole of the policy: shift
            // is a *spelling* for a binding, so an unbound `⇧⏎` or `⇧←` must not
            // become a dead key in the one mode where a hand is already holding
            // shift to type capitals. This editor has no shift-selection —
            // Visual state is how you select — so there is nothing else it could
            // sensibly mean.
            // The newline *and* the indentation to start the next line at, as
            // one batch — which is what makes this possible at all here rather
            // than in Lisp: a `RET` bound to a Lisp command would insert its
            // newline a queue turn late, after whatever you typed next, so
            // `a<RET>b` would come out as `ab` and a newline
            // (`docs/threading.org`). See [`Editor::indent_after`].
            Key::Enter | Key::ShiftEnter => {
                let indent = self.indent_for_next_line();
                match indent.is_empty() {
                    true => vec![EditorCommand::InsertNewline],
                    false => vec![
                        EditorCommand::InsertNewline,
                        EditorCommand::InsertText(indent),
                    ],
                }
            }
            Key::Backspace => vec![EditorCommand::DeleteBackward],
            Key::MetaBackspace => self.delete_word_backward(),
            Key::Left | Key::ShiftLeft => vec![EditorCommand::MoveCursor(Direction::Left)],
            Key::Right | Key::ShiftRight => vec![EditorCommand::MoveCursor(Direction::Right)],
            // A word at a time, which is what a Meta'd arrow means in readline,
            // in every text field on this platform, and — via `Input::AltLeft` —
            // inside a terminal session too.
            Key::MetaLeft => vec![EditorCommand::MoveTo(word_backward(
                &self.buffer,
                self.buffer.cursor,
                false,
            ))],
            Key::MetaRight => vec![EditorCommand::MoveTo(word_forward(
                &self.buffer,
                self.buffer.cursor,
                false,
            ))],
            Key::Up | Key::ShiftUp => vec![EditorCommand::MoveCursor(Direction::Up)],
            Key::Down | Key::ShiftDown => vec![EditorCommand::MoveCursor(Direction::Down)],
            // Home is the line start rather than the first non-blank, as it is
            // in every other text field on the machine — and as `<home>` is in
            // the Normal-mode motion table, so the key does not change meaning
            // when you press `i`.
            Key::Home => vec![EditorCommand::MoveTo(
                self.buffer.line_start(self.buffer.line_of(self.buffer.cursor)),
            )],
            Key::End => vec![EditorCommand::MoveTo(
                self.buffer.line_end(self.buffer.line_of(self.buffer.cursor)),
            )],
            // A screenful, which is `C-f`/`C-b`'s arithmetic — no count here,
            // since there is no way to type one in Insert.
            Key::PageDown => self.scroll_page(true, 1),
            Key::PageUp => self.scroll_page(false, 1),
            Key::Delete => vec![EditorCommand::DeleteForward],
            // Nothing, rather than the tab it used to type by arriving here as
            // `Tab`: nobody presses `⇧⇥` wanting whitespace. The keymap above
            // has already had its say, so `(define-key "insert" "<backtab>" …)`
            // is what gives it a meaning in a file buffer.
            Key::BackTab => vec![],
            // `M-<ret>` among them: it is a binding or it is nothing, and the
            // keymap above has already had its say. Typing a newline is `<ret>`.
            //
            // The F-keys are here on purpose and everywhere: they have no
            // default meaning in any mode, so being bindable *is* the feature.
            Key::Ctrl(_)
            | Key::Meta(_)
            | Key::CtrlMeta(_)
            | Key::MetaEnter
            | Key::MetaShiftEnter
            | Key::MetaShiftLeft
            | Key::MetaShiftRight
            | Key::F(_) => vec![],
            Key::CtrlEnter => vec![EditorCommand::SplitWindow(frame::Split::Columns)],
            Key::CtrlMetaEnter => vec![EditorCommand::SplitWindow(frame::Split::Rows)],
        }
    }

    /// Esc out of Insert: close the session, and finish whatever opened it.
    ///
    /// The one gesture whose *end* is handled here rather than where it began
    /// is the block insert — `I` and `A` in `C-v` type once and land on every
    /// line the block touched, and there is no other moment that knows what
    /// "once" turned out to be.
    fn leave_insert(&mut self) -> Vec<EditorCommand> {
        self.vim.typing = false;
        self.vim.replacing = false;
        // vim's `` `^ ``, and what `gi` goes back to.
        self.set_mark_at('^', self.buffer.cursor);
        let mut cmds = self.finish_block();
        cmds.push(EditorCommand::SetMode(Mode::Normal));
        cmds
    }

    /// Copy what the block insert typed onto the rest of its lines.
    ///
    /// Bottom-up, for [`Editor::op_block`]'s reason: every command in the batch
    /// is measured against the buffer as it stands now, so inserting on the
    /// last line first leaves every earlier target exactly where it was read.
    ///
    /// ponytail: a session containing a newline copies the newline too, which
    /// vim refuses to do at all. Refusing is more code than doing something
    /// defensible, and what it does is at least undoable in one press.
    fn finish_block(&mut self) -> Vec<EditorCommand> {
        let Some((start, rest)) = self.vim.block.take() else {
            return vec![];
        };
        let from = self.marker_position(start);
        self.delete_marker(start);
        let mut targets: Vec<usize> = rest
            .iter()
            .filter_map(|&id| self.marker_position(id))
            .collect();
        for id in rest {
            self.delete_marker(id);
        }
        let Some(from) = from.filter(|&f| f < self.buffer.cursor) else {
            return vec![];
        };
        let text = self.buffer.slice_string(from, self.buffer.cursor);
        targets.sort_unstable();
        let mut cmds = Vec::new();
        for at in targets.into_iter().rev() {
            cmds.push(EditorCommand::MoveTo(at));
            cmds.push(EditorCommand::InsertText(text.clone()));
        }
        cmds.push(EditorCommand::MoveTo(from));
        cmds
    }

    /// `I` and `A` in block mode: one insert session, applied to every line.
    ///
    /// The typing goes in at the block's first line as an ordinary session, and
    /// the others get a marker each so they are still findable after it — see
    /// [`Editor::finish_block`], which is where they are filled in.
    fn block_insert(&mut self, append: bool) -> Vec<EditorCommand> {
        let ranges = self.selection_ranges();
        let Some(&first) = ranges.first() else {
            return vec![EditorCommand::SetMode(Mode::Normal)];
        };
        let edge = |(s, e): (usize, usize)| if append { e } else { s };
        let rest: Vec<MarkerId> = ranges[1..]
            .iter()
            .map(|&r| self.make_marker(edge(r), Insertion::Stay))
            .collect();
        let at = edge(first);
        let start = self.make_marker(at, Insertion::Stay);
        self.vim.block = Some((start, rest));
        vec![
            EditorCommand::SetMode(Mode::Normal),
            EditorCommand::Checkpoint,
            EditorCommand::SetMode(Mode::Insert),
            EditorCommand::MoveTo(at),
        ]
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
            // Remembered before it is resolved, so `;` repeats a find that
            // found nothing this time and might next time.
            self.vim.last_find = Some((find, target));
            return match self.find_char(find, target, n) {
                Some(m) => self.resolve(op, m),
                None => vec![EditorCommand::Message(format!("not found: {target}"))],
            };
        }
        if self.pending.replace {
            let visual = self.mode.is_visual();
            self.pending.clear();
            let Key::Char(c) = key else { return vec![] };
            // Over a selection, `r` replaces every character in it — keeping
            // the newlines, because replacing those would fuse the lines the
            // selection spans into one.
            if visual {
                let Some((start, end)) = self.selection() else {
                    return vec![EditorCommand::SetMode(Mode::Normal)];
                };
                let out: String = self
                    .buffer
                    .slice_string(start, end)
                    .chars()
                    .map(|ch| if ch == '\n' { '\n' } else { c })
                    .collect();
                return vec![
                    EditorCommand::SetMode(Mode::Normal),
                    EditorCommand::Checkpoint,
                    EditorCommand::DeleteRange(start, end),
                    EditorCommand::MoveTo(start),
                    EditorCommand::InsertText(out),
                    EditorCommand::MoveTo(start),
                ];
            }
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
        if let Some(kind) = self.pending.object.take() {
            let n = self.pending.count();
            let op = self.pending.op.take();
            self.pending.clear();
            let Key::Char(obj) = key else { return vec![] };
            let Some((start, end, linewise)) = self.text_object(kind, obj, n) else {
                return vec![];
            };
            // With an operator, apply it. Without one — which means visual mode,
            // since `iw` alone in Normal mode is `i` — the object *becomes* the
            // selection, which is how `vi(` then `d` works.
            return match op {
                Some(op) => self.operate(op, start, end, linewise),
                None => {
                    self.visual_anchor = Some(start);
                    vec![EditorCommand::MoveTo(end.saturating_sub(1).max(start))]
                }
            };
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
            // ...and the echo area is unwound too, which is `C-g` in Emacs and
            // is what makes a report *dismissable*: `:!` answers in the status
            // line now, and a line that only leaves when something else
            // happens to write there is a line you learn to ignore.
            let mut cmds = vec![EditorCommand::Message(String::new())];
            if was_visual {
                cmds.push(EditorCommand::SetMode(Mode::Normal));
            }
            return cmds;
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

    /// Drop a request to enter Insert in a buffer that refuses edits.
    ///
    /// The grammar now runs in dired, magit and the dashboard, which is what
    /// makes their motions and operators work — but `i` there would park you in
    /// a mode where every keystroke is refused by `apply`, with the modeline
    /// claiming INSERT. The text is re-rendered from state and an edit could
    /// never survive anyway, so the honest answer is to say so and stay put.
    ///
    /// A buffer a mode has *frozen* is the same refusal for a different reason,
    /// and says so differently: `*magit*` is not a file, while a frozen `.org`
    /// very much is — it is being rendered instead of edited, which is
    /// something you can turn off, so the message names that rather than
    /// denying the file exists.
    fn without_insert(&mut self, cmds: Vec<EditorCommand>) -> Vec<EditorCommand> {
        let why = self.buffer.read_only();
        if why == crate::ReadOnly::No
            || !cmds
                .iter()
                .any(|c| matches!(c, EditorCommand::SetMode(Mode::Insert)))
        {
            return cmds;
        }
        let name = self.buffer.name();
        vec![EditorCommand::Message(match why {
            crate::ReadOnly::Claimed => format!("{name} is read-only"),
            _ => format!("{name} is not a file"),
        })]
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
        if matches!(seq, "j" | "k" | "<down>" | "<up>" | "S-<down>" | "S-<up>") {
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

        // `cw` is `ce`, and `cW` is `cE`. vim's one deliberate irregularity in
        // the operator grammar, and the reason it is there: changing a word
        // should not swallow the space after it, or every `cw` is followed by
        // typing the space back in. Only on a non-blank — on whitespace `cw`
        // really is `dw`, because then the "word" is the run of spaces.
        let seq = match (op, seq) {
            (Some(Op::Change), "w" | "W")
                if self.buffer.char_at(self.buffer.cursor).map(class) != Some(0) =>
            {
                if seq == "w" {
                    "e"
                } else {
                    "E"
                }
            }
            _ => seq,
        };

        // Motions first: they compose with a pending operator.
        if let Some(m) = self.motion(seq, n) {
            // A *jump* files where you were, so `C-o` can bring you back and
            // `` `` `` can bounce off it. vim's list — the motions that leave
            // the neighbourhood — and not every motion: a list of every `j` you
            // pressed is a list nobody can walk.
            if op.is_none() && JUMPS.contains(&seq) {
                self.push_jump();
            }
            return Some(self.resolve(op, m));
        }

        // Prefixes that need another key.
        if matches!(seq, "g" | "Z" | "z" | "[" | "]" | "C-w") {
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
        // the `` ` `` of ``d`a`` as "not a motion, give up" — but *after* the
        // key that doubles a pending operator, or `gqq` starts recording a
        // macro named `q` instead of wrapping the line.
        let doubles = op.is_some_and(|o| seq.len() == 1 && seq.starts_with(o.double()));
        if !doubles && matches!(seq, "\"" | "m" | "`" | "'" | "q" | "@") {
            self.pending.literal = seq.chars().next();
            self.pending.keys.clear();
            return None;
        }

        // `i` and `a` start a text object wherever one can go: after an
        // operator (`ciw`) or in visual mode (`vi(`). Everywhere else they are
        // still the keys that enter Insert, which is why this is not simply an
        // arm in the match below — in Normal mode with nothing pending, `i` has
        // no object reading at all.
        //
        // Before the operator-abort, which would otherwise eat the `i` of `ciw`
        // as "not a motion, give up".
        if (op.is_some() || visual) && matches!(seq, "i" | "a") {
            self.pending.object = seq.chars().next();
            self.pending.keys.clear();
            return None;
        }

        // `d/foo` — a search is a motion, so the operator has to *wait* for the
        // pattern rather than being thrown away by the abort below when `/`
        // turns out not to be one. Here rather than in the match, because the
        // abort stands between the two.
        if matches!(seq, "/" | "?") {
            self.vim.search_op = op;
            if op.is_none() {
                // Filed before the prompt opens: the incremental preview drags
                // the cursor from the very next keystroke, and there is no
                // later moment that still knows where you were standing.
                self.push_jump();
            }
            self.open_prompt(PromptKind::Search);
            self.search_backward = seq == "?";
            self.pending.clear();
            return Some(vec![]);
        }

        // Doubled operator = linewise over `count` lines: dd, yy, cc.
        if let Some(op) = op {
            if seq.len() == 1 && seq.starts_with(op.double()) {
                let (line, _) = self.buffer.cursor_line_col();
                let last = (line + n - 1).min(self.buffer.last_line());
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
            "d" | "c" | "y" | ">" | "<" | "g u" | "g U" | "g ~" | "g q" | "g w" if !visual => {
                self.pending.op = Some(match seq {
                    "d" => Op::Delete,
                    "c" => Op::Change,
                    "y" => Op::Yank,
                    ">" => Op::Shift(true),
                    "<" => Op::Shift(false),
                    "g u" => Op::Case(Case::Lower),
                    "g U" => Op::Case(Case::Upper),
                    "g q" => Op::Format(false),
                    "g w" => Op::Format(true),
                    _ => Op::Case(Case::Toggle),
                });
                self.pending.keys.clear(); // `count` survives: `2dw` == `d2w`
                return None;
            }
            // In visual mode the operator applies to the selection immediately.
            "d" | "x" | "<delete>" if visual => self.op_selection(Op::Delete),
            "c" | "s" if visual => self.op_selection(Op::Change),
            "y" if visual => self.op_selection(Op::Yank),
            ">" if visual => self.op_selection(Op::Shift(true)),
            "<" if visual => self.op_selection(Op::Shift(false)),
            // In visual mode these are single keys, not `g`-prefixed: `u`
            // lowercases the selection where in Normal mode it undoes.
            "u" | "g u" if visual => self.op_selection(Op::Case(Case::Lower)),
            "U" | "g U" if visual => self.op_selection(Op::Case(Case::Upper)),
            "~" | "g ~" if visual => self.op_selection(Op::Case(Case::Toggle)),
            "g q" | "g w" if visual => self.op_selection(Op::Format(seq == "g w")),
            // `I` and `A` over a block are the column edit everybody reaches
            // `C-v` for; over any other selection they mean what they mean in
            // Normal mode, which is where they fall through to.
            "I" | "A" if self.mode == Mode::VisualBlock => self.block_insert(seq == "A"),

            // `~` on its own: flip the character under the cursor and step over
            // it, which is the one edit vim spells without an operator at all.
            "~" => {
                let end = (cursor + n).min(self.buffer.line_end(line));
                let mut cmds = self.operate(Op::Case(Case::Toggle), cursor, end, false);
                cmds.push(EditorCommand::MoveTo(end.min(
                    self.buffer.line_end(line).saturating_sub(1).max(cursor),
                )));
                cmds
            }

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
            // Swap which end of the selection the cursor is on, so a selection
            // taken in the wrong direction can be extended from the other side
            // rather than started again.
            "o" if visual => {
                let anchor = self.visual_anchor.unwrap_or(cursor);
                self.visual_anchor = Some(cursor);
                vec![EditorCommand::MoveTo(anchor)]
            }
            // Both carry the current line's indent onto the new one — vim's
            // `autoindent`, which evil leaves on and which every editor written
            // since has agreed about. Without it every `o` inside a function is
            // followed by typing back the indentation you were already at.
            //
            // The newline goes after the last line a fold is hiding rather than
            // after the cursor's own — see [`Self::open_below_line`]. The indent
            // still comes from the line you are *on*, which is the headline: it
            // is what you can see and what you are opening a line beneath.
            //
            // Inserted at a computed offset rather than by moving there and
            // typing: on a closed fold the line being opened after is a line the
            // cursor may not sit on, and `clamp_cursor` would have escaped it
            // back to the fold's head between the `MoveTo` and the newline —
            // putting the new line at the *top* of the fold, inside it, on a row
            // that is not drawn. `InsertAt` leaves point on the new line, which
            // is below the fold and therefore visible.
            "o" => {
                let at = self.buffer.line_end(self.open_below_line(line));
                let indent = self.buffer.indent_after(
                    line,
                    &self.settings.indent_openers,
                    self.settings.tab_width,
                );
                vec![
                    EditorCommand::Checkpoint,
                    EditorCommand::SetMode(Mode::Insert),
                    EditorCommand::InsertAt(at, format!("\n{indent}")),
                ]
            }
            // `O` opens *above*, so the newline goes in at the line's start and
            // point comes back to it — the blank line is now the one at
            // `line_start`, and the indent lands on it.
            "O" => vec![
                EditorCommand::Checkpoint,
                EditorCommand::SetMode(Mode::Insert),
                EditorCommand::MoveTo(self.buffer.line_start(line)),
                EditorCommand::InsertNewline,
                EditorCommand::MoveTo(self.buffer.line_start(line)),
                // This line's own indent and not `indent_after`'s: `O` opens
                // *above*, so the line it is copying from is the one it is about
                // to sit on top of, and a `{` at the end of that line opens a
                // block below it rather than above.
                EditorCommand::InsertText(self.buffer.line_indent(line)),
            ],

            // --- single-key edits ---
            // These all go through `operate` so they inherit its empty-range
            // guard: on a blank line `start == end`, and yanking that would
            // clobber the register with "" while deleting nothing.
            // `⌦` is `x`: the character under point, which is the one it is
            // pointing at in a modal editor and the one after it everywhere
            // else — the same character either way.
            "x" | "<delete>" => {
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
            // `s` is `cl` and `S` is `cc` — vim's two abbreviations, and the
            // reason they exist is that both are one key away from something
            // you meant to do anyway.
            "s" => {
                let end = (cursor + n).min(self.buffer.line_end(line));
                self.operate(Op::Change, cursor, end, false)
            }
            "S" => {
                let last = (line + n - 1).min(self.buffer.last_line());
                let end = (self.buffer.line_end(last) + 1).min(self.buffer.len_chars());
                self.operate(Op::Change, self.buffer.line_start(line), end, true)
            }
            // `X` deletes *backwards*, and stops at the line start rather than
            // pulling the previous line up.
            "X" => {
                let start = cursor.saturating_sub(n).max(self.buffer.line_start(line));
                self.operate(Op::Delete, start, cursor, false)
            }
            // `;` and `,` repeat the last `f`/`t`, forwards and backwards.
            // Without them `f` is a search you cannot continue, which is most
            // of what makes `f` worth typing.
            ";" | "," => {
                let Some((kind, target)) = self.vim.last_find else {
                    return Some(vec![EditorCommand::Message("no previous f/t".into())]);
                };
                // `,` is the same find with its direction turned over.
                let kind = match seq {
                    "," => match kind {
                        'f' => 'F',
                        'F' => 'f',
                        't' => 'T',
                        _ => 't',
                    },
                    _ => kind,
                };
                match self.find_char(kind, target, n) {
                    Some(m) => self.resolve(op, m),
                    None => vec![EditorCommand::Message(format!("not found: {target}"))],
                }
            }
            // `gv` — the selection you last had back again.
            "g v" => match self.vim.last_selection {
                Some((a, b)) if b <= self.buffer.len_chars() => vec![
                    // Point goes to the anchor *first*: entering visual mode
                    // anchors on wherever the cursor is, so setting the field
                    // by hand here would be overwritten a command later.
                    EditorCommand::MoveTo(a),
                    EditorCommand::SetMode(Mode::Visual),
                    EditorCommand::MoveTo(b.saturating_sub(1).max(a)),
                ],
                _ => vec![EditorCommand::Message("no previous selection".into())],
            },
            "J" => self.join_lines(n, true),
            // `gJ` joins without putting a space at the seam, which is the only
            // way to rejoin something that was wrapped mid-word.
            "g J" => self.join_lines(n, false),
            // `gi` — back where the last insert ended, still inserting. The
            // whole point of the `` `^ `` mark, and the fastest way back into a
            // line you stepped out of to look at something.
            "g i" => match self.mark_at('^') {
                Some(at) => vec![
                    EditorCommand::Checkpoint,
                    EditorCommand::SetMode(Mode::Insert),
                    EditorCommand::MoveTo(at.min(self.buffer.len_chars())),
                ],
                None => vec![EditorCommand::Message("no previous insert".into())],
            },
            "p" => self.paste_cmds(true, n),
            "P" => self.paste_cmds(false, n),
            "u" => vec![EditorCommand::Undo],
            "C-r" => vec![EditorCommand::Redo],
            // `R` — overwrite until Esc. See `Vim::replacing` for why this is a
            // flag over Insert rather than a mode of its own.
            "R" => {
                self.vim.replacing = true;
                vec![
                    EditorCommand::Checkpoint,
                    EditorCommand::SetMode(Mode::Insert),
                ]
            }
            // `C-a`/`C-x` — the number at or after point, `n` bigger or smaller.
            "C-a" => self.increment(n as i64),
            "C-x" => self.increment(-(n as i64)),
            // The jump list. `C-i` only, and deliberately not `<tab>`: the two
            // are the same byte on a terminal and are two distinct keys here,
            // and `<tab>` already means something in every listing.
            "C-o" => self.jump_walk(true, n),
            "C-i" => self.jump_walk(false, n),

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
            // A whole screen, less the two lines of overlap vim keeps so you
            // can see where the last one ended.
            "C-f" | "<pagedown>" => return Some(self.scroll_page(true, n)),
            "C-b" | "<pageup>" => return Some(self.scroll_page(false, n)),
            // The view moves a line and point comes along only when the view
            // would otherwise leave it behind — which is the whole difference
            // between `C-e` and `C-d`.
            "C-e" | "C-y" => return Some(self.scroll_line(seq == "C-e", n)),

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
            // `n` and `N` follow the direction the search was *started* in, so
            // `?foo` then `n` keeps going backwards. Without this `?` would be
            // a search you can only repeat forwards, which is not a search
            // backwards at all.
            "n" if self.search_backward => self.search_from(self.buffer.cursor, false),
            "N" if self.search_backward => self.search_from(self.buffer.cursor + 1, true),
            "n" => self.search_from(self.buffer.cursor + 1, true),
            "N" => self.search_from(self.buffer.cursor, false),
            // `*` and `#`: search for the word under the cursor. The pattern is
            // anchored on word boundaries, as vim's is — hunting `foo` must not
            // stop on every `foobar`.
            "*" | "#" => {
                let Some(word) = self.word_at_point() else {
                    return Some(vec![EditorCommand::Message("no word under cursor".into())]);
                };
                self.last_search = format!(r"\b{}\b", regex::escape(&word));
                self.search_backward = seq == "#";
                self.push_jump();
                match seq {
                    "*" => self.search_from(self.buffer.cursor + 1, true),
                    _ => self.search_from(self.buffer.cursor, false),
                }
            }
            // Put the cursor's line at the top, middle or bottom of the window.
            // The view moves and point does not, which is what separates these
            // from `H`/`M`/`L` above.
            "z z" | "z t" | "z b" => {
                let h = self.viewport_lines.max(1);
                self.scroll = match seq {
                    "z t" => line,
                    "z b" => line.saturating_sub(h - 1),
                    _ => line.saturating_sub(h / 2),
                };
                vec![]
            }
            // `.` — do the last change again. See `last_change`.
            "." => return Some(self.repeat_change(n)),
            // Window splits, reachable in Normal and Visual as well as Insert.
            "C-<ret>" => vec![EditorCommand::SplitWindow(frame::Split::Columns)],
            "C-M-<ret>" => vec![EditorCommand::SplitWindow(frame::Split::Rows)],
            // `C-w` — vim's window prefix. The whole family is here rather than
            // in the Lisp keymap because `C-w` alone used to *be* "next
            // window", and a prefix that only sometimes waits is worse than
            // either. `C-w C-w` is the old gesture, one key longer.
            //
            // ponytail: `h`/`j`/`k`/`l` all mean "the next window". Aiming at a
            // *direction* needs the panes' rectangles, and `Frame::panes` wants
            // the drawing area — which the renderer has and core does not. Park
            // the area beside `viewport_lines` on the day this matters.
            "C-w w" | "C-w C-w" | "C-w h" | "C-w j" | "C-w k" | "C-w l" => {
                vec![EditorCommand::FocusNextWindow]
            }
            "C-w v" | "C-w C-v" => vec![EditorCommand::SplitWindow(frame::Split::Columns)],
            "C-w s" | "C-w S" | "C-w C-s" => vec![EditorCommand::SplitWindow(frame::Split::Rows)],
            "C-w c" | "C-w q" | "C-w C-c" => vec![EditorCommand::CloseWindow],
            "C-w =" => vec![EditorCommand::ZoomWindow(0)],
            "C-w +" => vec![EditorCommand::ZoomWindow(1)],
            "C-w -" => vec![EditorCommand::ZoomWindow(-1)],
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
                (true, l) if l < buf.last_line() => l + 1,
                (false, l) if l > 0 => l - 1,
                _ => return line,
            };
            if crate::fold_hiding(buf.overlays(), buf.line_start(l)).is_none() {
                return l;
            }
        }
    }

    /// The line `o` opens *after*.
    ///
    /// Normally the cursor's own line. On a closed fold it is the last line the
    /// fold hides, because that is where "below this" is on the screen: `o` used
    /// to take `line_end` of the headline, which is the one line of a fold you
    /// can see, and so pushed the new line *into* the fold — invisible, and
    /// inside a subtree you had deliberately collapsed.
    ///
    /// Asked of [`Self::step_line`] rather than of the overlay list, so it is
    /// the same predicate `j` moves by: the line before the next *visible* one
    /// is the last hidden one, whether one fold or three nested ones are hiding
    /// it. A fold running to the end of the document has no visible line below
    /// it, which `step_line` reports by standing still — then the answer is the
    /// last line there is.
    ///
    /// `O` needs no equivalent. A fold's first line stays drawn and the cursor
    /// can only ever be on a drawn line, so "above the cursor's line" is already
    /// above the fold.
    fn open_below_line(&self, line: usize) -> usize {
        match self.step_line(line, true) {
            l if l == line => self.buffer.last_line(),
            l => l - 1,
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

    /// Every motion in the grammar, as the table it is: a key sequence in, a
    /// destination and the span an operator would cover, out.
    ///
    /// The arms answer `(target, span)` rather than building a [`Motion`] each,
    /// because a motion *is* that pair and twenty repetitions of the struct
    /// literal hid which arms differ in more than their arithmetic. Two do, and
    /// now say so by returning early: `j` and `k` hand the whole decision to
    /// [`Editor::vertical`], which is the one motion whose *unit* — buffer line
    /// or visual row — depends on the window rather than on the key.
    fn motion(&self, seq: &str, n: usize) -> Option<Motion> {
        let buf = &self.buffer;
        let (line, col) = buf.cursor_line_col();
        let cur = buf.cursor;
        let (target, span) = match seq {
            // The `S-` spellings alongside the bare ones for `insert_key`'s
            // reason: shift is how a binding is *written*, and an arrow pressed
            // with it held still has to move. Reached only when nothing bound
            // the key, since the keymap is consulted before this table.
            "h" | "<left>" | "S-<left>" => {
                (buf.line_start(line) + col.saturating_sub(n), Span::Exclusive)
            }
            "l" | "<right>" | "S-<right>" | "SPC" => {
                ((cur + n).min(buf.line_end(line)), Span::Exclusive)
            }
            // Vertical motions hold the column. Targeting the line start would
            // send `j` to column 0, which is wrong for the cursor and invisible
            // to an operator (a linewise span only reads the *line*).
            "j" | "<down>" | "S-<down>" => return Some(self.vertical(true, n)),
            "k" | "<up>" | "S-<up>" => return Some(self.vertical(false, n)),
            // Home is `0` and not `^`: beginning of line, which is what Home
            // means in every other text field on the machine. Being motions
            // rather than special cases, both compose with an operator and with
            // a selection for free — `d<end>` is `d$`.
            "0" | "<home>" => (buf.line_start(line), Span::Exclusive),
            "^" => (buf.first_non_blank(line), Span::Exclusive),
            "$" | "<end>" => (
                buf.line_end((line + n - 1).min(buf.last_line())),
                Span::Inclusive,
            ),
            // `w`/`W`, and the one deliberate irregularity in vim's grammar
            // that everybody relies on without noticing: with an operator
            // pending, a `w` that would carry the range onto the next line
            // stops at the end of this one instead. `dw` on the last word of a
            // line deletes the word, not the line break and the indent of
            // whatever followed.
            //
            // Measured from where the *last* step started, so a count that
            // legitimately crosses lines still does — only the final hop is
            // clamped, which is the one that ran out of words.
            "w" | "W" => {
                let big = seq == "W";
                let (mut prev, mut pos) = (cur, cur);
                for _ in 0..n {
                    prev = pos;
                    pos = word_forward(buf, pos, big);
                }
                let mut target = pos;
                if self.pending.op.is_some() {
                    let eol = buf.line_end(buf.line_of(prev));
                    target = target.min(eol.max(cur));
                }
                (target, Span::Exclusive)
            }
            "b" | "B" => (
                (0..n).fold(cur, |p, _| word_backward(buf, p, seq == "B")),
                Span::Exclusive,
            ),
            "e" | "E" => (
                (0..n).fold(cur, |p, _| word_end(buf, p, seq == "E")),
                Span::Inclusive,
            ),
            // Backward to the end of the previous word. `g e` used to mean "the
            // end of the buffer", which is `G`'s job and is not what any vim
            // user pressing `ge` is asking for.
            "g e" | "g E" => (
                (0..n).fold(cur, |p, _| word_end_backward(buf, p, seq == "g E")),
                Span::Inclusive,
            ),
            // Linewise line motions. `_` is "this line", so `d_` is `dd` and
            // `3_` reaches two lines down — the off-by-one is vim's, not a slip.
            "_" => (
                buf.first_non_blank((line + n - 1).min(buf.last_line())),
                Span::Linewise,
            ),
            "+" | "<ret>" => (
                buf.first_non_blank((line + n).min(buf.last_line())),
                Span::Linewise,
            ),
            "-" => (buf.first_non_blank(line.saturating_sub(n)), Span::Linewise),
            // `|` — go to a column, counting from 1.
            "|" => (
                (buf.line_start(line) + n.saturating_sub(1)).min(buf.line_end(line)),
                Span::Exclusive,
            ),
            // The window's top, middle and bottom line. The only motions that
            // ask what is *drawn* rather than what is in the buffer, which is
            // why they read `scroll` and `viewport_lines`.
            "H" | "M" | "L" => {
                let h = self.viewport_lines.max(1);
                let top = self.scroll.min(buf.last_line());
                let bottom = (self.scroll + h - 1).min(buf.last_line());
                let target = match seq {
                    "H" => (top + n - 1).min(bottom),
                    "L" => bottom.saturating_sub(n - 1).max(top),
                    _ => top + (bottom - top) / 2,
                };
                (buf.first_non_blank(target), Span::Linewise)
            }
            // `%` — the other end of the bracket at or just before point.
            // Inclusive, so `d%` takes the pair and everything between it,
            // which is the whole reason anyone uses it with an operator.
            "%" => (buf.matching_bracket(cur)?, Span::Inclusive),
            // `j`/`k` by *visual* row, explicitly, whatever the window is
            // doing — vim's `gj`/`gk`.
            "g j" | "g k" => (
                self.visual_target(seq == "g j", n, self.held_col(self.cursor_vcol())),
                Span::Exclusive,
            ),
            "{" => (buf.line_start(paragraph(buf, line, false)), Span::Linewise),
            "}" => (buf.line_start(paragraph(buf, line, true)), Span::Linewise),
            // `(` and `)` — a sentence at a time. Exclusive, so `d)` takes the
            // sentence and leaves the one after it alone.
            "(" | ")" => {
                let forward = seq == ")";
                let mut at = cur;
                for _ in 0..n {
                    at = sentence_step(buf, at, forward);
                }
                (at, Span::Exclusive)
            }
            // `[[` and `]]` — the next line that starts a top-level form. vim
            // says "a `{` in column 1", which in a file of Lisp is any opener
            // and in Rust is `fn`'s brace: the rule that covers both is a line
            // beginning with something that is not whitespace and is not a
            // closer of what came before.
            "[ [" | "] ]" => {
                let forward = seq == "] ]";
                let mut l = line;
                for _ in 0..n {
                    l = section(buf, l, forward);
                }
                (buf.line_start(l), Span::Exclusive)
            }
            // `n` and `N` are motions, which is what makes `d/foo` spelled
            // `dn` and `cgn`'s cheaper cousin work at all. The direction is the
            // one the search was *started* in, so `?foo` then `n` keeps going
            // backwards.
            //
            // A pattern that does not match answers `None`, and the arm in
            // `builtin` picks the key up and reports why — which is the one
            // thing a motion has no way to say.
            "n" | "N" if !self.last_search.is_empty() => {
                let forward = (seq == "n") != self.search_backward;
                let from = match forward {
                    true => cur + 1,
                    false => cur,
                };
                (
                    self.search_pos(&self.last_search, from, forward)?,
                    Span::Exclusive,
                )
            }
            // A count on `G` is the line to go to rather than a repetition, and
            // its absence is the last line — which is why this reads `count`
            // itself instead of the `n` every other arm takes.
            "G" => {
                let last = buf.last_line();
                let target = match self.pending.count {
                    Some(c) => (c - 1).min(last),
                    None => last,
                };
                (buf.first_non_blank(target), Span::Linewise)
            }
            "g g" => (
                buf.first_non_blank(self.pending.count.unwrap_or(1).saturating_sub(1)),
                Span::Linewise,
            ),
            _ => return None,
        };
        Some(Motion { target, span })
    }

    /// The word under the cursor, for `*` and `#`.
    ///
    /// vim's rule when point is not on a word character: take the next one on
    /// the line. `*` pressed in the indentation searches for the first word of
    /// the line, which is nearly always what was meant.
    fn word_at_point(&self) -> Option<String> {
        let buf = &self.buffer;
        let line = buf.line_of(buf.cursor);
        let eol = buf.line_end(line);
        let mut i = buf.cursor;
        while i < eol && class_at(buf, i, false) != Some(1) {
            i += 1;
        }
        if class_at(buf, i, false) != Some(1) {
            return None;
        }
        let mut start = i;
        while start > 0 && class_at(buf, start - 1, false) == Some(1) {
            start -= 1;
        }
        let mut end = i;
        while end < buf.len_chars() && class_at(buf, end, false) == Some(1) {
            end += 1;
        }
        Some(buf.slice_string(start, end))
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
        // Indentation is a property of a line, so `>` takes whole ones whatever
        // the motion said: `>w` and `>j` both shift lines, which is the only
        // reading either could have. Re-wrapping is the same argument.
        let span = match op {
            Op::Shift(_) | Op::Format(_) => Span::Linewise,
            _ => m.span,
        };
        let m = Motion { span, ..m };
        let (start, end) = match m.span {
            Span::Exclusive => (cur.min(m.target), cur.max(m.target)),
            Span::Inclusive => (
                cur.min(m.target),
                (cur.max(m.target) + 1).min(self.buffer.len_chars()),
            ),
            Span::Linewise => {
                let a = self.buffer.line_of(cur);
                let b = self.buffer.line_of(m.target);
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
        //
        // `"_` is the exception and the reason `black` exists: the black hole
        // swallows the text and leaves *every* register alone, the unnamed one
        // included, which is the whole point of reaching for it.
        let named = self.vim.pending.take();
        let black = named == Some('_');
        let text = self.buffer.slice_string(start, end);
        if let Some(name) = named.filter(|c| !special_register(*c)) {
            self.vim.write(name, text.clone(), linewise);
        }
        if !black {
            self.vim.record(op != Op::Yank, &text, linewise);
            self.set_mark_at('[', start);
            self.set_mark_at(']', end.saturating_sub(1).max(start));
            if op != Op::Yank {
                self.set_mark_at('.', start);
            }
        }
        // Neither `>` nor `gu` nor `gq` touches a register, so the mark is
        // still worth setting for them and the yank is not.
        let yank = match black || matches!(op, Op::Shift(_) | Op::Case(_) | Op::Format(_)) {
            true => Vec::new(),
            false => vec![EditorCommand::Yank {
                start,
                end,
                linewise,
            }],
        };
        match op {
            Op::Yank => {
                let mut cmds = yank;
                cmds.push(EditorCommand::MoveTo(start));
                cmds
            }
            Op::Delete => {
                // A linewise range takes its own terminating newline with it.
                // The last line of a buffer that ends *without* one has none, so
                // that case takes the newline in front instead — otherwise `dd`
                // there leaves the blank line it was supposed to remove.
                //
                // The test is the range's last character and not `end ==
                // len_chars`, which is what it used to be and which is wrong for
                // every file that ends in a newline: `dd` on the last line of
                // "a\nb\n" ate the final newline too and left "a", where vim
                // leaves "a\n".
                let ends_with_newline =
                    end > start && self.buffer.char_at(end - 1) == Some('\n');
                let from = if linewise && !ends_with_newline && start > 0 {
                    start - 1
                } else {
                    start
                };
                let mut cmds = vec![EditorCommand::Checkpoint];
                cmds.extend(yank);
                cmds.extend([
                    EditorCommand::DeleteRange(from, end),
                    EditorCommand::MoveTo(self.after_linewise_delete(linewise, from, start, end)),
                ]);
                cmds
            }
            Op::Change => {
                // `cc`/`cj` keep the line, clearing its contents.
                let (start, end) = if linewise {
                    (start, end.saturating_sub(1))
                } else {
                    (start, end)
                };
                let mut cmds = vec![
                    EditorCommand::Checkpoint,
                    EditorCommand::SetMode(Mode::Insert),
                ];
                if !black {
                    cmds.push(EditorCommand::Yank {
                        start,
                        end,
                        linewise,
                    });
                }
                cmds.extend([
                    EditorCommand::DeleteRange(start, end),
                    EditorCommand::MoveTo(start),
                ]);
                cmds
            }
            // Neither of these touches the register: vim does not clobber `""`
            // with what you indented or lowercased, and reaching for `p` after
            // a `>>` expecting the last yank is exactly why.
            Op::Case(kind) => {
                let text = self.buffer.slice_string(start, end);
                let out: String = match kind {
                    Case::Lower => text.to_lowercase(),
                    Case::Upper => text.to_uppercase(),
                    Case::Toggle => text.chars().flat_map(flip_case).collect(),
                };
                if out == text {
                    return vec![EditorCommand::MoveTo(start)];
                }
                vec![
                    EditorCommand::Checkpoint,
                    EditorCommand::DeleteRange(start, end),
                    EditorCommand::MoveTo(start),
                    EditorCommand::InsertText(out),
                    EditorCommand::MoveTo(start),
                ]
            }
            Op::Shift(right) => self.shift_lines(right, start, end),
            Op::Format(keep) => self.format_lines(start, end, keep),
        }
    }

    /// `gq`/`gw` — re-wrap the lines `[start, end)` touches to the fill column.
    ///
    /// One replacement rather than a line at a time, for [`Editor::substitute`]'s
    /// reason: every command in the batch is measured against the pre-edit
    /// buffer, so a per-line rewrite would aim every command after the first at
    /// offsets that had already moved.
    ///
    /// Paragraphs are what get wrapped, not the range: a blank line inside the
    /// range separates two of them, and each keeps the indent of its own first
    /// line — which is what makes `gqip` work on an indented comment.
    ///
    /// ponytail: no comment-leader continuation, so `gq` over a block of `//`
    /// lines pulls the slashes into the middle of the prose. vim needs
    /// `formatoptions` and a `comments` table for that; this needs a language
    /// and has none.
    fn format_lines(&self, start: usize, end: usize, keep: bool) -> Vec<EditorCommand> {
        let buf = &self.buffer;
        let width = match self.settings.text_width {
            0 => FILL_COLUMN,
            w => w,
        };
        let first = buf.line_of(start);
        let last = buf.line_of(end.saturating_sub(1));
        let (from, to) = (buf.line_start(first), buf.line_end(last));
        let mut out: Vec<String> = Vec::new();
        for para in buf.slice_string(from, to).split('\n').collect::<Vec<_>>().split(|l| l.trim().is_empty()) {
            if para.is_empty() {
                out.push(String::new());
                continue;
            }
            let indent: String = para[0].chars().take_while(|c| c.is_whitespace()).collect();
            let mut line = indent.clone();
            for word in para.iter().flat_map(|l| l.split_whitespace()) {
                let room = line.trim_end().chars().count() + 1 + word.chars().count();
                if line.trim().is_empty() {
                    line.push_str(word);
                } else if room <= width {
                    line.push(' ');
                    line.push_str(word);
                } else {
                    out.push(std::mem::replace(&mut line, format!("{indent}{word}")));
                }
            }
            out.push(line);
            out.push(String::new());
        }
        // `split` yields an empty trailing piece for the blank line each
        // paragraph pushed; the last one has nothing after it to separate.
        while out.last().is_some_and(String::is_empty) {
            out.pop();
        }
        let text = out.join("\n");
        if text == buf.slice_string(from, to) {
            return vec![];
        }
        // `gq` leaves point on the last line it wrapped, `gw` where it was.
        let at = match keep {
            true => self.buffer.cursor.min(from + text.chars().count()),
            false => from + text.chars().count() - out.last().map_or(0, |l| l.chars().count()),
        };
        vec![
            EditorCommand::Checkpoint,
            EditorCommand::DeleteRange(from, to),
            EditorCommand::MoveTo(from),
            EditorCommand::InsertText(text),
            EditorCommand::MoveTo(at),
        ]
    }

    /// `>` and `<` over the lines `[start, end)` touches.
    ///
    /// Written bottom-up for the reason [`Editor::op_block`] is: every command
    /// in the batch is computed against the buffer as it stands *now*, so
    /// re-indenting a later line first leaves every earlier line's offsets
    /// exactly where they were measured.
    ///
    /// ponytail: leading tabs are counted as one column each and rewritten as
    /// spaces, because the indent this inserts is spaces — `set-tab-width`
    /// wide, matching what `<tab>` types in Insert. A file indented with tabs
    /// therefore converts the lines you shift. The upgrade is a
    /// `indent-with-tabs` setting read here and by `insert_key`, on the day
    /// someone is editing a Makefile.
    fn shift_lines(&mut self, right: bool, start: usize, end: usize) -> Vec<EditorCommand> {
        let buf = &self.buffer;
        let width = self.settings.tab_width.max(1);
        let first = buf.line_of(start);
        let last = buf.line_of(end.saturating_sub(1));
        let mut cmds = vec![EditorCommand::Checkpoint];
        let mut landing = buf.line_start(first);
        for line in (first..=last).rev() {
            let ls = buf.line_start(line);
            let text = buf.slice_string(ls, buf.line_end(line));
            // A blank line is left alone, as vim leaves it: indenting nothing
            // produces a line of trailing whitespace and no visible change.
            if text.trim().is_empty() {
                continue;
            }
            let lead = text.chars().take_while(|c| *c == ' ' || *c == '\t').count();
            let want = match right {
                true => lead + width,
                false => lead.saturating_sub(width),
            };
            if line == first {
                landing = ls + want;
            }
            if want == lead {
                continue;
            }
            cmds.push(EditorCommand::DeleteRange(ls, ls + lead));
            cmds.push(EditorCommand::MoveTo(ls));
            cmds.push(EditorCommand::InsertText(" ".repeat(want)));
        }
        if cmds.len() == 1 {
            return vec![];
        }
        // The first non-blank of the first line, which is where vim leaves it —
        // and it is known exactly, because nothing before that line moved.
        cmds.push(EditorCommand::MoveTo(landing));
        cmds
    }

    /// A text object: `iw`, `a(`, `i"`. `(start, end, linewise)`.
    ///
    /// The half of the vim grammar that is not `operator motion` — a range
    /// named by what it *is* rather than by where the cursor would get to — and
    /// the reason `ciw` works from anywhere in a word instead of needing `bce`.
    ///
    /// `i` is the inside, `a` is the inside plus its delimiters: `di(` leaves
    /// the parentheses and `da(` takes them. For a word, `a` takes the trailing
    /// whitespace instead, which is what makes `daw` join two words properly.
    ///
    fn text_object(&self, kind: char, obj: char, n: usize) -> Option<(usize, usize, bool)> {
        let buf = &self.buffer;
        let cur = buf.cursor.min(buf.len_chars().saturating_sub(1));
        let inside = kind == 'i';
        match obj {
            // A word, from wherever in it point is. `n` extends over following
            // words, which is what `d3aw` means.
            'w' | 'W' => {
                let big = obj == 'W';
                let here = class_at(buf, cur, big)?;
                let mut start = cur;
                while start > 0 && class_at(buf, start - 1, big) == Some(here) {
                    start -= 1;
                }
                let mut end = cur;
                let advance = |e: &mut usize| {
                    let c = class_at(buf, *e, big);
                    while *e < buf.len_chars() && class_at(buf, *e, big) == c {
                        *e += 1;
                    }
                };
                advance(&mut end);
                for _ in 1..n {
                    advance(&mut end);
                }
                if !inside {
                    // `aw` takes the whitespace after the word — and, when
                    // there is none because the word ended the line, the
                    // whitespace before it instead. vim's rule, and the one
                    // that makes `daw` on the last word of a line tidy.
                    let was = end;
                    while end < buf.len_chars() && class_at(buf, end, big) == Some(0) {
                        // Never across the line break: `daw` is not `dj`.
                        if buf.char_at(end) == Some('\n') {
                            break;
                        }
                        end += 1;
                    }
                    if end == was {
                        while start > 0 && class_at(buf, start - 1, big) == Some(0) {
                            if buf.char_at(start - 1) == Some('\n') {
                                break;
                            }
                            start -= 1;
                        }
                    }
                }
                Some((start, end, false))
            }
            // A paragraph: the run of non-blank lines around point, or the run
            // of blank ones if that is where point is. Linewise, like `dap`.
            'p' => {
                let line = buf.line_of(cur);
                let blank = |l: usize| buf.line_len(l) == 0;
                let here = blank(line);
                let mut first = line;
                while first > 0 && blank(first - 1) == here {
                    first -= 1;
                }
                let mut last = line;
                while last < buf.last_line() && blank(last + 1) == here {
                    last += 1;
                }
                if !inside {
                    // `ap` reaches on through the blank lines that follow.
                    while last < buf.last_line() && blank(last + 1) != here {
                        last += 1;
                    }
                }
                Some((
                    buf.line_start(first),
                    (buf.line_end(last) + 1).min(buf.len_chars()),
                    true,
                ))
            }
            // A bracketed run. `matching_bracket` does the nesting; all this
            // has to do is find the opener that encloses point when point is
            // not already on one.
            '(' | ')' | 'b' | '[' | ']' | '{' | '}' | 'B' | '<' | '>' => {
                let (open, close) = match obj {
                    '(' | ')' | 'b' => ('(', ')'),
                    '[' | ']' => ('[', ']'),
                    '<' | '>' => ('<', '>'),
                    _ => ('{', '}'),
                };
                let (a, b) = self.enclosing(open, close, cur)?;
                match inside {
                    true if a + 1 >= b => None, // `i()` over an empty pair
                    true => Some((a + 1, b, false)),
                    false => Some((a, b + 1, false)),
                }
            }
            // A quoted run. Not `matching_bracket`'s problem: the two ends are
            // the same character, so there is no nesting to count and the pair
            // is found by scanning the line from its start — which is also what
            // makes `ci"` work with point on either quote or anywhere between.
            '"' | '\'' | '`' => {
                let (a, b) = self.quoted(obj, cur)?;
                match inside {
                    true if a + 1 >= b => None,
                    true => Some((a + 1, b, false)),
                    false => Some((a, b + 1, false)),
                }
            }
            // A sentence. `is` is the sentence, `as` takes the space after it
            // — the same distinction `iw`/`aw` draws, and it is what makes
            // `das` close the gap instead of leaving two spaces behind.
            's' => {
                let all = sentences(buf, cur);
                let i = all.iter().rposition(|&(s, _, _)| s <= cur)?;
                let (start, ..) = all[i];
                let last = all.get(i + n - 1).copied().unwrap_or(*all.last()?);
                Some((start, if inside { last.1 } else { last.2 }, false))
            }
            // A tag, and the reason it is a scanner rather than a bracket
            // match: `<br/>` closes itself, an attribute may hold a `>` inside
            // quotes, and `</div>` pairs with the *matching* `<div>` and not
            // with the nearest one.
            't' => {
                let (os, oe, cs, ce) = self.tag(cur)?;
                match inside {
                    true if oe >= cs => None,
                    true => Some((oe, cs, false)),
                    false => Some((os, ce, false)),
                }
            }
            _ => None,
        }
    }

    /// The innermost element around `pos`, as (open start, open end, close
    /// start, close end) in the half-open way every range here is written.
    ///
    /// ponytail: a scan of the whole buffer per `it`, which is one pass over a
    /// rope for a keystroke a human made. Start it at the enclosing blank line
    /// the day someone edits a one-line minified document.
    fn tag(&self, pos: usize) -> Option<(usize, usize, usize, usize)> {
        let buf = &self.buffer;
        let n = buf.len_chars();
        let mut open: Vec<(String, usize, usize)> = Vec::new();
        let mut i = 0;
        while i < n {
            if buf.char_at(i) != Some('<') {
                i += 1;
                continue;
            }
            // To the `>` that closes *this* tag, which is not the first one:
            // `<a title="a>b">` has one inside a quoted attribute.
            let (mut j, mut quote) = (i + 1, None);
            while j < n {
                match buf.char_at(j) {
                    c if c == quote => quote = None,
                    Some(c @ ('"' | '\'')) if quote.is_none() => quote = Some(c),
                    Some('>') if quote.is_none() => break,
                    _ => {}
                }
                j += 1;
            }
            if j >= n {
                break;
            }
            let body = buf.slice_string(i + 1, j);
            let closing = body.starts_with('/');
            let name: String = body
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':'))
                .collect();
            if !name.is_empty() {
                if closing {
                    if let Some(k) = open.iter().rposition(|(nm, _, _)| *nm == name) {
                        let (_, start, after) = open[k].clone();
                        open.truncate(k);
                        // The innermost enclosing pair is the *first* one to
                        // close around point, so the first hit is the answer.
                        if start <= pos && pos < j + 1 {
                            return Some((start, after, i, j + 1));
                        }
                    }
                } else if !body.ends_with('/') {
                    open.push((name, i, j + 1));
                }
            }
            i = j + 1;
        }
        None
    }

    /// The innermost `open`/`close` pair containing `pos`, as (open, close).
    fn enclosing(&self, open: char, close: char, pos: usize) -> Option<(usize, usize)> {
        let buf = &self.buffer;
        // Already on one end: the matcher answers straight away.
        if buf.char_at(pos) == Some(open) {
            return buf.matching_bracket(pos).map(|b| (pos, b));
        }
        if buf.char_at(pos) == Some(close) {
            return buf.matching_bracket(pos).map(|a| (a, pos));
        }
        // Otherwise walk back for an unclosed opener.
        let mut depth = 0i32;
        let mut i = pos;
        loop {
            match buf.char_at(i) {
                Some(c) if c == close => depth += 1,
                Some(c) if c == open && depth == 0 => {
                    return buf.matching_bracket(i).map(|b| (i, b))
                }
                Some(c) if c == open => depth -= 1,
                _ => {}
            }
            i = i.checked_sub(1)?;
        }
    }

    /// The `q`-delimited run containing `pos`, as (open, close).
    ///
    /// Counted from the start of the line, so which quotes pair with which is
    /// decided the way you read it rather than by whichever is nearest.
    fn quoted(&self, q: char, pos: usize) -> Option<(usize, usize)> {
        let buf = &self.buffer;
        let line = buf.line_of(pos);
        let (ls, le) = (buf.line_start(line), buf.line_end(line));
        let mut open: Option<usize> = None;
        let mut i = ls;
        while i < le {
            // A quote the text escaped is not a delimiter.
            let escaped = i > ls && buf.char_at(i - 1) == Some('\\');
            if buf.char_at(i) == Some(q) && !escaped {
                match open {
                    None => open = Some(i),
                    Some(a) => {
                        if (a..=i).contains(&pos) {
                            return Some((a, i));
                        }
                        open = None;
                    }
                }
            }
            i += 1;
        }
        None
    }

    /// Where the cursor lands after a delete — the *first non-blank* of the
    /// line that moved up into the gap, which is what vim does and what makes
    /// `dd` in indented code leave point on the code rather than in the margin.
    ///
    /// Worked out against the pre-edit buffer because every command in the
    /// returned batch is: `from` is a stable offset (nothing before it moved),
    /// and how many blanks follow it afterwards is read off the text that is
    /// about to slide into that position.
    fn after_linewise_delete(
        &self,
        linewise: bool,
        from: usize,
        start: usize,
        end: usize,
    ) -> usize {
        if !linewise {
            return from;
        }
        if from < start {
            // The last-line case: the range and the newline in front of it both
            // go, so point ends up on the line *before* — which did not move.
            let line = self.buffer.line_of(from);
            return self.buffer.first_non_blank(line);
        }
        // Everything else: the line beginning at `end` is about to begin at
        // `from`, so its own indent is the offset to add.
        let blanks = (end..self.buffer.len_chars())
            .map(|i| self.buffer.text.char(i))
            .take_while(|c| *c == ' ' || *c == '\t')
            .count();
        from + blanks
    }

    /// `J` joins two lines, `{n}J` joins `n`.
    ///
    /// Built as one replacement rather than a loop of deletes: every command in
    /// the returned Vec is computed against the *pre-edit* buffer, so an
    /// iterative version would recompute the same offsets each time round and
    /// join only once however large the count.
    fn join_lines(&self, n: usize, spaced: bool) -> Vec<EditorCommand> {
        let (line, _) = self.buffer.cursor_line_col();
        let last = (line + n.max(2) - 1).min(self.buffer.last_line());
        if last <= line {
            return vec![];
        }
        let start = self.buffer.line_start(line);
        let end = self.buffer.line_end(last);
        let first = self.buffer.slice_string(start, self.buffer.line_end(line));
        // `gJ` keeps the following line's indent as well as skipping the space:
        // it is the join that changes nothing but the line break, which is the
        // only reason to reach for it.
        let joined = (line + 1..=last)
            .map(|l| {
                let from = match spaced {
                    true => self.buffer.first_non_blank(l),
                    false => self.buffer.line_start(l),
                };
                self.buffer.slice_string(from, self.buffer.line_end(l))
            })
            .fold(first.clone(), |acc, rest| match spaced {
                true => format!("{acc} {rest}"),
                false => format!("{acc}{rest}"),
            });
        vec![
            EditorCommand::Checkpoint,
            EditorCommand::DeleteRange(start, end),
            EditorCommand::InsertText(joined),
            // vim leaves the cursor on the first join seam.
            EditorCommand::MoveTo(start + first.chars().count()),
        ]
    }

    /// `C-a` and `C-x` — the number at or after point, `by` bigger or smaller.
    ///
    /// vim's rule for *which* number: the one under the cursor if there is one,
    /// otherwise the next one on this line and no further. Decimal and `0x`
    /// hex, and a leading `-` is part of the number rather than a subtraction —
    /// so `C-x` on `-1` gives `-2`, which is what anyone editing a table means.
    ///
    /// ponytail: no octal, no binary and no `nrformats`. Both are a radix and a
    /// prefix test beside the hex one, on the day someone is editing a file
    /// full of `0o755`.
    fn increment(&mut self, by: i64) -> Vec<EditorCommand> {
        let buf = &self.buffer;
        let line = buf.line_of(buf.cursor);
        let (ls, le) = (buf.line_start(line), buf.line_end(line));
        let dec = |i: usize| buf.char_at(i).is_some_and(|c| c.is_ascii_digit());
        let hexd = |i: usize| buf.char_at(i).is_some_and(|c| c.is_ascii_hexdigit());
        // A run of hex digits is only a number when `0x` introduces it: a bare
        // `abc` is a word.
        let prefixed = |i: usize| {
            i >= ls + 2
                && matches!(buf.char_at(i - 1), Some('x' | 'X'))
                && buf.char_at(i - 2) == Some('0')
        };
        // A *decimal* digit at or after point, which finds `0xff` too, through
        // its `0` — unless point is already among the letters of one, where
        // there is no decimal digit ahead to find and the run has to be picked
        // up by walking back.
        let hex_run = || {
            let mut i = buf.cursor.max(ls);
            while i > ls && hexd(i - 1) {
                i -= 1;
            }
            (buf.cursor < le && hexd(buf.cursor) && prefixed(i)).then_some(i)
        };
        let Some(seed) = (buf.cursor.max(ls)..le).find(|&i| dec(i)).or_else(hex_run) else {
            return vec![EditorCommand::Message("no number here".into())];
        };
        // Two ways this is hex: point is among the digits of one, or point is
        // on the `0` that introduces it.
        let mut left = seed;
        while left > ls && hexd(left - 1) {
            left -= 1;
        }
        let (mut start, hex) = match (
            prefixed(left),
            buf.char_at(seed) == Some('0')
                && matches!(buf.char_at(seed + 1), Some('x' | 'X'))
                && hexd(seed + 2),
        ) {
            (true, _) => (left, true),
            (false, true) => (seed + 2, true),
            (false, false) => (seed, false),
        };
        if !hex {
            while start > ls && dec(start - 1) {
                start -= 1;
            }
        }
        let mut end = start;
        while end < le && (if hex { hexd(end) } else { dec(end) }) {
            end += 1;
        }
        // A `-` in front is the number's sign and not an operator: `C-x` on
        // `-1` has to give `-2`, which is what anyone editing a table means.
        let negative = !hex && start > ls && buf.char_at(start - 1) == Some('-');
        let from = if negative { start - 1 } else { start };
        let text = buf.slice_string(start, end);
        let Ok(value) = i64::from_str_radix(&text, if hex { 16 } else { 10 }) else {
            return vec![EditorCommand::Message(format!("number too large: {text}"))];
        };
        let next = if negative { -value } else { value }.wrapping_add(by);
        // A number written with leading zeroes keeps its width, which is what
        // makes `C-a` usable on `009` and on a padded identifier.
        let width = match text.starts_with('0') && text.len() > 1 {
            true => text.len(),
            false => 0,
        };
        let out = match (hex, next < 0) {
            (true, _) => format!("{:0width$x}", next.max(0)),
            (false, true) => format!("-{:0width$}", -next),
            (false, false) => format!("{next:0width$}"),
        };
        // Written *before* the old digits are removed, and that order is not a
        // preference: in Normal mode the cursor clamps to the last character of
        // its line, so a `MoveTo` aimed at a gap the delete has just opened at
        // the end of a line lands one to the left of where it was asked for.
        let grew = out.chars().count();
        let at = from + grew;
        vec![
            EditorCommand::Checkpoint,
            EditorCommand::MoveTo(from),
            EditorCommand::InsertText(out),
            EditorCommand::DeleteRange(from + grew, end + grew),
            // vim leaves point on the number's last digit.
            EditorCommand::MoveTo(at.saturating_sub(1)),
        ]
    }

    /// `C-f`/`C-b` — a screenful, less the two lines of overlap vim keeps so
    /// that the line you were reading is still on the new screen.
    fn scroll_page(&mut self, down: bool, n: usize) -> Vec<EditorCommand> {
        let step = self.viewport_lines.saturating_sub(2).max(1) * n;
        let (line, col) = self.buffer.cursor_line_col();
        let target = match down {
            true => (line + step).min(self.buffer.last_line()),
            false => line.saturating_sub(step),
        };
        self.scroll = match down {
            true => self.scroll + step,
            false => self.scroll.saturating_sub(step),
        };
        vec![EditorCommand::MoveTo(
            self.buffer.line_start(target) + col.min(self.buffer.line_len(target)),
        )]
    }

    /// `C-e`/`C-y` — the *view* moves a line; point comes along only when the
    /// view would otherwise leave it behind.
    fn scroll_line(&mut self, down: bool, n: usize) -> Vec<EditorCommand> {
        let h = self.viewport_lines.max(1);
        self.scroll = match down {
            true => (self.scroll + n).min(self.buffer.last_line()),
            false => self.scroll.saturating_sub(n),
        };
        let (line, col) = self.buffer.cursor_line_col();
        let target = line.clamp(self.scroll, (self.scroll + h - 1).min(self.buffer.last_line()));
        if target == line {
            return vec![];
        }
        vec![EditorCommand::MoveTo(
            self.buffer.line_start(target) + col.min(self.buffer.line_len(target)),
        )]
    }

    fn scroll_half(&mut self, down: bool) -> Vec<EditorCommand> {
        let half = (self.viewport_lines / 2).max(1);
        let (line, col) = self.buffer.cursor_line_col();
        let target = if down {
            (line + half).min(self.buffer.last_line())
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
                    // A mark is a jump, so where you *were* goes on the list —
                    // and only after the target has been read, because `` `` ``
                    // reads the very entry this is about to push and would
                    // otherwise answer with the place you are standing.
                    Some(m) => {
                        if op.is_none() {
                            self.push_jump();
                        }
                        self.resolve(op, m)
                    }
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
            // ...and avy, for ace's reason: `q` aimed at a label is a label.
            && self.grab_key.is_none()
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

    /// `@a`, and `@@` for whatever ran last. The keys go back through
    /// [`Editor::run_keys`], which is where the replay is explained.
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
        let out = self.run_keys(&keys, count);
        self.vim.depth -= 1;
        out
    }

    /// `ma`. A *marker*, so the mark still names its character after an edit
    /// above it — which is the whole difference between this and remembering an
    /// offset.
    fn set_mark(&mut self, name: char) -> Vec<EditorCommand> {
        self.set_mark_at(name, self.buffer.cursor);
        vec![EditorCommand::Message(format!("mark {name} set"))]
    }

    /// The same, at a given offset and without the report — which is what the
    /// marks vim sets *for* you need: `` `[ `` and `` `] `` bracket the last
    /// change, `` `. `` is where it started and `` `^ `` is where the last
    /// insert ended. Ordinary entries in the same map, because the only thing
    /// unusual about them is who presses `m`.
    fn set_mark_at(&mut self, name: char, at: usize) {
        let slot = (self.buffer.id, name);
        // Replacing a mark frees the marker it used, or `ma` in a loop leaves
        // one dead marker per press in the buffer for `splice` to walk.
        if let Some(old) = self.vim.marks.remove(&slot) {
            self.delete_marker(old);
        }
        let id = self.make_marker(at.min(self.buffer.len_chars()), Insertion::Stay);
        self.vim.marks.insert(slot, id);
    }

    /// Where a mark is, letters and the ones vim writes itself alike.
    ///
    /// `'<` and `'>` come out of the last selection and `` ` `` out of the jump
    /// list rather than out of the map, because both of those already exist and
    /// a second copy is a second thing to keep in step.
    fn mark_at(&self, name: char) -> Option<usize> {
        match name {
            '<' => self.vim.last_selection.map(|(a, _)| a),
            '>' => self
                .vim
                .last_selection
                .map(|(_, b)| b.saturating_sub(1).min(self.buffer.len_chars())),
            // `` ` `` and `''` — where you were before the last jump, which is
            // the entry `C-o` would take you to. Read *before* the jump this
            // motion is itself about to file, which is what makes pressing it
            // twice a round trip rather than a fixpoint.
            '`' | '\'' => self.next_jump(self.vim.jump_at, true).map(|(_, pos)| pos),
            _ => self
                .vim
                .marks
                .get(&(self.buffer.id, name))
                .copied()
                .and_then(|id| self.marker_position(id)),
        }
    }

    /// File the cursor on the jump list, so `C-o` can come back to it.
    ///
    /// Everything ahead of where `C-o` has already walked is dropped, exactly
    /// as vim drops it: a jump made from halfway back is a new future, and the
    /// one you were retracing is gone.
    fn push_jump(&mut self) {
        let dropped = self.vim.jumps.split_off(self.vim.jump_at.min(self.vim.jumps.len()));
        for (_, id) in dropped {
            self.delete_marker(id);
        }
        let id = self.make_marker(self.buffer.cursor, Insertion::Stay);
        self.vim.jumps.push((self.buffer.id, id));
        if self.vim.jumps.len() > JUMP_LIMIT {
            let (_, old) = self.vim.jumps.remove(0);
            self.delete_marker(old);
        }
        self.vim.jump_at = self.vim.jumps.len();
    }

    /// The next jump-list entry from `at`, walking back or forward, as
    /// (index, offset). Entries belonging to another buffer — or to one that
    /// has since been closed — are stepped over rather than landed on, which
    /// is the whole of "the list is per editor and a marker is per buffer".
    fn next_jump(&self, mut at: usize, back: bool) -> Option<(usize, usize)> {
        loop {
            at = match back {
                true => at.checked_sub(1)?,
                false => at + 1,
            };
            let &(id, marker) = self.vim.jumps.get(at)?;
            if id == self.buffer.id {
                if let Some(pos) = self.marker_position(marker) {
                    return Some((at, pos));
                }
            }
        }
    }

    /// `C-o` and `C-i`.
    fn jump_walk(&mut self, back: bool, n: usize) -> Vec<EditorCommand> {
        // Standing at the newest entry, `C-o` has to file where we are or
        // `C-i` has nothing to come back to — and the index then names that
        // new entry rather than sitting past it.
        if back && self.vim.jump_at == self.vim.jumps.len() {
            self.push_jump();
            self.vim.jump_at -= 1;
        }
        let (mut at, mut pos) = (self.vim.jump_at, None);
        for _ in 0..n {
            match self.next_jump(at, back) {
                Some((a, p)) => (at, pos) = (a, Some(p)),
                None => break,
            }
        }
        match pos {
            Some(p) => {
                self.vim.jump_at = at;
                vec![EditorCommand::MoveTo(p)]
            }
            None => vec![EditorCommand::Message(
                match back {
                    true => "no older jump",
                    false => "no newer jump",
                }
                .into(),
            )],
        }
    }

    /// `` `a `` is the exact position, `'a` is the line — vim's distinction,
    /// and the reason they are two keys rather than one.
    ///
    /// ponytail: no uppercase file-marks. Those name a position in a buffer you
    /// are not in, which is a cross-buffer marker table and a switch on the way
    /// to the motion — a different feature wearing this one's keys.
    fn mark_motion(&self, linewise: bool, name: char) -> Option<Motion> {
        let at = self.mark_at(name)?;
        Some(match linewise {
            true => {
                let line = self.buffer.line_of(at);
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
    fn paste_cmds(&mut self, after: bool, n: usize) -> Vec<EditorCommand> {
        let name = self.vim.pending.take();
        // `"_p` pastes nothing — the black hole is empty by definition — and
        // `"+`/`"*` are the unnamed register, which *is* the system clipboard
        // here, so neither needs the swap below.
        let unnamed = || {
            let mut cmds = vec![EditorCommand::Checkpoint];
            cmds.extend(std::iter::repeat_n(EditorCommand::Paste { after }, n));
            cmds
        };
        let Some(name) = name.filter(|c| !special_register(*c)) else {
            return match name {
                Some('_') => vec![],
                _ => unnamed(),
            };
        };
        let Some((text, linewise)) = self.register_text(name) else {
            return vec![EditorCommand::Message(format!("register {name} is empty"))];
        };
        let mut cmds = vec![
            EditorCommand::Checkpoint,
            EditorCommand::SetRegister { text, linewise },
        ];
        cmds.extend(std::iter::repeat_n(EditorCommand::Paste { after }, n));
        // ...and put back what `""` held: vim leaves the unnamed register alone
        // when you name another one.
        cmds.push(EditorCommand::SetRegister {
            text: self.register.clone(),
            linewise: self.register_linewise,
        });
        cmds
    }

    /// What a register holds, including the three vim fills from somewhere
    /// else: the last search, the last `:` line and the last insert.
    ///
    /// Answered here rather than kept in the map because all three already live
    /// on the editor, and a second copy is a second thing to keep in step.
    fn register_text(&self, name: char) -> Option<(String, bool)> {
        match name {
            '/' => Some((self.last_search.clone(), false)),
            ':' => Some((self.vim.last_ex.clone(), false)),
            '.' => Some((self.vim.inserted.clone(), false)),
            '%' => Some((self.buffer.name(), false)),
            _ => self.vim.read(name).cloned(),
        }
        .filter(|(t, _)| !t.is_empty())
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

    /// The pattern the *hits* are drawn from, which is not always the one `n`
    /// repeats.
    ///
    /// While a `/` prompt is open it is what has been typed so far, so the file
    /// lights up as you type and you can see how much a pattern catches before
    /// committing to it — which is the half of incremental search this editor
    /// had no way to show. Otherwise it is `last_search`, so the hits stay up
    /// after Enter: vim's `hlsearch`, and `:noh` is the way out, which is what
    /// its arm in `ex_command` has always said it was clearing.
    fn highlight_pattern(&self) -> &str {
        match self.prompt.as_ref() {
            Some(p) if p.kind == PromptKind::Search => &p.text,
            _ => &self.last_search,
        }
    }

    /// Every search hit in `buf` between two buffer lines, as char ranges.
    ///
    /// **Bounded by the window on purpose.** The whole-buffer answer is what an
    /// index would be, and `HlKind::Match`'s own doc said an index was what this
    /// wanted — but the reason it said so was that the alternative on offer was
    /// *one overlay per hit*, and `overlays_for_line` is a linear scan per drawn
    /// line. A screenful of text scanned once per pane is neither: it is the
    /// same shape as the highlight spans beside it, it cannot grow with the
    /// file, and there is nothing to keep in step with an edit because it is
    /// recomputed from the text every time it is asked for.
    ///
    /// `last` is exclusive and clamped, so a pane whose rows outrun the buffer
    /// asks about lines that are not there and gets nothing rather than a panic.
    ///
    /// ponytail: the pattern is compiled per call, which is once per pane per
    /// drawn frame. Tens of microseconds against a draw measured in
    /// milliseconds, and the idle loop no longer draws at all — cache it on the
    /// editor beside `last_search` if a profile ever disagrees.
    pub fn search_hits(
        &self,
        buf: &crate::Buffer,
        first: usize,
        last: usize,
    ) -> Vec<(usize, usize)> {
        let pat = self.highlight_pattern();
        if pat.is_empty() {
            return Vec::new();
        }
        let Some(re) = compile(pat, false) else {
            return Vec::new();
        };
        let lines = buf.len_lines();
        if first >= lines {
            return Vec::new();
        }
        let from = buf.line_start(first);
        let to = match last >= lines {
            true => buf.len_chars(),
            false => buf.line_start(last),
        };
        let text = buf.slice_string(from, to);
        // One walk for the whole window rather than a byte-to-char conversion
        // per hit: matches come out left to right and non-overlapping, so the
        // count only ever moves forward.
        let mut hits = Vec::new();
        let (mut byte, mut chars) = (0usize, 0usize);
        for m in re.find_iter(&text) {
            // A pattern that can match nothing — `x*` against a line with no
            // `x` — reports a hit at every position. Painting a zero-width
            // band draws nothing and costs a fill per character.
            if m.start() == m.end() {
                continue;
            }
            chars += text[byte..m.start()].chars().count();
            byte = m.start();
            let s = chars;
            chars += text[byte..m.end()].chars().count();
            byte = m.end();
            hits.push((from + s, from + chars));
        }
        hits
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
        // Read before the prompt is borrowed, and only for the one key that
        // wants it: a `String` clone per keystroke would be a copy of the
        // clipboard on every letter typed into a prompt.
        let paste = matches!(key, Key::Ctrl('y')).then(|| self.register.replace('\n', " "));
        // Before the borrow below, because walking the history reads a field of
        // the editor the prompt does not own. `M-p`/`M-n` rather than the arrows
        // for the reason Emacs uses them: up and down already move the
        // selection, and a completing prompt needs both gestures at once.
        match key {
            Key::Meta('p') => return self.walk_history(1),
            Key::Meta('n') => return self.walk_history(-1),
            _ => {}
        }
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
            // `M-<bs>`, the one thing a path prompt needs that a plain backspace
            // cannot do: separators first, then the word itself, so `~/src/doc`
            // goes to `~/src/` and then to `~/` rather than a character a press.
            // Empty cancels, exactly as `<bs>` does — a delete key at an empty
            // prompt means "I did not want this prompt".
            Key::MetaBackspace => {
                if p.text.is_empty() {
                    return self.cancel_prompt();
                }
                let word = |c: char| c.is_alphanumeric() || c == '_';
                while p.text.chars().next_back().is_some_and(|c| !word(c)) {
                    p.text.pop();
                }
                while p.text.chars().next_back().is_some_and(word) {
                    p.text.pop();
                }
                p.refilter();
            }
            Key::Char(c) => {
                p.text.push(c);
                p.refilter();
            }
            // Paste. The unnamed register *is* the system clipboard here (see
            // `Editor::register`), so this is `C-v` as well without knowing it —
            // which is the point: a URL to clone, a path to open, a pattern to
            // search for all arrive from somewhere else, and retyping one was
            // the only way to get it into a prompt.
            //
            // A prompt is one line, so a linewise register's newlines are
            // dropped rather than truncating at the first: pasting three yanked
            // lines into `:` should give you all three words, not the first one
            // and a silent loss.
            Key::Ctrl('y') => {
                p.text.push_str(paste.unwrap_or_default().trim_end());
                p.refilter();
            }
            // `C-j`/`C-k` alongside `C-n`/`C-p`: both spellings are muscle
            // memory depending on which completion UI you came from.
            Key::Ctrl('n') | Key::Ctrl('j') | Key::Down | Key::ShiftDown => p.next(),
            Key::Ctrl('p') | Key::Ctrl('k') | Key::Up | Key::ShiftUp => p.prev(),
            Key::Tab => p.complete(),
            Key::Enter => return self.accept_prompt(),
            _ => return vec![],
        }
        self.preview()
    }

    /// What a newline typed at point should put on the line it opens.
    ///
    /// Measured from the line point is *on*, and only when there is nothing but
    /// whitespace ahead of it on that line — splitting a line in the middle is
    /// not opening a block under it, and pushing the tail of `foo(bar)` across
    /// and then indenting it is a surprise nobody asked for. Emacs' own
    /// `electric-indent` draws the line in the same place.
    fn indent_for_next_line(&self) -> String {
        let (line, _) = self.buffer.cursor_line_col();
        let rest = self
            .buffer
            .slice_string(self.buffer.cursor, self.buffer.line_end(line));
        match rest.trim().is_empty() {
            false => String::new(),
            true => self.buffer.indent_after(
                line,
                &self.settings.indent_openers,
                self.settings.tab_width,
            ),
        }
    }

    /// File an accepted answer under its prompt's kind, for `M-p` to find.
    ///
    /// A repeat moves to the front rather than being appended, which is what
    /// keeps the ring useful: the half-dozen commands anyone actually runs
    /// would otherwise push everything else out within a session, and `M-p M-p`
    /// would walk through the same name three times.
    fn remember_answer(&mut self, p: &crate::minibuffer::Prompt) {
        let entry = p.submitted();
        if entry.is_empty() {
            return;
        }
        self.prompt_history
            .retain(|(k, e)| *k != p.kind || *e != entry);
        self.prompt_history.push((p.kind, entry));
        if self.prompt_history.len() > crate::HISTORY_LIMIT {
            self.prompt_history.remove(0);
        }
    }

    /// The answers given to prompts of `kind`, newest first.
    fn history_for(&self, kind: PromptKind) -> Vec<&str> {
        self.prompt_history
            .iter()
            .rev()
            .filter(|(k, _)| *k == kind)
            .map(|(_, e)| e.as_str())
            .collect()
    }

    /// `M-p`/`M-n`: walk this kind's history, `by` entries back or forward.
    ///
    /// Position `0` is what you had typed, kept in `stash`, so walking forward
    /// off the end gives back the filter you were narrowing with instead of
    /// stranding you on the oldest thing you ever ran.
    fn walk_history(&mut self, by: isize) -> Vec<EditorCommand> {
        let Some(kind) = self.prompt.as_ref().map(|p| p.kind) else {
            return vec![];
        };
        let past: Vec<String> = self.history_for(kind).iter().map(|s| s.to_string()).collect();
        let Some(p) = self.prompt.as_mut() else {
            return vec![];
        };
        if past.is_empty() {
            return vec![];
        }
        if p.history == 0 {
            p.stash = p.text.clone();
        }
        let want = (p.history as isize + by).clamp(0, past.len() as isize) as usize;
        if want == p.history {
            return vec![];
        }
        p.history = want;
        let entry = match want {
            0 => p.stash.clone(),
            n => past[n - 1].clone(),
        };
        p.recall(entry);
        self.preview()
    }

    /// Enter: what the prompt was asking for decides what happens.
    fn accept_prompt(&mut self) -> Vec<EditorCommand> {
        let Some(p) = self.prompt.take() else {
            return vec![];
        };
        self.remember_answer(&p);
        match p.kind {
            PromptKind::Ex => {
                // `":` holds it, and only the line you *typed* — a `:g` that
                // runs `:d` a hundred times must not leave `d` there.
                self.vim.last_ex = p.text.clone();
                self.ex_command(&p.text)
            }
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
                //
                // `?` searches backwards, which it used to only *repeat*: the
                // jump itself went forwards, so the first hit of a `?` was the
                // one in front of you.
                let (from, forward) = match self.search_backward {
                    true => (origin, false),
                    false => (origin + 1, true),
                };
                let Some(op) = self.vim.search_op.take() else {
                    return self.search_from(from, forward);
                };
                // `d/foo`. The preview has been dragging the cursor around, so
                // the operator is measured from where the search *began* — put
                // point back there before anything reads it.
                self.apply(EditorCommand::MoveTo(origin));
                let pat = self.last_search.clone();
                match self.search_pos(&pat, from, forward) {
                    Some(at) => self.resolve(
                        Some(op),
                        Motion {
                            target: at,
                            span: Span::Exclusive,
                        },
                    ),
                    None => vec![EditorCommand::Message(format!("pattern not found: {pat}"))],
                }
            }
            PromptKind::Command => {
                // `submitted` and not `value`: the image pads a docstring and a
                // key onto the row — see `%annotated-command` in
                // `runtime/modes/which-key.lisp` — and only the first word is
                // the command. That is what lets the annotation be plain
                // readable text rather than a Lisp block comment smuggled onto
                // the line to keep the whole candidate callable, and it puts an
                // annotated `project-find-file` back in front of the `project-`
                // arm in `run_action` instead of past it.
                let name = p.submitted();
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
            PromptKind::Buffer | PromptKind::Line => self.candidate_target(&p),
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
            // Anything that is not `yes` is no. Emacs' `yes-or-no-p` rejects
            // and re-asks instead, which is the one behaviour worth *not*
            // copying: the answer to a question guarding a discarded rebase
            // should never be one keystroke away from being retyped.
            PromptKind::Confirm => match self.pending_confirm.take() {
                Some(cmd) if p.text.trim().eq_ignore_ascii_case("yes") => vec![*cmd],
                Some(_) => vec![EditorCommand::Message("cancelled".into())],
                // Nothing parked: the prompt outlived its command, which can
                // only happen if something else cleared the slot. Doing nothing
                // is the safe half of that.
                None => vec![],
            },
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
        // Escape on a confirmation is "no". Dropped here rather than left for
        // the next `confirm` to overwrite, because a parked command that
        // outlives its question is a discarded rebase waiting for an unrelated
        // Enter. The `d` of an abandoned `d/` goes for the same reason.
        self.pending_confirm = None;
        self.vim.search_op = None;
        // Back to the buffer you were in *before* the cursor, because the
        // cursor offset means nothing until the right document is live again.
        let mut out: Vec<EditorCommand> = p
            .origin_buffer
            .into_iter()
            .map(EditorCommand::SwitchBufferId)
            .chain(p.origin.map(EditorCommand::MoveTo))
            .collect();
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
            let (from, forward) = match self.search_backward {
                true => (origin, false),
                false => (origin + 1, true),
            };
            let at = (!pat.is_empty())
                .then(|| self.search_pos(&pat, from, forward))
                .flatten();
            return vec![EditorCommand::MoveTo(at.unwrap_or(origin))];
        }
        self.candidate_target(p)
    }

    /// Where the highlighted candidate takes you: the buffer it names, or the
    /// line it is.
    ///
    /// One function because `preview` and `accept_prompt` have to agree about
    /// it — the whole promise of a preview is that Enter lands where you are
    /// already looking, and two copies of "which candidate is that, and what
    /// does it mean" is two chances for them not to.
    fn candidate_target(&self, p: &Prompt) -> Vec<EditorCommand> {
        let Some(&i) = p.matches.get(p.selected) else {
            return vec![];
        };
        match p.kind {
            // By id and never by index: switching *reorders* the editor's list,
            // so the position this candidate had when the prompt opened stopped
            // being true the first time the preview moved. Nothing else in the
            // prompt changes — the candidate list is not rebuilt, so the names
            // stay where they are underneath.
            PromptKind::Buffer => match p.ids.get(i) {
                Some(&id) => vec![EditorCommand::SwitchBufferId(id)],
                None => vec![],
            },
            // The image asked to be told, so tell it — the *text* of the
            // candidate, which is the only thing Lisp gave core in the first
            // place. What previewing one means is decided there.
            //
            // Deliberately not `p.text`: the preview follows the highlight,
            // not what has been typed, which is what makes holding `C-n` walk
            // a theme list the way it walks the buffer list.
            PromptKind::Lisp { id, .. } => match p.items.get(i) {
                Some(item) => vec![EditorCommand::CallLisp(format!(
                    "(%prompt-preview {id} {})",
                    crate::query::lisp_string(item)
                ))],
                None => vec![],
            },
            // Items are one per line, in order, so the item index *is* the line
            // number — no parsing back out of the rendered text.
            _ => vec![EditorCommand::MoveTo(self.buffer.first_non_blank(i))],
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

    /// Ask a yes-or-no question, and run `on_yes` if the answer is `yes`.
    ///
    /// The caller is whoever is about to do something that can lose work, and it
    /// parks the *already-guarded* form of its own action — see
    /// [`EditorCommand::Confirmed`] for why that is a wrapper rather than a flag.
    ///
    /// Not part of [`Editor::open_prompt`]: that builds a prompt from its kind
    /// alone, and this one needs both a question and a command that only the
    /// caller knows.
    pub fn confirm(&mut self, question: &str, on_yes: EditorCommand) {
        self.pending_confirm = Some(Box::new(on_yes));
        self.prompt = Some(Prompt::new(
            PromptKind::Confirm,
            &format!("{question} (yes or no) "),
            Vec::new(),
        ));
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
            PromptKind::Buffer => ("Buffer: ", self.buffer_candidates()),
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
                let n = self.buffer.last_line() + 1;
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
            // Likewise: its label is the question, and there is no question
            // without the command it guards. `Editor::confirm` is the door.
            PromptKind::Confirm => ("", Vec::new()),
        };
        let mut prompt = Prompt::new(kind, label, items);
        prompt.origin = kind.previews().then_some(self.buffer.cursor);
        // What has been chosen here before, newest first. It is what `M-p`
        // walks *and* an input to the ranking — see `Prompt::recent` — so it is
        // handed over on the way in rather than looked up per keystroke.
        prompt.recent = self
            .history_for(kind)
            .into_iter()
            .map(str::to_string)
            .collect();
        // The switcher previews by *showing* the highlighted buffer, so it needs
        // stable handles for the candidates and one for the way back. Captured
        // here, with the list, because this is the only moment the two are known
        // to correspond — switching reorders them.
        if kind == PromptKind::Buffer {
            prompt.ids = self.buffer_ids();
            prompt.origin_buffer = Some(self.buffer.id);
            // The live buffer goes to the back. It is the one already on screen,
            // so offering it first makes the switcher's default answer "stay
            // here" — a keystroke that does nothing. Emacs' `C-x b` has always
            // defaulted to the *other* buffer, and the rest of the list is
            // already most-recently-used, so rotating by one puts the buffer you
            // came from under the cursor and leaves the order otherwise intact.
            //
            // Both lists, together, or the ids stop naming the rows.
            if !prompt.items.is_empty() {
                prompt.items.rotate_left(1);
                prompt.ids.rotate_left(1);
            }
        }
        prompt.refilter();
        // The width of the number and the two spaces after it, matching the
        // `format!` above. Told to the prompt rather than re-derived from a
        // row, so the one place that decides the layout is the one that wrote
        // it — see `Prompt::prefix`.
        if kind == PromptKind::Line {
            prompt.prefix = self.buffer.len_lines().to_string().len() + 2;
        }
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
    fn substitute(&mut self, rest: &str, lines: (usize, usize)) -> Option<Vec<EditorCommand>> {
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

        let (first, last) = lines;
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

    /// An inclusive line range as the char range an operator wants — the whole
    /// of the last line, its newline included, exactly as `dd` takes it.
    fn line_bounds(&self, (first, last): (usize, usize)) -> (usize, usize) {
        (
            self.buffer.line_start(first),
            (self.buffer.line_end(last) + 1).min(self.buffer.len_chars()),
        )
    }

    /// `:m` and `:t` — move or copy the range to just after `dest`.
    ///
    /// One `InsertText` and at most one `DeleteRange`, and the delete goes
    /// *first* when it is above the destination: every command in a batch is
    /// measured against the pre-edit buffer, so the two orders are not the same
    /// batch and only one of them lands the text where it was asked for.
    fn ex_move(&mut self, lines: (usize, usize), dest: &str, cut: bool) -> Vec<EditorCommand> {
        let Some((to, _)) = self.ex_addr(dest.trim()) else {
            return vec![EditorCommand::Message(format!("bad address: {dest}"))];
        };
        // `:m0` is "to the very top", which is the one address that is *before*
        // a line rather than after it.
        let above = dest.trim() == "0";
        let (start, end) = self.line_bounds(lines);
        if cut && (lines.0..=lines.1).contains(&to) {
            return vec![EditorCommand::Message("cannot move a range into itself".into())];
        }
        let text = self.buffer.slice_string(start, end);
        let at = match above {
            true => 0,
            false => self.line_bounds((to, to)).1,
        };
        let mut cmds = vec![EditorCommand::Checkpoint];
        // Below the range: delete first, and the destination slides up by what
        // the delete removed. Above it: insert first, for the mirror reason.
        if cut && at > start {
            cmds.push(EditorCommand::DeleteRange(start, end));
            cmds.push(EditorCommand::MoveTo(at - (end - start)));
            cmds.push(EditorCommand::InsertText(text));
            cmds.push(EditorCommand::MoveTo(at - (end - start)));
        } else {
            cmds.push(EditorCommand::MoveTo(at));
            cmds.push(EditorCommand::InsertText(text.clone()));
            if cut {
                cmds.push(EditorCommand::DeleteRange(
                    start + text.chars().count(),
                    end + text.chars().count(),
                ));
            }
            cmds.push(EditorCommand::MoveTo(at));
        }
        cmds
    }

    /// `count` lines starting at the cursor's, as an inclusive line range.
    fn line_range(&self, count: usize) -> (usize, usize) {
        let (line, _) = self.buffer.cursor_line_col();
        let last = self.buffer.last_line();
        (line, (line + count.max(1) - 1).min(last))
    }

    /// The inclusive line range of the visual selection, if there is one.
    fn selection_lines(&self) -> Option<(usize, usize)> {
        let (a, b) = self.selection()?;
        Some((self.buffer.line_of(a), self.buffer.line_of(b.saturating_sub(1))))
    }

    /// One ex address — `.`, `$`, a number, `'a`, and `+n`/`-n` off any of
    /// them. Answers the line and what is left of the string.
    ///
    /// ponytail: no `/pat/` address. It is the one form that needs the search
    /// engine rather than arithmetic, and `:g` covers what people reach for it
    /// to do.
    fn ex_addr<'a>(&self, s: &'a str) -> Option<(usize, &'a str)> {
        let here = self.buffer.line_of(self.buffer.cursor);
        let (mut line, mut rest) = match s.chars().next()? {
            '.' => (here, &s[1..]),
            '$' => (self.buffer.last_line(), &s[1..]),
            '\'' => {
                let name = s[1..].chars().next()?;
                let at = self.mark_at(name)?;
                (self.buffer.line_of(at), &s[1 + name.len_utf8()..])
            }
            c if c.is_ascii_digit() => {
                let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
                // Ex counts from 1 and everything here counts from 0. `:0` is
                // vim's "before the first line", which as a *line* is the first.
                let n: usize = s[..digits].parse().ok()?;
                (n.saturating_sub(1), &s[digits..])
            }
            // A bare `+3` or `-2` is measured from the current line.
            '+' | '-' => (here, s),
            _ => return None,
        };
        while let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
            let tail = &rest[1..];
            let digits = tail.len() - tail.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            let by: usize = tail[..digits].parse().unwrap_or(1);
            line = match sign {
                '+' => line + by,
                _ => line.saturating_sub(by),
            };
            rest = &tail[digits..];
        }
        Some((line.min(self.buffer.last_line()), rest))
    }

    /// `[range]` in front of an ex command, as an inclusive pair of lines and
    /// the rest of the line. `None` means there was no range at all, which is
    /// not the same as an empty one: `:d` deletes the current line and `:%d`
    /// the buffer, and only the second of those is a range.
    fn ex_range<'a>(&self, s: &'a str) -> (Option<(usize, usize)>, &'a str) {
        if let Some(rest) = s.strip_prefix('%') {
            return (Some((0, self.buffer.last_line())), rest);
        }
        // `'<,'>`, which `:` in visual mode types for you. Answered from the
        // selection rather than through `ex_addr` twice, because the marks are
        // set on the way *out* of visual mode and this is read on the way in.
        if let Some(rest) = s.strip_prefix("'<,'>") {
            return (self.selection_lines().or(self.vim.last_selection.map(|(a, b)| {
                (
                    self.buffer.line_of(a),
                    self.buffer.line_of(b.saturating_sub(1)),
                )
            })), rest);
        }
        let Some((first, rest)) = self.ex_addr(s) else {
            return (None, s);
        };
        match rest.strip_prefix(',') {
            Some(tail) => match self.ex_addr(tail) {
                Some((last, rest)) => (Some((first.min(last), first.max(last))), rest),
                None => (Some((first, first)), tail),
            },
            None => (Some((first, first)), rest),
        }
    }

    /// `:[range]g/pat/cmd` — run `cmd` on every line the pattern matches, and
    /// `:v` on every one it does not.
    ///
    /// Bottom-up, and applying as it goes: `cmd` is nearly always `d`, so every
    /// line after the one just handled has moved. Working backwards means no
    /// line has moved by the time its turn comes, and it is the same argument
    /// [`Editor::op_block`] makes about a rectangle.
    ///
    /// This is the second place in the file that applies rather than returning
    /// — see [`Editor::run_keys`] for the first, and for why that is allowed:
    /// `apply` is still the only writer, with the loop driven from here.
    fn ex_global(
        &mut self,
        (first, last): (usize, usize),
        rest: &str,
        invert: bool,
    ) -> Vec<EditorCommand> {
        let mut head = rest.chars();
        let Some(delim) = head.next().filter(|c| !c.is_alphanumeric() && !c.is_whitespace()) else {
            return vec![EditorCommand::Message("usage: :g/pattern/command".into())];
        };
        let parts = split_delim(&rest[delim.len_utf8()..], delim);
        let pat = match parts.first().map_or("", |s| s.as_str()) {
            "" => self.last_search.clone(),
            p => p.to_string(),
        };
        let Some(re) = compile(&pat, false) else {
            return vec![EditorCommand::Message(format!("bad pattern: {pat}"))];
        };
        self.last_search = pat.clone();
        // Everything after the pattern is the command, delimiters and all:
        // `:g/x/s/a/b/` has three more of them and they belong to the `:s`.
        let cmd = match parts[1..].join(&delim.to_string()) {
            c if c.trim().is_empty() => "d".to_string(),
            c => c,
        };
        let hits: Vec<usize> = (first..=last)
            .filter(|&l| {
                let text = self
                    .buffer
                    .slice_string(self.buffer.line_start(l), self.buffer.line_end(l));
                re.is_match(&text) != invert
            })
            .collect();
        if hits.is_empty() {
            return vec![EditorCommand::Message(format!("pattern not found: {pat}"))];
        }
        let n = hits.len();
        let mut out = Vec::new();
        for line in hits.into_iter().rev() {
            self.apply(EditorCommand::MoveTo(self.buffer.first_non_blank(line)));
            for c in self.ex_command(&cmd) {
                match c.needs_app() {
                    true => out.push(c),
                    false => self.apply(c),
                }
            }
        }
        out.push(EditorCommand::Message(format!("{n} lines")));
        out
    }

    /// `:[range]normal {keys}` — the keys, on every line of the range.
    ///
    /// The Esc on the end is vim's: `:normal` leaves no half-typed command
    /// behind, so `:normal A;` closes its own insert session.
    fn ex_normal(&mut self, range: Option<(usize, usize)>, keys: &str) -> Vec<EditorCommand> {
        let mut typed: Vec<Key> = keys.chars().map(Key::Char).collect();
        typed.push(Key::Esc);
        let Some((first, last)) = range else {
            return self.run_keys(&typed, 1);
        };
        let mut out = Vec::new();
        for line in (first..=last).rev() {
            if line > self.buffer.last_line() {
                continue;
            }
            self.apply(EditorCommand::MoveTo(self.buffer.line_start(line)));
            out.extend(self.run_keys(&typed, 1));
        }
        out
    }

    fn ex_command(&mut self, line: &str) -> Vec<EditorCommand> {
        let line = line.trim();
        // `:!cmd` — vim's shell escape, and like `:s` it has to be read before
        // the split below, since `:!wc -l` is one command line and not a verb
        // with an argument.
        //
        // Handed to the app because core owns no processes. What happens there
        // is `Term::shell`: run through `$SHELL -c` — so a pipe, an alias and a
        // redirect all mean what they say — and *reported* in the echo area
        // rather than opened. `:!` is a thing you want done; the shell is still
        // there for the commands you want to sit in front of.
        //
        // ponytail: no filter form. `:%!sort` in vim replaces the range with the
        // command's output, which needs the range fed in as stdin and the result
        // read back — a synchronous spawn plus a new `EditorCommand`, where this
        // is one line. Worth building the first time someone reaches for it.
        if let Some(cmd) = line.strip_prefix('!') {
            let cmd = cmd.trim();
            return match cmd.is_empty() {
                true => vec![EditorCommand::Message("usage: :!<command>".into())],
                false => vec![EditorCommand::Term(format!("shell:{cmd}"))],
            };
        }
        // The range comes off the front of every command that takes one, and
        // `None` — no range typed at all — is not the same as the current line:
        // `:d` deletes this line and `:%d` the buffer, but only `:normal` with
        // a range runs more than once.
        let (range, rest) = self.ex_range(line);
        let rest = rest.trim_start();
        let lines = range.unwrap_or_else(|| self.line_range(1));
        // Substitute before the split below: `s/a/b/g` has no whitespace in it,
        // so the generic parse would take the whole line for a command name.
        if let Some(cmds) = self.substitute(rest, lines) {
            return cmds;
        }
        // The command name is its leading letters and nothing else: ex does not
        // want a space in front of an argument, so `:m0`, `:b2` and `:q!` are
        // one token each and splitting on whitespace read all three as a verb
        // nobody has heard of.
        let name = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let (cmd, tail) = rest.split_at(name);
        let bang = tail.starts_with('!');
        let arg = tail.trim_start_matches('!').trim();
        // `:g` and `:v` take the pattern with no space in front of it, so they
        // are read before the split and default to the whole buffer rather than
        // to the current line — `:g/x/d` on one line is nobody's intention.
        let whole = range.unwrap_or((0, self.buffer.last_line()));
        if let Some(pat) = global_arg(rest, "global", 'g') {
            return self.ex_global(whole, pat, false);
        }
        if let Some(pat) = global_arg(rest, "vglobal", 'v') {
            return self.ex_global(whole, pat, true);
        }
        match cmd {
            // A bare range is "go to that line", which is how `:42` works.
            "" if range.is_some() => {
                self.push_jump();
                vec![EditorCommand::MoveTo(self.buffer.first_non_blank(lines.1))]
            }
            "" => vec![],
            // The line-oriented verbs, which are the operators under another
            // name: `:d` is `dd` over a range you can name without going there.
            "d" | "delete" | "y" | "yank" => {
                let (start, end) = self.line_bounds(lines);
                let op = match cmd.starts_with('d') {
                    true => Op::Delete,
                    false => Op::Yank,
                };
                let mut cmds = match self.mode.is_visual() {
                    true => vec![EditorCommand::SetMode(Mode::Normal)],
                    false => vec![],
                };
                cmds.extend(self.operate(op, start, end, true));
                cmds
            }
            // `:m` moves the range and `:t`/`:co` copies it, both to *after*
            // the address given — and `:m0` is "to the top", which is why the
            // destination is read as an address rather than as a line number.
            "m" | "move" | "t" | "co" | "copy" => self.ex_move(lines, arg, cmd.starts_with('m')),
            "normal" | "norm" => self.ex_normal(range, arg),
            // vim's `:noh`: stop highlighting, which here is "forget the
            // pattern", since the highlight is drawn from `last_search`.
            "noh" | "nohl" | "nohlsearch" => {
                self.last_search.clear();
                vec![EditorCommand::Message(String::new())]
            }
            // ponytail: no `:only`, and no `:new`/`:vnew`. The first wants
            // "close every window but this one" and the second "a split
            // showing an empty buffer", and core has a command for neither —
            // `CloseWindow` closes the *focused* one, so `:only` spelled with
            // it would close exactly the window you meant to keep.
            "sp" | "split" => vec![EditorCommand::SplitWindow(frame::Split::Rows)],
            "vs" | "vsp" | "vsplit" => vec![EditorCommand::SplitWindow(frame::Split::Columns)],
            "clo" | "close" => vec![EditorCommand::CloseWindow],
            "b" | "bu" | "buf" | "buffer" if arg.is_empty() => {
                self.open_prompt(PromptKind::Buffer);
                vec![]
            }
            // By name, and by index when the name is a number — which is what
            // `:b2` means and what makes the switcher's list addressable.
            "b" | "bu" | "buf" | "buffer" => match arg.parse::<usize>() {
                Ok(i) => vec![EditorCommand::SwitchBuffer(i)],
                Err(_) => match self.buffer_names().iter().position(|n| n.contains(arg)) {
                    Some(i) => vec![EditorCommand::SwitchBuffer(i)],
                    None => vec![EditorCommand::Message(format!("no such buffer: {arg}"))],
                },
            },
            "bn" | "bnext" => vec![EditorCommand::SwitchBuffer(1)],
            "bp" | "bprev" | "bprevious" | "bN" => {
                vec![EditorCommand::SwitchBuffer(self.buffer_names().len().saturating_sub(1))]
            }
            "bd" | "bdelete" => vec![EditorCommand::KillBuffer(0)],
            // vim's `:q` closes the *window*, and reaches the application only
            // when there is nothing smaller left to close. That is the ordering
            // worth having: closing a split is something you do constantly and
            // quitting is something you do once, so the common gesture must not
            // be the destructive one.
            //
            // Decided here rather than by chaining commands, because every
            // command in a returned batch is computed against the *pre-edit*
            // state — a `CloseWindow` followed by a conditional quit would ask
            // its question of the world before the close happened.
            //
            // `:q!` and `:quit` stay unconditional: `!` is vim's "I mean it",
            // and typing the whole word is not something a hand does by
            // accident.
            "q" if !bang && self.frame().windows.len() > 1 => vec![EditorCommand::CloseWindow],
            "q" if !bang && self.frames.len() > 1 => vec![EditorCommand::CloseFrame],
            "q" | "quit" => vec![EditorCommand::Quit],
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
            // Magnify this pane and no other. Built-in verbs rather than Lisp,
            // because the thing being changed is a `Window` and core owns those
            // — the Lisp side has no handle on one to name.
            "zoom-in" => vec![EditorCommand::ZoomWindow(1)],
            "zoom-out" => vec![EditorCommand::ZoomWindow(-1)],
            "zoom-reset" => vec![EditorCommand::ZoomWindow(0)],
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
            // Only the verbs the app's project backend actually answers to,
            // which is exactly what `BUILTIN_COMMANDS` lists. A bare prefix
            // test swallowed every *Lisp* command spelled `project-…` —
            // `project-make`, `project-clone` — turning a working function
            // into "unknown project verb: make". Anything not core's own
            // falls through to the image below, where it was defined.
            other if other.starts_with("project-") && BUILTIN_COMMANDS.contains(&other) => {
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

/// The pattern part of `:g` or `:v`, or `None` when the line merely begins
/// with the same letter — `:vsplit` is not a `:v`, and the delimiter is what
/// tells the two apart.
fn global_arg<'a>(rest: &'a str, long: &str, short: char) -> Option<&'a str> {
    let tail = rest
        .strip_prefix(long)
        .or_else(|| rest.strip_prefix(short))?;
    tail.starts_with(|c: char| !c.is_alphanumeric() && !c.is_whitespace())
        .then_some(tail)
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

/// The class of the character at `i`, for a `w`-word or a `W`-WORD.
///
/// vim's two vocabularies, and the whole difference between them: a WORD is
/// delimited by whitespace and nothing else, so `foo.bar(baz)` is one WORD and
/// five words. One function with a flag rather than two sets of three motions,
/// because the *only* thing that changes is whether punctuation is its own
/// class or part of the run.
fn class_at(buf: &crate::Buffer, i: usize, big: bool) -> Option<u8> {
    let c = buf.char_at(i)?;
    Some(match (big, class(c)) {
        (_, 0) => 0,
        (true, _) => 1,
        (false, k) => k,
    })
}

fn word_forward(buf: &crate::Buffer, pos: usize, big: bool) -> usize {
    let n = buf.len_chars();
    let mut i = pos;
    let start_class = class_at(buf, i, big).unwrap_or(0);
    if start_class != 0 {
        while i < n && class_at(buf, i, big) == Some(start_class) {
            i += 1;
        }
    }
    while i < n && class_at(buf, i, big) == Some(0) {
        i += 1;
    }
    i.min(n)
}

fn word_backward(buf: &crate::Buffer, pos: usize, big: bool) -> usize {
    let mut i = pos;
    if i == 0 {
        return 0;
    }
    i -= 1;
    while i > 0 && class_at(buf, i, big) == Some(0) {
        i -= 1;
    }
    let c = class_at(buf, i, big).unwrap_or(0);
    while i > 0 && class_at(buf, i - 1, big) == Some(c) {
        i -= 1;
    }
    i
}

fn word_end(buf: &crate::Buffer, pos: usize, big: bool) -> usize {
    let n = buf.len_chars();
    let mut i = pos + 1;
    while i < n && class_at(buf, i, big) == Some(0) {
        i += 1;
    }
    let c = class_at(buf, i, big).unwrap_or(0);
    while i + 1 < n && class_at(buf, i + 1, big) == Some(c) {
        i += 1;
    }
    i.min(n.saturating_sub(1))
}

/// `ge` — backwards to the last character of the previous word.
///
/// The mirror of [`word_end`] rather than [`word_backward`]: `b` lands on a
/// word's *first* character and this lands on the previous word's *last* one,
/// which is why neither can be written in terms of the other.
fn word_end_backward(buf: &crate::Buffer, pos: usize, big: bool) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut i = pos - 1;
    // Off the end of whatever word point was inside.
    let here = class_at(buf, pos, big).unwrap_or(0);
    if here != 0 {
        while i > 0 && class_at(buf, i, big) == Some(here) {
            i -= 1;
        }
    }
    while i > 0 && class_at(buf, i, big) == Some(0) {
        i -= 1;
    }
    i
}

/// `g~`, per character. An iterator because a character's other case is not
/// always one character — German `ß` uppercases to `SS`, and dropping the
/// second one would silently eat text.
fn flip_case(c: char) -> Box<dyn Iterator<Item = char>> {
    if c.is_lowercase() {
        Box::new(c.to_uppercase())
    } else if c.is_uppercase() {
        Box::new(c.to_lowercase())
    } else {
        Box::new(std::iter::once(c))
    }
}

/// Every sentence of the paragraph around `pos`, as (start, end, next) —
/// where `end` stops after the sentence's last character and `next` is where
/// the following one begins. `is` is the first pair, `as` the second, and `(`
/// and `)` walk the starts.
///
/// A paragraph at a time rather than the whole buffer: a sentence is found by
/// scanning *forwards* to a `.`, `!` or `?` followed by whitespace, so "which
/// sentence am I in" has no answer that does not start somewhere known, and a
/// blank line is the nearest such place.
fn sentences(buf: &crate::Buffer, pos: usize) -> Vec<(usize, usize, usize)> {
    let line = buf.line_of(pos);
    let head = paragraph(buf, line, false);
    let first = match buf.line_len(head) == 0 && head < line {
        true => head + 1,
        false => head,
    };
    let stop = buf.line_end(paragraph(buf, line, true));
    let space = |i: usize| matches!(buf.char_at(i), Some(' ' | '\t' | '\n'));
    let mut out = Vec::new();
    let mut i = buf.line_start(first);
    while i < stop {
        // The end of this sentence: a terminator, then any closing quotes and
        // brackets that belong to it, then whitespace.
        let mut end = stop;
        let mut j = i;
        while j < stop {
            if matches!(buf.char_at(j), Some('.' | '!' | '?')) {
                let mut k = j + 1;
                while matches!(buf.char_at(k), Some(')' | ']' | '"' | '\'')) {
                    k += 1;
                }
                if k >= stop || space(k) {
                    end = k;
                    break;
                }
            }
            j += 1;
        }
        let mut next = end;
        while next < stop && space(next) {
            next += 1;
        }
        out.push((i, end, next));
        if next == i {
            break;
        }
        i = next;
    }
    out
}

/// `(` and `)` — the start of the previous or next sentence.
fn sentence_step(buf: &crate::Buffer, pos: usize, forward: bool) -> usize {
    let here = sentences(buf, pos);
    match forward {
        true => here
            .iter()
            .map(|&(s, _, _)| s)
            .find(|&s| s > pos)
            // Off the end of this paragraph: the first sentence of the next.
            .unwrap_or_else(|| {
                let after = buf.line_start(paragraph(buf, buf.line_of(pos), true));
                sentences(buf, after).first().map_or(after, |&(s, _, _)| s)
            }),
        false => here
            .iter()
            .map(|&(s, _, _)| s)
            .rev()
            .find(|&s| s < pos)
            .unwrap_or_else(|| buf.line_start(paragraph(buf, buf.line_of(pos), false))),
    }
}

/// `[[` and `]]` — the next line that begins a top-level form.
///
/// "Begins in column 1 with something that is not a closer", which covers both
/// vim's rule (a `{` on its own at the left margin) and the shape this editor
/// is mostly pointed at (a `(defun` at the left margin).
fn section(buf: &crate::Buffer, line: usize, forward: bool) -> usize {
    let opens = |l: usize| {
        let start = buf.line_start(l);
        buf.line_len(l) > 0
            && !buf
                .char_at(start)
                .is_some_and(|c| c.is_whitespace() || matches!(c, ')' | '}' | ']'))
    };
    match forward {
        true => ((line + 1)..=buf.last_line())
            .find(|&l| opens(l))
            .unwrap_or(buf.last_line()),
        false => (0..line).rev().find(|&l| opens(l)).unwrap_or(0),
    }
}

/// Next/previous blank line — `{` and `}`.
fn paragraph(buf: &crate::Buffer, line: usize, forward: bool) -> usize {
    let last = buf.last_line();
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

    /// The block above the arrows, which used to be dropped on the floor in
    /// every mode: `key_from_keydown` had no arm for any of them, so they all
    /// reached `combo_char` and answered `None`.
    #[test]
    fn the_navigation_block_moves_scrolls_and_deletes() {
        // Home is the line start and not the first non-blank — `0`, not `^`.
        let mut ed = fresh("    indented\n");
        feed(&mut ed, &keys("$"));
        feed(&mut ed, &[Key::Home]);
        assert_eq!(ed.buffer.cursor_line_col(), (0, 0));
        // The last *character*, not past it: `<end>` is `$`, and in Normal mode
        // the cursor cannot sit on the newline. Insert mode differs, and that
        // difference is `$`'s, not this key's.
        feed(&mut ed, &[Key::End]);
        assert_eq!(ed.buffer.cursor_line_col(), (0, 11));

        // Being motions rather than special cases, they take an operator. The
        // claim is that `d<end>` *is* `d$`, so it is asserted against `d$` and
        // is not also a second opinion about what `d$` ought to do.
        let mut ed = fresh("hello world\n");
        feed(&mut ed, &keys("wd"));
        feed(&mut ed, &[Key::End]);
        let mut same = fresh("hello world\n");
        feed(&mut same, &keys("wd$"));
        assert_eq!(ed.buffer.text.to_string(), same.buffer.text.to_string());

        // `⌦` is `x`, in Normal and over a selection both.
        let mut ed = fresh("abc\n");
        feed(&mut ed, &[Key::Delete]);
        assert_eq!(ed.buffer.text.to_string(), "bc\n");
        feed(&mut ed, &keys("vl"));
        feed(&mut ed, &[Key::Delete]);
        assert_eq!(ed.buffer.text.to_string(), "\n");

        // The page keys are `C-f`/`C-b`, which this asserts by running the same
        // check `join_paste_and_scroll_take_their_counts` makes of those.
        let mut ed = fresh("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n");
        ed.viewport_lines = 4;
        feed(&mut ed, &[Key::PageDown]);
        assert_eq!(ed.buffer.cursor_line_col(), (2, 0));
        feed(&mut ed, &[Key::PageUp]);
        assert_eq!(ed.buffer.cursor_line_col(), (0, 0));

        // An F-key does nothing at all until something binds it, in Normal and
        // in Insert — which is the whole of its default behaviour.
        let mut ed = fresh("abc\n");
        assert_eq!(ed.handle_key(Key::F(5)), vec![]);
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        assert_eq!(ed.handle_key(Key::F(5)), vec![]);
        assert_eq!(ed.buffer.text.to_string(), "abc\n");
        // ...and once bound it fires, which is the only thing it is for.
        ed.apply(EditorCommand::BindKey {
            mode: "normal".into(),
            keys: "<f5>".into(),
            command: "my-f5".into(),
        });
        ed.apply(EditorCommand::SetMode(Mode::Normal));
        let out = ed.handle_key(Key::F(5));
        assert!(
            out.iter()
                .any(|c| matches!(c, EditorCommand::CallLisp(s) if s.contains("my-f5"))),
            "an F-key is a key a config can bind: {out:?}"
        );
    }

    /// Insert mode agrees with Normal about what they mean, so pressing `i`
    /// does not change where Home lands.
    #[test]
    fn the_navigation_block_works_in_insert_too() {
        let mut ed = fresh("    indented\n");
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        feed(&mut ed, &[Key::End]);
        assert_eq!(ed.buffer.cursor_line_col(), (0, 12));
        feed(&mut ed, &[Key::Home]);
        assert_eq!(ed.buffer.cursor_line_col(), (0, 0));
        // Forward delete, as against `<bs>`'s backward one.
        feed(&mut ed, &[Key::Delete]);
        assert_eq!(ed.buffer.text.to_string(), "   indented\n");
    }

    /// Every one of them belongs to the child. Home and End are readline's
    /// line-start and line-end, the page keys move a full-screen program and
    /// `⌦` is forward-delete — none of which the editor has any claim on while
    /// a shell has the keyboard.
    #[test]
    fn the_navigation_block_reaches_the_shell() {
        let mut ed = fresh("");
        ed.apply(EditorCommand::SetMode(Mode::Terminal));
        for key in [
            Key::Home,
            Key::End,
            Key::PageUp,
            Key::PageDown,
            Key::Delete,
            Key::F(1),
            Key::F(12),
        ] {
            assert_eq!(
                ed.handle_key(key),
                vec![EditorCommand::TermKey(key)],
                "{key:?} must go to the shell"
            );
        }
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

    /// ...and a binding made for the buffer's *mode* fires, which is what
    /// `(define-key "ai-mode" "C-M-r" ...)` used to do silently nothing.
    #[test]
    fn a_mode_binding_fires_inside_a_terminal() {
        let mut ed = fresh("");
        ed.apply(EditorCommand::SetMajorMode("ai-mode".into()));
        ed.apply(EditorCommand::BindKey {
            mode: "ai-mode".into(),
            keys: "C-M-r".into(),
            command: "ai-restart".into(),
        });
        // Bound in both maps, so this also pins the order: the mode's map is
        // the narrower one and wins, exactly as it does in `normal_key`.
        ed.apply(EditorCommand::BindKey {
            mode: "terminal".into(),
            keys: "C-M-r".into(),
            command: "terminal-normal".into(),
        });
        ed.apply(EditorCommand::SetMode(Mode::Terminal));
        assert_eq!(
            ed.handle_key(Key::CtrlMeta('r')),
            vec![EditorCommand::CallLisp("(ai-restart)".into())]
        );

        // And the lookup reaches for nothing it was not given: `ai-mode` binds
        // no `C-c`, so SIGINT still leaves for the child.
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

    /// `+` is a *shifted* key and a vim motion both, so the two ways a dired
    /// binding on it could quietly stop working are the ones worth pinning: a
    /// keymap lookup that never sees the `+` the keyboard makes out of `⇧=`,
    /// and a grammar arm claiming it before the user's binding is consulted.
    #[test]
    fn a_shifted_punctuation_binding_reaches_dired() {
        let mut ed = fresh("a\nb\n");
        ed.apply(EditorCommand::BindKey {
            mode: "dired".into(),
            keys: "+".into(),
            command: "dired-mkdir".into(),
        });
        ed.apply(EditorCommand::SetMode(Mode::Dired));
        assert_eq!(
            ed.handle_key(Key::Char('+')),
            vec![EditorCommand::Dired("mkdir".into())]
        );
    }

    /// Esc takes the echo area down, which is what makes a report you can
    /// dismiss — `:!` answers there now. The log keeps what was *said*, so the
    /// clear must not land in it as a blank line.
    #[test]
    fn esc_clears_the_status_without_logging_the_clear() {
        let mut ed = fresh("a\n");
        ed.apply(EditorCommand::Message("42 files".into()));
        assert_eq!(ed.status, "42 files");

        feed(&mut ed, &[Key::Esc]);
        assert_eq!(ed.status, "");
        assert_eq!(ed.messages, vec!["42 files".to_string()]);
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

    /// With `scroll-past-end` off, which is what this has always tested: the
    /// pane must stay full of document, so a file shorter than the window
    /// cannot scroll at all. `C-d` writes `scroll` without clamping, so this is
    /// really a test of the backstop at the end of `ensure_cursor_visible`.
    #[test]
    fn scroll_never_walks_a_short_file_off_screen() {
        let mut ed = fresh("a\nb\nc\nd\ne");
        ed.settings.scroll_past_end = false;
        ed.viewport_lines = 33;
        feed(&mut ed, &[Key::Ctrl('d')]);
        assert_eq!(ed.scroll, 0);
    }

    /// And with it on, `C-d` may empty the pane — but only down to the last
    /// *real* line, and point comes with it rather than being left behind or
    /// pushed onto the rope's phantom trailing line.
    #[test]
    fn scroll_past_end_lets_c_d_walk_a_short_file_up_to_its_last_line() {
        let mut ed = fresh("a\nb\nc\nd\ne\n");
        ed.viewport_lines = 33;
        feed(&mut ed, &[Key::Ctrl('d')]);
        assert_eq!(ed.scroll, ed.buffer.last_line());
        assert_eq!(ed.buffer.cursor_line_col().0, ed.buffer.last_line());
    }

    /// Enter carries the line's indentation to the line it opens, and one step
    /// further when that line opened a block.
    ///
    /// `o` and `O` already carried the indent and Enter carried nothing, so the
    /// same gesture written two ways produced two different lines. All three go
    /// through `indent_after` now.
    #[test]
    fn a_newline_lands_where_the_line_above_it_started() {
        let mut ed = fresh("    already indented\n");
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::MoveTo(ed.buffer.line_end(0)));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.text.to_string(), "    already indented\n    \n");
        // ...and point is after the indent, not before it.
        assert_eq!(ed.buffer.cursor, ed.buffer.len_chars() - 1);

        // With nothing claimed, a colon is just a colon.
        let mut ed = fresh("if x:\n");
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::MoveTo(ed.buffer.line_end(0)));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.text.to_string(), "if x:\n\n");
    }

    #[test]
    fn a_line_that_opens_a_block_indents_the_next_one() {
        let mut ed = fresh("if x:\n");
        ed.settings.indent_openers = vec![":".into()];
        ed.settings.tab_width = 4;
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::MoveTo(ed.buffer.line_end(0)));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.text.to_string(), "if x:\n    \n");

        // It stacks on what was already there, which is what makes a nested
        // block land in the right place.
        ed.apply(EditorCommand::InsertText("while y:".into()));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.text.to_string(), "if x:\n    while y:\n        \n");

        // Trailing whitespace does not hide the opener — a `{` you left a space
        // after still opens a block.
        let mut ed = fresh("fn main() {   \n");
        ed.settings.indent_openers = vec!["{".into()];
        ed.settings.tab_width = 2;
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::MoveTo(ed.buffer.line_end(0)));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.text.to_string(), "fn main() {   \n  \n");
    }

    /// Splitting a line in the middle is not opening a block under it, so the
    /// tail keeps the column it would have kept anywhere else. Emacs'
    /// `electric-indent` draws the line in the same place.
    #[test]
    fn a_newline_in_the_middle_of_a_line_indents_nothing() {
        let mut ed = fresh("    foo(bar)\n");
        ed.settings.indent_openers = vec!["(".into()];
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        // Between `foo(` and `bar)`.
        ed.apply(EditorCommand::MoveTo(8));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(ed.buffer.text.to_string(), "    foo(\nbar)\n");
    }

    /// `o` opens below and takes the step; `O` opens *above* and must not — the
    /// line it copies from is the one it is about to sit on top of.
    #[test]
    fn o_takes_the_step_and_shift_o_does_not() {
        let mut ed = fresh("if x:\n    body\n");
        ed.settings.indent_openers = vec![":".into()];
        ed.settings.tab_width = 4;
        feed(&mut ed, &keys("o"));
        assert_eq!(ed.buffer.text.to_string(), "if x:\n    \n    body\n");

        let mut ed = fresh("if x:\n    body\n");
        ed.settings.indent_openers = vec![":".into()];
        ed.settings.tab_width = 4;
        feed(&mut ed, &keys("O"));
        assert_eq!(ed.buffer.text.to_string(), "\nif x:\n    body\n");
    }

    /// The face `HlKind::Match` was reserved for and left undrawn. What is
    /// asserted here is the arithmetic behind it — the renderer's own machinery
    /// turns a char range into cells, and the selection has been proving that
    /// path works for as long as there has been one.
    #[test]
    fn a_search_lights_up_every_hit_on_screen() {
        let mut ed = fresh("fn alpha() {}\nlet alpha = alpha + 1;\nfn beta() {}\n");
        // Nothing searched for, nothing lit.
        assert!(ed.search_hits(&ed.buffer, 0, 10).is_empty());

        ed.last_search = "alpha".into();
        let hits = ed.search_hits(&ed.buffer, 0, 10);
        assert_eq!(hits.len(), 3, "{hits:?}");
        // Char ranges into the buffer, which is what the cell conversion wants.
        assert_eq!(&ed.buffer.slice_string(hits[0].0, hits[0].1), "alpha");
        assert_eq!(hits[0], (3, 8));

        // Bounded by the window it was asked about: a pane showing one line is
        // not made to scan the file. The third hit is on line 1, the first two
        // on lines 0 and 1 — so line 0 alone holds exactly one.
        assert_eq!(ed.search_hits(&ed.buffer, 0, 1).len(), 1);
        assert_eq!(ed.search_hits(&ed.buffer, 1, 2).len(), 2);
        // ...and a window past the end is empty rather than a panic.
        assert!(ed.search_hits(&ed.buffer, 99, 120).is_empty());

        // `:noh` is the way out, which is what its arm has always claimed to be.
        ed.ex_command("noh");
        assert!(ed.search_hits(&ed.buffer, 0, 10).is_empty());
    }

    /// While `/` is open the hits follow what is being *typed*, which is the
    /// half of incremental search that was missing: the cursor moved to the
    /// first match and nothing showed you how much the pattern caught.
    #[test]
    fn an_open_search_prompt_lights_up_what_it_would_find() {
        let mut ed = fresh("one two one\n");
        ed.last_search = "two".into();
        ed.handle_key(Key::Char('/'));
        assert_eq!(ed.prompt.as_ref().map(|p| p.kind), Some(PromptKind::Search));
        for c in "one".chars() {
            ed.handle_key(Key::Char(c));
        }
        // The prompt's text, not the pattern `n` would repeat.
        let hits = ed.search_hits(&ed.buffer, 0, 10);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(&ed.buffer.slice_string(hits[0].0, hits[0].1), "one");
    }

    /// Offsets are characters everywhere in this editor, and a regex answers in
    /// bytes. The buffer below is the one where the two disagree.
    #[test]
    fn hits_are_character_offsets_through_multibyte_text() {
        let mut ed = fresh("héllo wörld héllo\n");
        ed.last_search = "héllo".into();
        let hits = ed.search_hits(&ed.buffer, 0, 10);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0], (0, 5));
        // 12 and not 14: `é` and `ö` are two bytes each and one character each.
        assert_eq!(hits[1], (12, 17));
        assert_eq!(&ed.buffer.slice_string(hits[1].0, hits[1].1), "héllo");
    }

    /// A pattern that can match nothing matches *everywhere*, and a zero-width
    /// band is a fill per character that draws no pixels.
    #[test]
    fn a_pattern_that_matches_nothing_paints_nothing() {
        let mut ed = fresh("aaa bbb\n");
        ed.last_search = "x*".into();
        assert!(ed.search_hits(&ed.buffer, 0, 10).is_empty());
        // ...but the same pattern where it does match is still a hit.
        ed.last_search = "a*".into();
        let hits = ed.search_hits(&ed.buffer, 0, 10);
        assert_eq!(hits, vec![(0, 3)]);
    }

    /// The signal the draw loop skips frames on. What makes it usable is that it
    /// is *conservative*: everything that could have changed the screen moves
    /// it, including the things that changed nothing.
    #[test]
    fn every_keystroke_and_every_command_moves_the_generation() {
        let mut ed = fresh("hello\n");
        let mut seen = ed.generation;
        macro_rules! moved {
            ($what:expr) => {
                assert_ne!(ed.generation, seen, "{} left the generation still", $what);
                seen = ed.generation;
            };
        }

        ed.handle_key(Key::Char('l'));
        moved!("a motion");
        // A key that does nothing at all — `k` on the first line — still costs a
        // frame rather than risking one that should have happened.
        ed.handle_key(Key::Char('k'));
        moved!("a motion that could not move");
        ed.apply(EditorCommand::InsertChar('x'));
        moved!("an edit");
        // A command that was *refused* wrote to the status line, so it changed
        // the screen and has to say so. This is the case a bump placed after the
        // read-only guard would miss.
        ed.show_special(crate::BufferKind::Dired, "listing");
        ed.apply(EditorCommand::InsertChar('x'));
        moved!("an edit a read-only buffer refused");
        // ...and a plain message, which reaches no other counter in the editor.
        ed.apply(EditorCommand::Message("hello".into()));
        moved!("a message");
        let _ = seen;
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

    /// The comment over `BUILTIN_COMMANDS` asks a human to keep the list in
    /// step with the match in `run_action`. This is that same request, made of
    /// something that will actually notice: an offered name with no arm behind
    /// it falls through to the Lisp fallback above, so `M-x` lists it and
    /// running it reports an undefined function.
    ///
    /// Only this direction is checkable and only this direction is a bug. The
    /// match deliberately has arms the list leaves out — `M-x`, `consult-line`,
    /// `goto-line`, `grep`, `term` and `shell` are second spellings of verbs
    /// already offered, and `open:` takes an argument.
    ///
    /// A fresh editor per name because these are real verbs: several open a
    /// prompt, and one leaves ace labels up.
    #[test]
    fn every_offered_verb_has_an_arm_behind_it() {
        for name in crate::evil::BUILTIN_COMMANDS {
            let mut ed = fresh("hello\n");
            assert_ne!(
                ed.run_action(name),
                vec![EditorCommand::CallLisp(format!("({name})"))],
                "{name} is offered by M-x and `run_action` has no arm for it"
            );
        }
    }

    /// The other direction of the same list, for `project-` only: a name core
    /// does not offer is Lisp's. `project-make` and `project-clone` are
    /// `defun`s in `runtime/library.lisp`, and a bare prefix test turned both
    /// into a verb the app has never heard of.
    ///
    /// The rest came the same way and by hand: wave 2 moved `root`, `dired`,
    /// `compile` and `test` into `runtime/plugins/project.lisp` and wave 3 moved
    /// the four pickers, and the *only* thing that makes a Lisp `project-…`
    /// reachable is its absence from `BUILTIN_COMMANDS`. Leaving one behind
    /// would route the key to a verb `Project::run_verb` no longer has, which
    /// reports "unknown project verb" — so this is the assertion that catches a
    /// half-finished migration.
    #[test]
    fn a_project_name_core_does_not_own_reaches_lisp() {
        for name in [
            "project-make",
            "project-clone",
            "project-root",
            "project-dired",
            "project-compile",
            "project-test",
            "project-find-file",
            "project-find-dir",
            "project-switch",
            "project-open",
        ] {
            let mut ed = fresh("hello\n");
            assert_eq!(
                ed.run_action(name),
                vec![EditorCommand::CallLisp(format!("({name})"))],
                "{name} is Lisp's and was swallowed by the `project-` prefix"
            );
        }
        // ...and the one verb core still owns goes to the app. It is about the
        // *cache* rather than about projects, which is why it stayed.
        assert_eq!(
            fresh("hello\n").run_action("project-forget"),
            vec![EditorCommand::Project("forget".into())]
        );
    }

    /// The two halves of an annotated `M-x` row, which have to agree: the
    /// candidate on screen carries a docstring and a key, and the command is
    /// only its first word. Getting that wrong means `M-x` calls a Lisp
    /// function whose name is the whole sentence, and the history recalls a
    /// sentence you cannot press Enter on.
    #[test]
    fn an_annotated_row_runs_and_is_remembered_as_the_command_alone() {
        let mut ed = fresh("");
        // Exactly the shape `%annotated-command` builds: name, padding, the
        // first line of the docstring, then the key in parentheses.
        ed.commands = vec!["qzz-thing                Do the thing (SPC q z)".into()];
        ed.open_prompt(PromptKind::Command);
        feed(&mut ed, &keys("qzz"));
        assert_eq!(
            ed.handle_key(Key::Enter),
            vec![EditorCommand::CallLisp("(qzz-thing)".into())],
            "the annotation must not reach the image"
        );

        // ...and it comes back off `M-p` as something you could press Enter on.
        ed.open_prompt(PromptKind::Command);
        feed(&mut ed, &keys("xy"));
        ed.handle_key(Key::Meta('p'));
        assert_eq!(ed.prompt.as_ref().unwrap().text, "qzz-thing");
        // Walking forward off the end gives back what was being typed rather
        // than stranding you on the oldest entry.
        ed.handle_key(Key::Meta('n'));
        assert_eq!(ed.prompt.as_ref().unwrap().text, "xy");
        // A kind with nothing in it is simply a no-op, not a wrong recall.
        ed.open_prompt(PromptKind::File);
        ed.handle_key(Key::Meta('p'));
        assert_eq!(ed.prompt.as_ref().unwrap().text, "");
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
        // The old clamp, pinned deliberately: `scroll-past-end` moves only the
        // *limit*, and every other property of a wheel notch — the view moving,
        // the cursor being dragged no further than it must — has to be
        // identical either way. The past-the-end limit is tested below.
        ed.settings.scroll_past_end = false;

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
        ed.settings.scroll_past_end = false;
        ed.viewport_lines = 40;
        ed.apply(EditorCommand::ScrollLines(5));
        assert_eq!(ed.scroll, 0);
    }

    /// The feature, in the units it is defined in: the wheel stops with the
    /// last line on the *top* row and the rest of the pane empty, which is
    /// vim's `~` filler and Emacs' end of buffer.
    ///
    /// Three separate hazards in one test because they are one gesture:
    /// the limit, point staying inside the document, and the buffer being
    /// untouched — nothing is inserted to make the empty rows, which is the
    /// whole point of moving a clamp instead of the text.
    #[test]
    fn scroll_past_end_stops_with_the_last_line_on_the_top_row() {
        let text: String = (0..100).map(|i| format!("line{i}\n")).collect();
        let mut ed = fresh(&text);
        ed.viewport_lines = 10;
        ed.apply(EditorCommand::ScrollLines(10_000));

        assert_eq!(ed.scroll, ed.buffer.last_line());
        // `len_lines() - 1` is the empty string after the trailing newline. The
        // clamp naming it would drag point onto a line `G` refuses to visit.
        assert_eq!(ed.scroll, ed.buffer.len_lines() - 2);
        assert_eq!(ed.buffer.cursor_line_col().0, ed.buffer.last_line());
        assert!(ed.buffer.cursor < ed.buffer.len_chars());
        assert_eq!(ed.buffer.text.to_string(), text);
    }

    /// Turning it off does not merely stop new scrolling: it puts a view that
    /// is already out past the end back where the old rule would have it, on
    /// the spot, because the writer runs through `apply` like everything else.
    #[test]
    fn turning_scroll_past_end_off_pulls_the_view_back() {
        let text: String = (0..100).map(|i| format!("line{i}\n")).collect();
        let mut ed = fresh(&text);
        ed.viewport_lines = 10;
        ed.apply(EditorCommand::ScrollLines(10_000));
        assert_eq!(ed.scroll, ed.buffer.last_line());

        ed.apply(EditorCommand::SetScrollPastEnd(false));
        assert_eq!(ed.scroll, ed.buffer.len_lines() - 10);
        ed.apply(EditorCommand::ScrollLines(10_000));
        assert_eq!(ed.scroll, ed.buffer.len_lines() - 10);
    }

    /// A listing is not a document, so there is no past the end of one — the
    /// same instinct that takes the gutter off a generated buffer.
    #[test]
    fn a_generated_buffer_keeps_the_old_clamp() {
        let text: String = (0..100).map(|i| format!("line{i}\n")).collect();
        let mut ed = fresh(&text);
        ed.buffer.kind = BufferKind::Dired;
        ed.viewport_lines = 10;
        ed.apply(EditorCommand::ScrollLines(10_000));
        assert_eq!(ed.scroll, ed.buffer.len_lines() - 10);
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

    /// `o` on a closed fold opens a line after the whole fold, not inside it.
    ///
    /// The bug was invisible in the worst way: `o` took `line_end` of the
    /// cursor's line, which on a fold is the one line of it you can see, so the
    /// new line went *into* the collapsed subtree and you were left typing on a
    /// row the renderer does not draw.
    #[test]
    fn o_on_a_closed_fold_opens_below_the_whole_fold() {
        // "* one\nbody\nmore\n* two", line starts 0, 6, 11, 16.
        let mut ed = fresh("* one\nbody\nmore\n* two");
        folded(&mut ed, 0, 15);
        feed(&mut ed, &keys("o"));
        feed(&mut ed, &keys("X"));
        assert_eq!(
            ed.buffer.text.to_string(),
            "* one\nbody\nmore\nX\n* two",
            "the new line lands after `more`, the last line the fold hides"
        );

        // A fold running to the end of the document has no visible line below
        // it, so `o` opens at the very end rather than standing still.
        let mut tail = fresh("* one\nbody\nmore");
        folded(&mut tail, 0, 15);
        feed(&mut tail, &keys("o"));
        feed(&mut tail, &keys("X"));
        assert_eq!(tail.buffer.text.to_string(), "* one\nbody\nmore\nX");

        // Unfolded, `o` is exactly what it always was — the line after the one
        // point is on, not the line after the paragraph.
        let mut plain = fresh("* one\nbody\nmore");
        feed(&mut plain, &keys("o"));
        feed(&mut plain, &keys("X"));
        assert_eq!(plain.buffer.text.to_string(), "* one\nX\nbody\nmore");

        // `O` needs no fold handling and must not grow any: a fold's first line
        // is drawn, the cursor is on it, and above it is above the fold.
        let mut above = fresh("* one\nbody\nmore\n* two");
        folded(&mut above, 0, 15);
        feed(&mut above, &keys("O"));
        feed(&mut above, &keys("X"));
        assert_eq!(above.buffer.text.to_string(), "X\n* one\nbody\nmore\n* two");
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
        off.wrap_cols = 10; // drawn...
        off.settings.line_overflow = LineOverflow::Truncate; // ...but truncating
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

    /// `M-<bs>` in a prompt eats a word, and eats the separator before it on
    /// the next press — the two-step that makes it usable for walking back up
    /// a path. The last press, on an empty line, cancels like `<bs>` does.
    #[test]
    fn meta_backspace_kills_a_word_in_the_prompt() {
        let mut ed = fresh("");
        ed.run_action("find-file");
        feed(&mut ed, &keys("~/src/doc"));

        for want in ["~/src/", "~/", ""] {
            for cmd in ed.handle_key(Key::MetaBackspace) {
                ed.apply(cmd);
            }
            assert_eq!(ed.prompt.as_ref().map(|p| p.text.clone()).as_deref(), Some(want));
        }
        for cmd in ed.handle_key(Key::MetaBackspace) {
            ed.apply(cmd);
        }
        assert!(ed.prompt.is_none(), "empty prompt cancels, as `<bs>` does");
    }

    /// The switcher shows you the buffer you are pointing at, and cancelling
    /// puts back the one you came from.
    ///
    /// The trap this guards is that switching *reorders* the buffer list — the
    /// outgoing buffer goes to the front, which is what makes the switcher
    /// most-recently-used. So a preview built on the indices the candidates had
    /// when the prompt opened would scramble them on the first arrow key, and
    /// each subsequent press would show a buffer other than the highlighted one.
    /// Hence ids, and hence this test moving the selection *twice*.
    /// A name alone does not say what a buffer *is* — two `mod.rs` and a
    /// `*scratch*` read alike. The mode is the missing word, and it is the one
    /// the modeline already uses, so the switcher and the strip agree.
    #[test]
    fn the_buffer_switcher_names_the_mode() {
        let mut ed = fresh("first");
        ed.apply(EditorCommand::CreateBuffer("*second*".into()));
        ed.run_action("switch-buffer");
        let items = ed.prompt.as_ref().unwrap().items.clone();
        assert!(
            items.iter().all(|i| i.ends_with("Fundamental")),
            "every row should name its mode; got {items:#?}"
        );
        // Padded to one column, so the modes line up rather than tracking the
        // ragged right edge of the names.
        let widths: Vec<usize> = items
            .iter()
            .map(|i| i.rfind("Fundamental").unwrap())
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{items:#?}");
    }

    /// `C-x b RET` should go somewhere. The buffer you are looking at is not a
    /// useful default for a command whose whole job is to leave it.
    #[test]
    fn the_switcher_opens_on_the_buffer_you_would_switch_to() {
        let mut ed = fresh("first");
        let home = ed.buffer.id;
        ed.apply(EditorCommand::CreateBuffer("*second*".into()));
        ed.apply(EditorCommand::CreateBuffer("*third*".into()));
        let third = ed.buffer.id;

        ed.run_action("switch-buffer");
        let p = ed.prompt.as_ref().unwrap();
        assert_ne!(p.ids[p.matches[p.selected]], third, "not the live buffer");
        assert!(p.current().unwrap().starts_with("*second*"), "the last one");
        // Still offered, though — moved to the back, not dropped, so the way
        // home is one `C-p` and the ids still name every row.
        assert_eq!(p.ids.last(), Some(&third), "{:?}", p.items);
        assert!(p.ids.contains(&home));

        // Enter takes it, without a keystroke in between.
        for cmd in ed.handle_key(Key::Enter) {
            ed.apply(cmd);
        }
        assert_eq!(ed.buffer.name(), "*second*");
    }

    #[test]
    fn the_buffer_switcher_shows_what_it_is_pointing_at() {
        let mut ed = fresh("first");
        ed.apply(EditorCommand::CreateBuffer("*second*".into()));
        ed.apply(EditorCommand::CreateBuffer("*third*".into()));
        let home = ed.buffer.id;

        ed.run_action("switch-buffer");
        assert_eq!(ed.prompt.as_ref().map(|p| p.kind), Some(PromptKind::Buffer));
        let names = ed.prompt.as_ref().unwrap().items.clone();
        let ids = ed.prompt.as_ref().unwrap().ids.clone();
        assert_eq!(names.len(), ids.len(), "one id per candidate");

        // Down twice. The second press is the one that would fail on indices:
        // by then the first preview has already moved the editor's own list.
        let mut seen = Vec::new();
        for _ in 0..2 {
            for cmd in ed.handle_key(Key::Ctrl('n')) {
                ed.apply(cmd);
            }
            let p = ed.prompt.as_ref().unwrap();
            let want = p.ids[p.matches[p.selected]];
            assert_eq!(ed.buffer.id, want, "the live buffer is the highlighted one");
            seen.push(want);
        }
        assert_ne!(seen[0], seen[1], "two presses, two different buffers");

        // Escape comes home — to the *buffer*, which is the thing a cursor
        // offset is meaningless without.
        for cmd in ed.handle_key(Key::Esc) {
            ed.apply(cmd);
        }
        assert!(ed.prompt.is_none());
        assert_eq!(ed.buffer.id, home, "cancelling restores the buffer");

        // ...and accepting lands on the highlighted one for good.
        ed.run_action("switch-buffer");
        for cmd in ed.handle_key(Key::Ctrl('n')) {
            ed.apply(cmd);
        }
        let want = {
            let p = ed.prompt.as_ref().unwrap();
            p.ids[p.matches[p.selected]]
        };
        for cmd in ed.handle_key(Key::Enter) {
            ed.apply(cmd);
        }
        assert!(ed.prompt.is_none());
        assert_eq!(ed.buffer.id, want);
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
        // Sanity: it came back to where *this* prompt opened, which is not where
        // the buffer started — otherwise the assertion above would pass on a
        // preview that never moved at all.
        assert_ne!(ed.buffer.cursor, start);
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

    /// A minor-mode binding answers in Visual too, which is the whole reason
    /// magit's `s` and `u` live in one.
    ///
    /// A *state* keymap layers over Normal only, so pressing `V` to select the
    /// lines you mean to stage is exactly the moment magit stops answering. The
    /// fix is a minor mode on the status buffer rather than a change to how
    /// state keymaps layer — because `k` is bound to discard, and a listing
    /// keymap reaching Visual would turn the key that *builds* the selection
    /// into the key that throws the work away.
    #[test]
    fn a_minor_mode_binding_answers_in_visual_mode_as_well_as_normal() {
        let mut ed = fresh("one\ntwo\nthree");
        ed.apply(EditorCommand::SetMinorMode("magit-mode".into(), true));
        ed.apply(EditorCommand::BindKey {
            mode: "magit-mode".into(),
            keys: "s".into(),
            command: "magit-stage".into(),
        });
        assert_eq!(
            ed.handle_key(Key::Char('s')),
            vec![EditorCommand::Git("stage".into())],
            "in Normal"
        );

        feed(&mut ed, &keys("V"));
        assert!(ed.mode.is_visual());
        assert_eq!(
            ed.handle_key(Key::Char('s')),
            vec![EditorCommand::Git("stage".into())],
            "and over a selection, which is the case that was unreachable"
        );

        // ...and `j`/`k` are still motions in there, which is what makes a
        // selection possible at all. Bind `k` in this map and staging a region
        // becomes impossible *by discarding it*.
        let before = ed.buffer.cursor_line_col().0;
        feed(&mut ed, &keys("j"));
        assert_eq!(ed.buffer.cursor_line_col().0, before + 1);
        feed(&mut ed, &keys("k"));
        assert_eq!(ed.buffer.cursor_line_col().0, before);
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

    /// `:!` is read before the verb split, or `:!wc -l` loses its argument —
    /// and before `:s`, so a command line with slashes in it stays a command.
    #[test]
    fn a_bang_runs_a_shell_command_line_whole() {
        let mut ed = fresh("");
        assert_eq!(
            ed.ex_command("!sed s/a/b/ < in | wc -l"),
            vec![EditorCommand::Term("shell:sed s/a/b/ < in | wc -l".into())]
        );
        // A bare `:!` is a typo, not an empty command line typed at the shell.
        assert!(matches!(
            ed.ex_command("!  ").as_slice(),
            [EditorCommand::Message(_)]
        ));
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
            hint: String::new(),
        });
        let out = ed.handle_key(Key::Char('p'));
        assert!(out.contains(&EditorCommand::CallLisp("(zemacs-projects)".into())));
    }
}




#[cfg(test)]
mod vim_grammar {
    use crate::tests::{feed, fresh};
    use crate::*;

    fn keys(s: &str) -> Vec<Key> {
        s.chars().map(Key::Char).collect()
    }

    /// Type `s`, then Esc — one insert session, spelled the way a test reads.
    fn run(text: &str, s: &str) -> Editor {
        let mut ed = fresh(text);
        feed(&mut ed, &keys(s));
        ed
    }

    fn text(ed: &Editor) -> String {
        ed.buffer.text.to_string()
    }

    /// A rope counts the empty string after a trailing newline as a line, and
    /// every file has one. Everything that reaches for "the last line" was
    /// landing on a row with no text in it.
    #[test]
    fn the_last_line_is_the_last_line_with_text_on_it() {
        let mut ed = fresh("one\ntwo\nthree\n");
        feed(&mut ed, &keys("G"));
        assert_eq!(ed.buffer.cursor_line_col(), (2, 0), "G lands on `three`");
        // ...and `j` cannot walk off the end onto it either.
        let mut ed = fresh("one\ntwo\n");
        feed(&mut ed, &keys("jjjj"));
        assert_eq!(ed.buffer.cursor_line_col(), (1, 0));
        // A file that really does end without a newline keeps its last line.
        let mut ed = fresh("one\ntwo");
        feed(&mut ed, &keys("G"));
        assert_eq!(ed.buffer.cursor_line_col(), (1, 0));
    }

    /// `dd` on the last line took the file's trailing newline with it, so a
    /// file that ended properly stopped doing so the first time you deleted its
    /// last line.
    #[test]
    fn dd_on_the_last_line_keeps_the_trailing_newline() {
        assert_eq!(text(&run("one\ntwo\nthree\n", "jjdd")), "one\ntwo\n");
        // ...and a file with no trailing newline still loses the line before's,
        // rather than leaving the blank line `dd` was asked to remove.
        assert_eq!(text(&run("one\ntwo\nthree", "jjdd")), "one\ntwo");
        assert_eq!(text(&run("one\ntwo\n", "dd")), "two\n");
    }

    /// vim leaves point on the first non-blank of the line that moved up.
    #[test]
    fn a_linewise_delete_lands_on_the_first_non_blank() {
        let ed = run("a\n    indented\nc\n", "dd");
        assert_eq!(text(&ed), "    indented\nc\n");
        assert_eq!(ed.buffer.cursor_line_col(), (0, 4));
    }

    /// `o` and `O` carry the line's indent — vim's `autoindent`, which evil
    /// leaves on and without which every open in indented code is followed by
    /// retyping the indentation.
    #[test]
    fn o_and_capital_o_keep_the_indent() {
        let mut ed = fresh("fn main() {\n    body\n}\n");
        feed(&mut ed, &keys("joX"));
        assert_eq!(text(&ed), "fn main() {\n    body\n    X\n}\n");

        let mut ed = fresh("fn main() {\n    body\n}\n");
        feed(&mut ed, &keys("jOX"));
        assert_eq!(text(&ed), "fn main() {\n    X\n    body\n}\n");
    }

    /// `cw` behaves like `ce` — vim's one deliberate irregularity, and the one
    /// everybody relies on without knowing its name.
    #[test]
    fn cw_does_not_eat_the_space_after_the_word() {
        assert_eq!(text(&run("one two three", "cw")), " two three");
        // On whitespace it really is `dw`: there the "word" is the run of
        // spaces, and there is nothing irregular to do.
        assert_eq!(text(&run("a   b", "lcw")), "ab");
    }

    /// `dw` on the last word of a line stops there rather than pulling the next
    /// line up behind it.
    #[test]
    fn dw_does_not_cross_the_line_break() {
        assert_eq!(text(&run("one two\nthree\n", "wdw")), "one \nthree\n");
        // A count that legitimately spans words on one line still does.
        assert_eq!(text(&run("one two three\n", "d2w")), "three\n");
    }

    /// Text objects: the half of the grammar that names a range by what it *is*.
    #[test]
    fn text_objects_name_a_range_rather_than_a_destination() {
        // From anywhere in the word, not just its start.
        assert_eq!(text(&run("foo bar baz", "wldiw")), "foo  baz");
        // `aw` takes the trailing space with it, which is what joins the words.
        assert_eq!(text(&run("foo bar baz", "wldaw")), "foo baz");
        // Brackets, nested, from inside.
        assert_eq!(text(&run("f(a, g(b), c)", "fbdi(")), "f(a, g(), c)");
        assert_eq!(text(&run("f(a, g(b), c)", "fbda(")), "f(a, g, c)");
        // Point outside any inner pair walks out to the enclosing one.
        assert_eq!(text(&run("f(a, b)", "fadi(")), "f()");
        // Quotes pair from the start of the line, so `ci\"` works from either.
        assert_eq!(text(&run("x = \"hello\" + y", "fhdi\"")), "x = \"\" + y");
        assert_eq!(text(&run("x = \"hello\" + y", "fhda\"")), "x =  + y");
        // A paragraph is linewise.
        assert_eq!(text(&run("a\nb\n\nc\n", "dip")), "\nc\n");
    }

    /// ...and in visual mode the object *becomes* the selection.
    #[test]
    fn a_text_object_in_visual_mode_selects_it() {
        let mut ed = fresh("foo bar baz");
        feed(&mut ed, &keys("wvi w"[..2].chars().collect::<String>().as_str()));
        feed(&mut ed, &keys("iw"));
        assert_eq!(ed.selection(), Some((4, 7)), "`bar`");
        feed(&mut ed, &keys("d"));
        assert_eq!(text(&ed), "foo  baz");
    }

    /// `>` and `<` shift whole lines whatever the motion, because indentation
    /// is a property of a line and there is nothing else they could mean.
    #[test]
    fn shift_operators_indent_whole_lines() {
        let mut ed = fresh("a\nb\nc\n");
        ed.settings.tab_width = 2;
        feed(&mut ed, &keys(">>"));
        assert_eq!(text(&ed), "  a\nb\nc\n");
        assert_eq!(ed.buffer.cursor_line_col(), (0, 2), "on the first non-blank");
        feed(&mut ed, &keys("<<"));
        assert_eq!(text(&ed), "a\nb\nc\n");
        // Over a motion, and over a selection.
        let mut ed = fresh("a\nb\nc\n");
        ed.settings.tab_width = 2;
        feed(&mut ed, &keys(">j"));
        assert_eq!(text(&ed), "  a\n  b\nc\n");
        let mut ed = fresh("a\nb\nc\n");
        ed.settings.tab_width = 2;
        feed(&mut ed, &keys("Vj>"));
        assert_eq!(text(&ed), "  a\n  b\nc\n");
        // A blank line is left alone rather than given trailing whitespace.
        let mut ed = fresh("a\n\nc\n");
        ed.settings.tab_width = 2;
        feed(&mut ed, &keys("Vjj>"));
        assert_eq!(text(&ed), "  a\n\n  c\n");
    }

    #[test]
    fn case_operators_and_tilde() {
        assert_eq!(text(&run("hello world", "gUw")), "HELLO world");
        assert_eq!(text(&run("HELLO world", "guw")), "hello world");
        assert_eq!(text(&run("Hello", "g~~")), "hELLO");
        // `~` flips the character under point and steps over it.
        let ed = run("abc", "~");
        assert_eq!(text(&ed), "Abc");
        assert_eq!(ed.buffer.cursor, 1);
        // ...and over a selection, `u`/`U`/`~` are the single-key spellings.
        assert_eq!(text(&run("hello", "vllU")), "HELlo");
    }

    /// WORD motions: whitespace-delimited, so `foo.bar` is one of them.
    #[test]
    fn a_word_and_a_big_word_are_two_vocabularies() {
        assert_eq!(text(&run("foo.bar baz", "dW")), "baz");
        assert_eq!(text(&run("foo.bar baz", "dw")), ".bar baz");
        // `ge` is backwards to the end of the previous word — not, as it was,
        // the end of the buffer.
        let mut ed = fresh("one two three");
        feed(&mut ed, &keys("$"));
        feed(&mut ed, &keys("ge"));
        assert_eq!(ed.buffer.cursor, 6, "the `o` of `two`");
    }

    /// The matcher `%` and the paren highlight both ask.
    #[test]
    fn matching_bracket_answers_from_either_end_and_from_just_past_a_closer() {
        let b = crate::Buffer::from_str("f(a, g(b), c)");
        assert_eq!(b.matching_bracket(1), Some(12), "the outer pair, forwards");
        assert_eq!(b.matching_bracket(12), Some(1), "and backwards");
        assert_eq!(b.matching_bracket(6), Some(8), "nesting is counted");
        // Just *past* a closer, which is where point sits the instant you
        // finish typing one — the case the highlight exists for.
        assert_eq!(b.matching_bracket(9), Some(6));
        // ...but not just past an opener: there is nothing to answer about yet.
        assert_eq!(b.matching_bracket(7), None);
        // Unbalanced is None rather than a guess.
        assert_eq!(crate::Buffer::from_str("(a").matching_bracket(0), None);
        assert_eq!(crate::Buffer::from_str("abc").matching_bracket(1), None);
    }

    #[test]
    fn percent_jumps_between_the_brackets() {
        let mut ed = fresh("if (a && b) {\n}\n");
        feed(&mut ed, &keys("f("));
        feed(&mut ed, &keys("%"));
        assert_eq!(ed.buffer.cursor, 10, "the closing paren");
        feed(&mut ed, &keys("%"));
        assert_eq!(ed.buffer.cursor, 3, "and back");
        // With an operator it is inclusive, so the pair goes with it.
        assert_eq!(text(&run("f(a, b) rest", "ld%")), "f rest");
    }

    #[test]
    fn semicolon_repeats_the_last_find() {
        let mut ed = fresh("a.b.c.d");
        feed(&mut ed, &keys("f."));
        assert_eq!(ed.buffer.cursor, 1);
        feed(&mut ed, &keys(";"));
        assert_eq!(ed.buffer.cursor, 3);
        feed(&mut ed, &keys(","));
        assert_eq!(ed.buffer.cursor, 1, "`,` is the same find turned over");
    }

    #[test]
    fn s_and_capital_s_and_x() {
        assert_eq!(text(&run("abcd", "sZ")), "Zbcd");
        assert_eq!(text(&run("  abc\nd\n", "SZ")), "Z\nd\n");
        // `X` deletes backwards and stops at the line start.
        assert_eq!(text(&run("ab\ncd\n", "jlX")), "ab\nd\n");
        assert_eq!(text(&run("ab\ncd\n", "jXX")), "ab\ncd\n");
    }

    /// `.` repeats the last change, insert session and all.
    #[test]
    fn dot_repeats_the_last_change() {
        // An operator with a motion.
        let mut ed = fresh("one two three four");
        feed(&mut ed, &keys("dw"));
        assert_eq!(text(&ed), "two three four");
        feed(&mut ed, &keys("."));
        assert_eq!(text(&ed), "three four");

        // ...and one that opened an insert session: the typing is part of it.
        let mut ed = fresh("aaa bbb ccc");
        feed(&mut ed, &keys("ciwX"));
        feed(&mut ed, &[Key::Esc]);
        assert_eq!(text(&ed), "X bbb ccc");
        feed(&mut ed, &keys("w."));
        assert_eq!(text(&ed), "X X ccc");

        // A motion is not a change, so `.` after one still repeats the edit.
        feed(&mut ed, &keys("w"));
        feed(&mut ed, &keys("."));
        assert_eq!(text(&ed), "X X X");
    }

    #[test]
    fn gv_puts_the_last_selection_back() {
        let mut ed = fresh("hello world");
        feed(&mut ed, &keys("vll"));
        feed(&mut ed, &[Key::Esc]);
        feed(&mut ed, &keys("gv"));
        assert!(ed.mode.is_visual());
        assert_eq!(ed.selection(), Some((0, 3)));
    }

    #[test]
    fn visual_o_swaps_the_ends_and_r_replaces_the_selection() {
        let mut ed = fresh("abcdef");
        feed(&mut ed, &keys("lllvl"));
        assert_eq!(ed.selection(), Some((3, 5)));
        feed(&mut ed, &keys("o"));
        assert_eq!(ed.buffer.cursor, 3, "point is on the other end now");

        let mut ed = fresh("abc\ndef\n");
        feed(&mut ed, &keys("vjl"));
        feed(&mut ed, &keys("rz"));
        assert_eq!(text(&ed), "zzz\nzzf\n", "newlines survive");
    }

    #[test]
    fn star_searches_for_the_word_under_the_cursor() {
        let mut ed = fresh("foo bar\nfoobar\nfoo\n");
        feed(&mut ed, &keys("*"));
        // Not `foobar`: the pattern is anchored on word boundaries.
        assert_eq!(ed.buffer.cursor_line_col(), (2, 0));
    }

    #[test]
    fn line_and_screen_motions() {
        // `|` is a column, counting from 1.
        let mut ed = fresh("abcdef\n");
        feed(&mut ed, &keys("4|"));
        assert_eq!(ed.buffer.cursor, 3);
        // `+` and `-` are linewise and land on the first non-blank.
        let mut ed = fresh("a\n    b\nc\n");
        feed(&mut ed, &keys("+"));
        assert_eq!(ed.buffer.cursor_line_col(), (1, 4));
        feed(&mut ed, &keys("-"));
        assert_eq!(ed.buffer.cursor_line_col(), (0, 0));
        // `H`/`L` are about the window, so they read what is drawn.
        let mut ed = fresh("1\n2\n3\n4\n5\n6\n7\n8\n");
        ed.viewport_lines = 4;
        ed.scroll = 2;
        feed(&mut ed, &keys("H"));
        assert_eq!(ed.buffer.cursor_line_col(), (2, 0));
        feed(&mut ed, &keys("L"));
        assert_eq!(ed.buffer.cursor_line_col(), (5, 0));
    }

    /// `C-x C-s` has to reach `save-file` while you are typing, so Insert mode
    /// has to hold a sequence whose first key was never going to be text.
    #[test]
    fn a_control_prefixed_sequence_works_in_insert_mode() {
        let mut ed = fresh("");
        ed.keymap
            .insert((Mode::Insert, "C-x C-s".into()), "save-file".into());
        feed(&mut ed, &keys("i"));
        assert_eq!(ed.mode, Mode::Insert);
        // `C-x` alone types nothing and waits.
        for cmd in ed.handle_key(Key::Ctrl('x')) {
            ed.apply(cmd);
        }
        assert_eq!(text(&ed), "", "the prefix is not text");
        let out = ed.handle_key(Key::Ctrl('s'));
        assert!(
            out.iter().any(|c| matches!(c, EditorCommand::CallLisp(s) if s.contains("save-file"))),
            "{out:?}"
        );
        // ...and a letter still types itself rather than starting a sequence.
        feed(&mut ed, &keys("z"));
        assert_eq!(text(&ed), "z");
    }

    /// `C-a`/`C-x`, which is the one edit nobody remembers is a vim feature
    /// until the day they are renumbering a list.
    #[test]
    fn ctrl_a_and_ctrl_x_step_the_number_under_the_cursor() {
        let mut ed = fresh("x = 41\n");
        feed(&mut ed, &[Key::Ctrl('a')]);
        assert_eq!(text(&ed), "x = 42\n", "the next number on the line, not the first");
        assert_eq!(ed.buffer.cursor, 5, "point lands on the last digit");
        feed(&mut ed, &[Key::Ctrl('x')]);
        assert_eq!(text(&ed), "x = 41\n");
        // A count.
        let mut ed = fresh("7");
        feed(&mut ed, &keys("10"));
        feed(&mut ed, &[Key::Ctrl('a')]);
        assert_eq!(text(&ed), "17");
        // The `-` is the number's sign, not a subtraction.
        let mut ed = fresh("a -1 b");
        feed(&mut ed, &[Key::Ctrl('x')]);
        assert_eq!(text(&ed), "a -2 b");
        // Leading zeroes are a width, and are kept.
        let mut ed = fresh("v009");
        feed(&mut ed, &[Key::Ctrl('a')]);
        assert_eq!(text(&ed), "v010");
        // Hex, from the `0` and from inside the digits alike.
        let mut ed = fresh("0x0f");
        feed(&mut ed, &[Key::Ctrl('a')]);
        assert_eq!(text(&ed), "0x10");
        let mut ed = fresh("0xff");
        feed(&mut ed, &keys("$"));
        feed(&mut ed, &[Key::Ctrl('a')]);
        assert_eq!(text(&ed), "0x100");
        // A line with no number says so rather than editing something else.
        let mut ed = fresh("no digits here");
        feed(&mut ed, &[Key::Ctrl('a')]);
        assert_eq!(text(&ed), "no digits here");
        assert!(ed.status.contains("no number"), "{}", ed.status);
    }

    /// `R` overwrites until Esc, and stops at the end of the line rather than
    /// eating the newline and pulling the next line up.
    #[test]
    fn capital_r_overwrites_rather_than_inserting() {
        let mut ed = fresh("abcdef\nghi\n");
        feed(&mut ed, &keys("Rxyz"));
        assert_eq!(text(&ed), "xyzdef\nghi\n");
        feed(&mut ed, &[Key::Esc]);
        // ...and Esc puts the grammar back, so the next `x` deletes.
        feed(&mut ed, &keys("x"));
        assert_eq!(text(&ed), "xydef\nghi\n");
        // At the end of a line there is nothing left to replace.
        let mut ed = fresh("ab\ncd\n");
        feed(&mut ed, &keys("$R123"));
        assert_eq!(text(&ed), "a123\ncd\n");
    }

    /// `C-w` is vim's window prefix. It used to *be* "next window", which is
    /// now one key longer and everything else is reachable.
    #[test]
    fn ctrl_w_is_the_window_prefix() {
        let mut ed = fresh("a\n");
        // The prefix alone waits rather than doing anything.
        assert!(ed.handle_key(Key::Ctrl('w')).is_empty());
        let out = ed.handle_key(Key::Char('v'));
        assert!(
            matches!(out.first(), Some(EditorCommand::SplitWindow(frame::Split::Columns))),
            "{out:?}"
        );
        for (key, want) in [
            (Key::Char('s'), frame::Split::Rows),
            (Key::Char('v'), frame::Split::Columns),
        ] {
            ed.handle_key(Key::Ctrl('w'));
            let out = ed.handle_key(key);
            assert!(matches!(out.first(), Some(EditorCommand::SplitWindow(s)) if *s == want));
        }
        ed.handle_key(Key::Ctrl('w'));
        assert!(matches!(
            ed.handle_key(Key::Ctrl('w')).first(),
            Some(EditorCommand::FocusNextWindow)
        ));
        ed.handle_key(Key::Ctrl('w'));
        assert!(matches!(
            ed.handle_key(Key::Char('c')).first(),
            Some(EditorCommand::CloseWindow)
        ));
    }

    /// Ex ranges: the half of `:` that was `%` or nothing.
    #[test]
    fn ex_ranges_name_lines_numbers_marks_and_offsets() {
        let ex = |ed: &mut Editor, line: &str| {
            for cmd in ed.ex_command(line) {
                ed.apply(cmd);
            }
        };
        let mut ed = fresh("a\nb\nc\nd\ne\n");
        ex(&mut ed, "2,3d");
        assert_eq!(text(&ed), "a\nd\ne\n");
        // `$` is the last line, `.` the current one, and `+n` walks off either.
        let mut ed = fresh("a\nb\nc\nd\ne\n");
        ex(&mut ed, ".,+1d");
        assert_eq!(text(&ed), "c\nd\ne\n");
        let mut ed = fresh("a\nb\nc\nd\ne\n");
        ex(&mut ed, "4,$d");
        assert_eq!(text(&ed), "a\nb\nc\n");
        // A bare number is "go there".
        let mut ed = fresh("a\nb\nc\nd\n");
        ex(&mut ed, "3");
        assert_eq!(ed.buffer.cursor_line_col(), (2, 0));
        // A mark can name an end of the range.
        let mut ed = fresh("a\nb\nc\nd\n");
        feed(&mut ed, &keys("jma"));
        feed(&mut ed, &keys("jj"));
        ex(&mut ed, "'a,.d");
        assert_eq!(text(&ed), "a\n");
        // ...and `%` still means all of it.
        let mut ed = fresh("a\nb\n");
        ex(&mut ed, "%d");
        assert_eq!(text(&ed), "");
    }

    /// The line verbs — `:d`, `:y`, `:m`, `:t` — and `:normal`.
    #[test]
    fn ex_line_verbs_move_copy_and_run_keys() {
        let ex = |ed: &mut Editor, line: &str| {
            for cmd in ed.ex_command(line) {
                ed.apply(cmd);
            }
        };
        // `:y` fills the register without touching the text.
        let mut ed = fresh("one\ntwo\nthree\n");
        ex(&mut ed, "2y");
        assert_eq!(text(&ed), "one\ntwo\nthree\n");
        assert_eq!(ed.register().0, "two\n");
        // `:m` moves the range to after the address; `:m0` is the top.
        let mut ed = fresh("a\nb\nc\n");
        ex(&mut ed, "3m0");
        assert_eq!(text(&ed), "c\na\nb\n");
        let mut ed = fresh("a\nb\nc\n");
        ex(&mut ed, "1m$");
        assert_eq!(text(&ed), "b\nc\na\n");
        // `:t` leaves the original where it was.
        let mut ed = fresh("a\nb\n");
        ex(&mut ed, "1t$");
        assert_eq!(text(&ed), "a\nb\na\n");
        // `:normal` runs keys, once per line of the range.
        let mut ed = fresh("a\nb\nc\n");
        ex(&mut ed, "1,3normal A;");
        assert_eq!(text(&ed), "a;\nb;\nc;\n");
    }

    /// `:g` and `:v`, which is the one ex command worth the whole parser.
    #[test]
    fn global_runs_a_command_on_every_matching_line() {
        let ex = |ed: &mut Editor, line: &str| {
            for cmd in ed.ex_command(line) {
                ed.apply(cmd);
            }
        };
        let mut ed = fresh("keep\ndrop me\nkeep\ndrop me too\n");
        ex(&mut ed, "g/drop/d");
        assert_eq!(text(&ed), "keep\nkeep\n");
        // `:v` is the complement.
        let mut ed = fresh("keep\ndrop\nkeep\n");
        ex(&mut ed, "v/keep/d");
        assert_eq!(text(&ed), "keep\nkeep\n");
        // A command other than `d`, and one that is itself a substitute.
        let mut ed = fresh("a1\nb2\na3\n");
        ex(&mut ed, "g/a/s/[0-9]/X/");
        assert_eq!(text(&ed), "aX\nb2\naX\n");
        // ...and `:g/x/normal`, which is the idiom the two exist for together.
        let mut ed = fresh("a\nbb\na\n");
        ex(&mut ed, "g/a/normal A!");
        assert_eq!(text(&ed), "a!\nbb\na!\n");
        // `:vsplit` is not a `:v`.
        let mut ed = fresh("a\n");
        assert!(matches!(
            ed.ex_command("vsplit").first(),
            Some(EditorCommand::SplitWindow(_))
        ));
    }

    /// The registers vim fills for you, and the two that are not registers at
    /// all: the black hole and the read-only ones.
    #[test]
    fn numbered_and_special_registers_hold_what_vim_says_they_do() {
        // `"0` is the last yank and survives a delete, which is the whole
        // reason anyone learns it exists.
        let mut ed = fresh("yanked\ndeleted\nrest\n");
        feed(&mut ed, &keys("yyjdd"));
        assert_eq!(ed.register().0, "deleted\n");
        feed(&mut ed, &keys("\"0p"));
        assert_eq!(text(&ed), "yanked\nrest\nyanked\n");
        // `"1` is the last linewise delete, and older ones shift down.
        let mut ed = fresh("one\ntwo\nthree\n");
        feed(&mut ed, &keys("dddd"));
        feed(&mut ed, &keys("\"1P"));
        assert_eq!(text(&ed), "two\nthree\n");
        feed(&mut ed, &keys("\"2P"));
        assert_eq!(text(&ed), "one\ntwo\nthree\n");
        // `"_` swallows: nothing is left in any register, `""` included.
        let mut ed = fresh("keep\ngone\n");
        feed(&mut ed, &keys("yyj\"_dd"));
        assert_eq!(text(&ed), "keep\n");
        assert_eq!(ed.register().0, "keep\n", "the black hole left `\"\"` alone");
        // The read-only ones come from where they already live.
        let mut ed = fresh("");
        ed.last_search = "hunted".into();
        feed(&mut ed, &keys("i"));
        feed(&mut ed, &keys("typed"));
        feed(&mut ed, &[Key::Esc]);
        feed(&mut ed, &keys("\"/p"));
        assert!(text(&ed).contains("hunted"), "{}", text(&ed));
        feed(&mut ed, &keys("\".p"));
        assert!(text(&ed).contains("typed"), "{}", text(&ed));
        // `"+` and `"*` are the unnamed register, because the unnamed register
        // *is* the system clipboard here.
        let mut ed = fresh("word\nelse\n");
        feed(&mut ed, &keys("\"+yyj\"*p"));
        assert_eq!(text(&ed), "word\nelse\nword\n");
        // ...and `"_p` pastes nothing, since nothing is what a black hole has.
        feed(&mut ed, &keys("\"_p"));
        assert_eq!(text(&ed), "word\nelse\nword\n");
    }

    /// A block insert types once and lands on every line — the whole reason
    /// anyone reaches for `C-v`.
    #[test]
    fn a_block_insert_types_on_every_line_it_covers() {
        let mut ed = fresh("aaa\nbbb\nccc\n");
        feed(&mut ed, &[Key::Ctrl('v')]);
        feed(&mut ed, &keys("jj"));
        feed(&mut ed, &keys("I"));
        assert_eq!(ed.mode, Mode::Insert);
        feed(&mut ed, &keys("X"));
        feed(&mut ed, &[Key::Esc]);
        assert_eq!(text(&ed), "Xaaa\nXbbb\nXccc\n");
        // `A` appends at the block's right edge instead.
        let mut ed = fresh("aaa\nbbb\n");
        feed(&mut ed, &[Key::Ctrl('v')]);
        feed(&mut ed, &keys("jlA;"));
        feed(&mut ed, &[Key::Esc]);
        assert_eq!(text(&ed), "aa;a\nbb;b\n");
    }

    /// A search is a motion, so an operator can take one — and the jump list
    /// is what brings you back afterwards.
    #[test]
    fn search_is_a_motion_and_jumps_are_undoable() {
        let mut ed = fresh("alpha beta gamma\n");
        ed.last_search = "gamma".into();
        feed(&mut ed, &keys("dn"));
        assert_eq!(text(&ed), "gamma\n");
        // ...and `d/pat` too, which means the operator has to outlive the
        // prompt the pattern is typed into.
        let mut ed = fresh("alpha beta gamma\n");
        feed(&mut ed, &keys("d/gamma"));
        feed(&mut ed, &[Key::Enter]);
        assert_eq!(text(&ed), "gamma\n");
        // An abandoned one takes its operator with it.
        let mut ed = fresh("alpha beta\n");
        feed(&mut ed, &keys("d/beta"));
        feed(&mut ed, &[Key::Esc]);
        feed(&mut ed, &keys("x"));
        assert_eq!(text(&ed), "lpha beta\n");
        // `C-o` goes back where the jump started, `C-i` forward again.
        let mut ed = fresh("1\n2\n3\n4\n5\n6\n7\n8\n");
        feed(&mut ed, &keys("jj"));
        assert_eq!(ed.buffer.cursor_line_col(), (2, 0));
        feed(&mut ed, &keys("G"));
        assert_eq!(ed.buffer.cursor_line_col(), (7, 0));
        feed(&mut ed, &[Key::Ctrl('o')]);
        assert_eq!(ed.buffer.cursor_line_col(), (2, 0), "back where `G` began");
        feed(&mut ed, &[Key::Ctrl('i')]);
        assert_eq!(ed.buffer.cursor_line_col(), (7, 0), "and forward again");
        // `` `` `` bounces between the two.
        feed(&mut ed, &keys("``"));
        assert_eq!(ed.buffer.cursor_line_col(), (2, 0));
        feed(&mut ed, &keys("``"));
        assert_eq!(ed.buffer.cursor_line_col(), (7, 0));
    }

    /// Marks nobody sets by hand: the last change, the last insert, and the
    /// ends of the last selection.
    #[test]
    fn the_marks_vim_writes_itself() {
        // `` `. `` is where the last change started.
        let mut ed = fresh("one\ntwo\nthree\n");
        feed(&mut ed, &keys("jdd"));
        feed(&mut ed, &keys("G"));
        feed(&mut ed, &keys("`."));
        assert_eq!(ed.buffer.cursor_line_col(), (1, 0));
        // `` `[ `` and `` `] `` bracket the last yank.
        let mut ed = fresh("hello world\n");
        feed(&mut ed, &keys("wyw"));
        feed(&mut ed, &keys("0`]"));
        assert_eq!(ed.buffer.cursor, 10, "the last character yanked");
        // `gi` goes back to where the last insert ended, still inserting.
        let mut ed = fresh("abc\nxyz\n");
        feed(&mut ed, &keys("A!"));
        feed(&mut ed, &[Key::Esc]);
        feed(&mut ed, &keys("G0"));
        feed(&mut ed, &keys("gi"));
        assert_eq!(ed.mode, Mode::Insert);
        feed(&mut ed, &keys("?"));
        assert_eq!(text(&ed), "abc!?\nxyz\n");
        // `'<`/`'>` name the last selection, which is what `:'<,'>` reads.
        let mut ed = fresh("a\nb\nc\nd\n");
        feed(&mut ed, &keys("jVj"));
        feed(&mut ed, &[Key::Esc]);
        feed(&mut ed, &keys("gg"));
        feed(&mut ed, &keys("'>"));
        assert_eq!(ed.buffer.cursor_line_col(), (2, 0));
    }

    /// Sentences: `(`, `)` and the `is`/`as` objects.
    #[test]
    fn sentences_are_a_motion_and_an_object() {
        let mut ed = fresh("One two. Three four. Five six.\n");
        feed(&mut ed, &keys(")"));
        assert_eq!(ed.buffer.cursor, 9, "the `T` of `Three`");
        feed(&mut ed, &keys(")"));
        assert_eq!(ed.buffer.cursor, 21);
        feed(&mut ed, &keys("("));
        assert_eq!(ed.buffer.cursor, 9);
        // `dis` leaves the space, `das` closes it up.
        assert_eq!(
            text(&run("One two. Three four. Five six.\n", ")dis")),
            "One two.  Five six.\n"
        );
        assert_eq!(
            text(&run("One two. Three four. Five six.\n", ")das")),
            "One two. Five six.\n"
        );
    }

    /// `it`/`at`, which needs a scanner and not a bracket match.
    #[test]
    fn tag_objects_pair_by_name_rather_than_by_nesting() {
        assert_eq!(
            text(&run("<a><b>text</b></a>", "fedit")),
            "<a><b></b></a>"
        );
        assert_eq!(text(&run("<a><b>text</b></a>", "fedat")), "<a></a>");
        // An attribute holding a `>` does not end the tag early.
        assert_eq!(
            text(&run("<p title=\"a>b\">hi</p>", "fhdit")),
            "<p title=\"a>b\"></p>"
        );
        // A self-closing tag is not an opener waiting to be matched.
        assert_eq!(text(&run("<d>a<br/>b</d>", "fadit")), "<d></d>");
    }

    /// `gJ`, a counted paste, and the scroll keys.
    #[test]
    fn join_paste_and_scroll_take_their_counts() {
        // `gJ` joins without putting a space at the seam.
        assert_eq!(text(&run("foo\n  bar\n", "gJ")), "foo  bar\n");
        assert_eq!(text(&run("foo\n  bar\n", "J")), "foo bar\n");
        // `3p` pastes three times.
        assert_eq!(text(&run("ab", "yl3p")), "aaaab");
        // `C-f` and `C-b` move a screenful, less the two lines of overlap.
        let mut ed = fresh("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n");
        ed.viewport_lines = 4;
        feed(&mut ed, &[Key::Ctrl('f')]);
        assert_eq!(ed.buffer.cursor_line_col(), (2, 0));
        feed(&mut ed, &[Key::Ctrl('b')]);
        assert_eq!(ed.buffer.cursor_line_col(), (0, 0));
        // `C-e` scrolls the view and drags point only when it has to.
        let mut ed = fresh("1\n2\n3\n4\n5\n6\n7\n8\n");
        ed.viewport_lines = 4;
        feed(&mut ed, &keys("jj"));
        feed(&mut ed, &[Key::Ctrl('e')]);
        assert_eq!(ed.scroll, 1);
        assert_eq!(ed.buffer.cursor_line_col(), (2, 0), "still on screen, so it stays");
        feed(&mut ed, &[Key::Ctrl('e')]);
        assert_eq!(ed.buffer.cursor_line_col(), (2, 0));
        feed(&mut ed, &[Key::Ctrl('e')]);
        assert_eq!(ed.buffer.cursor_line_col(), (3, 0), "the view left it behind");
    }

    /// `gq` re-wraps to the fill column, paragraph by paragraph.
    #[test]
    fn gq_rewraps_paragraphs_to_the_fill_column() {
        let mut ed = fresh("aaa bbb ccc ddd eee\n\nsecond\n");
        ed.settings.text_width = 7;
        feed(&mut ed, &keys("gqip"));
        assert_eq!(text(&ed), "aaa bbb\nccc ddd\neee\n\nsecond\n");
        // A short paragraph that already fits is joined back up rather than
        // left as it was.
        let mut ed = fresh("aa\nbb\n");
        ed.settings.text_width = 20;
        feed(&mut ed, &keys("gqG"));
        assert_eq!(text(&ed), "aa bb\n");
        // The indent of the first line carries onto every line it makes.
        let mut ed = fresh("    one two three four\n");
        ed.settings.text_width = 12;
        feed(&mut ed, &keys("gqq"));
        assert_eq!(text(&ed), "    one two\n    three\n    four\n");
    }

    /// `[[` and `]]` walk top-level forms, which in a file of Lisp is what the
    /// left margin means.
    #[test]
    fn section_motions_walk_the_left_margin() {
        let mut ed = fresh("(defun a ()\n  1)\n\n(defun b ()\n  2)\n");
        feed(&mut ed, &keys("]]"));
        assert_eq!(ed.buffer.cursor_line_col(), (3, 0));
        feed(&mut ed, &keys("[["));
        assert_eq!(ed.buffer.cursor_line_col(), (0, 0));
    }
}

#[cfg(test)]
mod window_zoom {
    use crate::tests::fresh;
    use crate::*;

    fn zoom(ed: &Editor) -> u16 {
        ed.frames[0].current_window().zoom
    }

    /// Magnification is a property of the *window*, so zooming one pane leaves
    /// its neighbour alone — which is the whole point of it being per window
    /// and is what a per-frame font size could not do.
    #[test]
    fn zooming_one_window_leaves_its_neighbour_alone() {
        let mut ed = fresh("hello\n");
        assert_eq!(zoom(&ed), 100);
        ed.apply(EditorCommand::SplitWindow(frame::Split::Columns));
        let zoomed = ed.frames[0].current;

        for cmd in ed.run_action("zoom-in") {
            ed.apply(cmd);
        }
        assert_eq!(zoom(&ed), 125);
        ed.apply(EditorCommand::FocusNextWindow);
        assert_ne!(ed.frames[0].current, zoomed, "a different pane");
        assert_eq!(zoom(&ed), 100, "which nobody zoomed");
    }

    #[test]
    fn zoom_reset_goes_back_to_the_body_size() {
        let mut ed = fresh("hello\n");
        for _ in 0..3 {
            for cmd in ed.run_action("zoom-in") {
                ed.apply(cmd);
            }
        }
        assert_eq!(zoom(&ed), 200);
        for cmd in ed.run_action("zoom-out") {
            ed.apply(cmd);
        }
        assert_eq!(zoom(&ed), 150);
        for cmd in ed.run_action("zoom-reset") {
            ed.apply(cmd);
        }
        assert_eq!(zoom(&ed), 100);
    }

    /// The global font size is a separate knob and stays one: a window's zoom
    /// multiplies it rather than replacing it.
    #[test]
    fn the_global_font_size_is_untouched_by_a_window_zoom() {
        let mut ed = fresh("hello\n");
        ed.apply(EditorCommand::SetFontSize(20.0));
        for cmd in ed.run_action("zoom-in") {
            ed.apply(cmd);
        }
        assert_eq!(ed.settings.font_size, 20.0);
        assert_eq!(zoom(&ed), 125);
    }

    /// ...and it is reachable by name, so `M-x` and a `define-key` both find it.
    #[test]
    fn the_zoom_verbs_are_published() {
        for name in ["zoom-in", "zoom-out", "zoom-reset"] {
            assert!(crate::evil::BUILTIN_COMMANDS.contains(&name), "{name} is not offered");
        }
    }
}

