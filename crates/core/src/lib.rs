//! zemacs-core — the pure editor model.
//!
//! No GPU, no Lisp, no filesystem policy. This crate owns the document
//! (`Buffer`), the modal state machine (`Mode`), the single mutation channel
//! (`EditorCommand`), and the input translator (`Editor::handle_key`, in
//! [`evil`]).
//!
//! Design rule, unchanged from v0: every change to the *document* flows through
//! [`Editor::apply`] so there is exactly one writer. `handle_key` is the
//! translator from raw key events into commands; it may touch ephemeral UI
//! state (command line, pending operator, counts) but never the document.

pub mod dashboard;
pub mod display;
pub mod evil;
pub mod frame;
pub mod marker;
pub mod minibuffer;
pub mod modeline;
pub mod overlay;
pub mod query;
pub mod scene;

use std::collections::HashMap;
use std::path::PathBuf;

use ropey::Rope;

pub use dashboard::Dashboard;
pub use frame::{BufferId, Frame, Rect, Window, WindowId};
pub use marker::{Insertion, MarkerId};
pub use minibuffer::{CompletionStyle, Prompt, PromptKind};
pub use overlay::{fold_hiding, fold_starts_in, Image, ImageId, Overlay, OverlayEdit, OverlayId};

/// The editor, as the app and the Lisp image both hold it.
///
/// One writer at a time rather than one writer forever: `apply` is still the
/// only way the document changes, but it is now reachable from the Lisp thread
/// too. The lock is meant to be held for a single operation — a read, an
/// `apply`, one frame's drawing — and never across a wait.
pub type Shared = std::sync::Arc<std::sync::Mutex<Editor>>;

/// Editing mode — the heart of the modal ("Evil") feel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Normal,
    Insert,
    Visual,
    VisualLine,
    /// Rectangular selection — `C-v`.
    VisualBlock,
    /// The startup screen. Its own mode because its keymap is entirely its own.
    Dashboard,
    /// The git status buffer. Its own mode so `s`, `u` and `c` can be bound to
    /// staging rather than to substitute, undo and change — a user binding is
    /// consulted before the built-in grammar, so the motions still work.
    Magit,
    /// The directory editor.
    Dired,
    /// A shell has the keyboard. Almost every key goes to the child process
    /// rather than to the editor, so this is the one mode whose keymap is
    /// consulted *instead of* the Evil grammar rather than before it — `d` and
    /// `j` have to reach the shell.
    Terminal,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "V-LINE",
            Mode::VisualBlock => "V-BLOCK",
            Mode::Dashboard => "DASHBOARD",
            Mode::Magit => "MAGIT",
            Mode::Dired => "DIRED",
            Mode::Terminal => "TERM",
        }
    }

    /// Name used by Lisp `define-key` and by keymap lookup.
    pub fn from_name(s: &str) -> Option<Mode> {
        match s.to_ascii_lowercase().as_str() {
            "normal" => Some(Mode::Normal),
            "insert" => Some(Mode::Insert),
            "visual" => Some(Mode::Visual),
            "visual-line" | "vline" => Some(Mode::VisualLine),
            "visual-block" | "vblock" => Some(Mode::VisualBlock),
            "dashboard" => Some(Mode::Dashboard),
            "magit" | "git" => Some(Mode::Magit),
            "dired" => Some(Mode::Dired),
            "terminal" | "term" => Some(Mode::Terminal),
            _ => None,
        }
    }

    pub fn is_visual(self) -> bool {
        matches!(self, Mode::Visual | Mode::VisualLine | Mode::VisualBlock)
    }

    /// A keymap for a generated buffer rather than an editing mode: dired,
    /// magit, the dashboard.
    ///
    /// These *layer over* Normal rather than replacing it. `s` in magit stages
    /// and `d` in dired flags, but `M-x`, `M-o`, the window splits and every
    /// motion have to keep working — a buffer you cannot run a command from is
    /// a dead end, and nothing about listing a directory means the leader key
    /// should stop existing. Terminal is deliberately not here: a shell owns
    /// the keyboard and has its own, narrower fallthrough ([`Key::is_editor_key`]).
    pub fn layers_over_normal(self) -> bool {
        matches!(self, Mode::Dashboard | Mode::Magit | Mode::Dired)
    }
}

impl Key {
    /// True for the keys a *terminal* has no use for, and which therefore stay
    /// with the editor even while a shell has the keyboard.
    ///
    /// On macOS that is everything involving Command, plus the two modified
    /// Enters. Ctrl is deliberately excluded: `C-c`, `C-a`, `C-d`, `C-r` and
    /// `C-w` all belong to the shell, and taking any of them would break it.
    pub fn is_editor_key(self) -> bool {
        matches!(
            self,
            Key::Meta(_) | Key::CtrlMeta(_) | Key::CtrlEnter | Key::CtrlMetaEnter
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// A key event, abstracted away from any windowing library so `core` stays pure
/// and unit-testable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    /// Alt/Option on the keyboard, `M-` in a binding — Emacs' Meta.
    Meta(char),
    /// Both together, `C-M-` in a binding.
    CtrlMeta(char),
    // ponytail: three named keys now carry modifiers — the two window splits
    // and `M-<bs>`. A modifier bitset over a `Named` enum is the real answer and
    // is getting closer; at four, do it.
    CtrlEnter,
    CtrlMetaEnter,
    /// `⌘⌫`. Deletes the word before point, and in a terminal is handed to the
    /// shell, which has its own idea of where a word starts.
    MetaBackspace,
    Enter,
    Tab,
    Backspace,
    Esc,
    Left,
    Right,
    Up,
    Down,
}

impl Key {
    /// The token used in keymap sequences: `"a"`, `"SPC"`, `"C-x"`, `"<esc>"`.
    pub fn token(self) -> String {
        match self {
            Key::Char(' ') => "SPC".into(),
            Key::Char(c) => c.to_string(),
            Key::Ctrl(c) => format!("C-{}", c.to_ascii_lowercase()),
            Key::Meta(c) => format!("M-{c}"),
            Key::CtrlMeta(c) => format!("C-M-{}", c.to_ascii_lowercase()),
            Key::CtrlEnter => "C-<ret>".into(),
            Key::CtrlMetaEnter => "C-M-<ret>".into(),
            Key::MetaBackspace => "M-<bs>".into(),
            Key::Enter => "<ret>".into(),
            Key::Tab => "<tab>".into(),
            Key::Backspace => "<bs>".into(),
            Key::Esc => "<esc>".into(),
            Key::Left => "<left>".into(),
            Key::Right => "<right>".into(),
            Key::Up => "<up>".into(),
            Key::Down => "<down>".into(),
        }
    }

    // --- lisp-api: the inverse of `token` -----------------------------------

    /// The key a keymap token names, or `None`. The exact inverse of
    /// [`Key::token`] for everything `token` can produce.
    ///
    /// `token` could always *spell* a key and nothing could read one back, which
    /// is precisely why [`EditorCommand::TermKey`] was unreachable from Lisp: a
    /// primitive can carry a string and had no way to turn one into a `Key`.
    ///
    /// One token, never a sequence — `"g d"` is two keys and is
    /// [`normalize_keys`]' business. Case is kept for a plain character (`"A"`
    /// is shift-a) and folded for a control chord, exactly as `token` writes
    /// them. The handful of aliases are the spellings a config already types in
    /// `define-key`.
    pub fn from_token(s: &str) -> Option<Key> {
        // Exactly one character, so `"C-xy"` is rejected rather than silently
        // meaning `C-x`.
        fn one(s: &str) -> Option<char> {
            let mut it = s.chars();
            let c = it.next()?;
            it.next().is_none().then_some(c)
        }
        Some(match s {
            "SPC" | "<spc>" => Key::Char(' '),
            "<ret>" | "RET" | "<cr>" => Key::Enter,
            "<tab>" | "TAB" => Key::Tab,
            "<bs>" | "<backspace>" => Key::Backspace,
            "<esc>" | "ESC" => Key::Esc,
            "<left>" => Key::Left,
            "<right>" => Key::Right,
            "<up>" => Key::Up,
            "<down>" => Key::Down,
            "C-<ret>" => Key::CtrlEnter,
            "C-M-<ret>" => Key::CtrlMetaEnter,
            "M-<bs>" => Key::MetaBackspace,
            // Order matters: `C-M-` has to be tried before `C-`.
            _ if s.starts_with("C-M-") => Key::CtrlMeta(one(&s[4..])?.to_ascii_lowercase()),
            _ if s.starts_with("C-") => Key::Ctrl(one(&s[2..])?.to_ascii_lowercase()),
            _ if s.starts_with("M-") => Key::Meta(one(&s[2..])?),
            _ => Key::Char(one(s)?),
        })
    }

    // --- end of the lisp-api block ------------------------------------------
}

// --- Syntax highlighting types -------------------------------------------
// Defined here (not in zemacs-syntax) so the renderer can consume highlights
// without depending on tree-sitter.

/// What a window does with a line wider than it is.
///
/// Emacs' `truncate-lines`, named for what you see rather than for a boolean
/// nobody remembers the polarity of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LineOverflow {
    /// Cut the line at the pane edge and mark it, so a hidden tail is visible
    /// as a fact rather than as silence.
    #[default]
    Truncate,
    /// Continue the line on the next row.
    Wrap,
}

impl LineOverflow {
    pub fn from_name(s: &str) -> Option<LineOverflow> {
        match s.to_ascii_lowercase().as_str() {
            "truncate" | "truncated" | "off" | "nil" => Some(LineOverflow::Truncate),
            "wrap" | "wrapped" | "on" | "t" => Some(LineOverflow::Wrap),
            _ => None,
        }
    }

    /// Drawn in the last column of a truncated line. `→` rather than Emacs'
    /// `$` because it says "there is more that way" without looking like text.
    pub const MARKER: char = '→';
}

/// A highlight class. Deliberately small: a theme has to name every one of
/// these, and tree-sitter capture names get folded down onto them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HlKind {
    Keyword,
    Function,
    Type,
    String,
    Number,
    Comment,
    Constant,
    Variable,
    Operator,
    Punctuation,
    Default,
    /// Modeline faces. In the same table as the syntax faces so `init.lisp`
    /// styles them with the `set-syntax-color` it already has.
    Modeline,
    ModelineInactive,
    ModelineText,
    /// Markup faces, used by org mode. These carry *colour* and nothing else,
    /// which is all a highlight span can carry: a span is a fact about the text
    /// the parser found, and it is recomputed from scratch on every keystroke.
    ///
    /// Emphasis used to be colour-only for a second reason — the renderer opened
    /// exactly one font face — and that one is gone. Real weight, slant and type
    /// size live on [`Overlay`](crate::Overlay) instead of here, where they can
    /// be *put* by a mode rather than derived by a parser, and where clearing
    /// one does not mean re-parsing the buffer. `org-modern.lisp` sets both: the
    /// face for the hue, `weight`/`slant` for the shape.
    Heading1,
    Heading2,
    Heading3,
    Bold,
    Italic,
    Link,
    Code,
    /// The `*`, `/` and `=` that delimit markup — dimmed, so the text stands
    /// out from its own syntax.
    Markup,
}

impl HlKind {
    pub const ALL: [HlKind; 22] = [
        HlKind::Keyword,
        HlKind::Function,
        HlKind::Type,
        HlKind::String,
        HlKind::Number,
        HlKind::Comment,
        HlKind::Constant,
        HlKind::Variable,
        HlKind::Operator,
        HlKind::Punctuation,
        HlKind::Default,
        HlKind::Modeline,
        HlKind::ModelineInactive,
        HlKind::ModelineText,
        HlKind::Heading1,
        HlKind::Heading2,
        HlKind::Heading3,
        HlKind::Bold,
        HlKind::Italic,
        HlKind::Link,
        HlKind::Code,
        HlKind::Markup,
    ];

    pub fn name(self) -> &'static str {
        match self {
            HlKind::Keyword => "keyword",
            HlKind::Function => "function",
            HlKind::Type => "type",
            HlKind::String => "string",
            HlKind::Number => "number",
            HlKind::Comment => "comment",
            HlKind::Constant => "constant",
            HlKind::Variable => "variable",
            HlKind::Operator => "operator",
            HlKind::Punctuation => "punctuation",
            HlKind::Default => "default",
            HlKind::Modeline => "modeline",
            HlKind::ModelineInactive => "modeline-inactive",
            HlKind::ModelineText => "modeline-text",
            HlKind::Heading1 => "heading-1",
            HlKind::Heading2 => "heading-2",
            HlKind::Heading3 => "heading-3",
            HlKind::Bold => "bold",
            HlKind::Italic => "italic",
            HlKind::Link => "link",
            HlKind::Code => "code",
            HlKind::Markup => "markup",
        }
    }

    pub fn from_name(s: &str) -> Option<HlKind> {
        HlKind::ALL.into_iter().find(|k| k.name() == s)
    }
}

/// A highlighted run of the document, in **char** offsets (the unit the rope
/// and the renderer both use).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: HlKind,
}

/// Syntax colors, one per [`HlKind`]. Every entry is settable from Lisp.
#[derive(Clone, Debug)]
pub struct Theme {
    map: HashMap<HlKind, [f32; 3]>,
}

impl Default for Theme {
    fn default() -> Self {
        // A calm, slightly cool palette to match the default background.
        let mut map = HashMap::new();
        map.insert(HlKind::Keyword, [0.78, 0.57, 0.94]);
        map.insert(HlKind::Function, [0.51, 0.75, 1.00]);
        map.insert(HlKind::Type, [0.45, 0.86, 0.83]);
        map.insert(HlKind::String, [0.62, 0.85, 0.55]);
        map.insert(HlKind::Number, [0.98, 0.72, 0.47]);
        map.insert(HlKind::Comment, [0.42, 0.46, 0.58]);
        map.insert(HlKind::Constant, [0.98, 0.60, 0.60]);
        map.insert(HlKind::Variable, [0.86, 0.90, 1.00]);
        map.insert(HlKind::Operator, [0.70, 0.78, 0.92]);
        map.insert(HlKind::Punctuation, [0.55, 0.60, 0.72]);
        map.insert(HlKind::Default, [0.86, 0.90, 1.00]);
        Self { map }
    }
}

impl Theme {
    pub fn color(&self, kind: HlKind, fallback: [f32; 3]) -> [f32; 3] {
        self.map.get(&kind).copied().unwrap_or(fallback)
    }

    pub fn set(&mut self, kind: HlKind, rgb: [f32; 3]) {
        self.map.insert(kind, clamp3(rgb));
    }
}

/// The single channel of document mutation. Keyboard input and the Lisp
/// runtime both produce these; [`Editor::apply`] is the only consumer.
#[derive(Clone, Debug, PartialEq)]
pub enum EditorCommand {
    /// Snapshot the document for undo. Emitted as the first command of any
    /// mutating group so one `u` reverses one user-level edit.
    Checkpoint,
    InsertChar(char),
    InsertText(String),
    InsertNewline,
    DeleteBackward,
    DeleteForward,
    /// Delete `[start, end)` in char offsets.
    DeleteRange(usize, usize),
    /// Copy `[start, end)` into the unnamed register.
    Yank {
        start: usize,
        end: usize,
        linewise: bool,
    },
    /// Put literal text in the register — a block yank is several disjoint
    /// runs, so there is no single range to copy.
    SetRegister {
        text: String,
        linewise: bool,
    },
    Paste {
        after: bool,
    },
    MoveCursor(Direction),
    MoveTo(usize),
    /// Scroll the view by whole lines; negative is toward the top of the file.
    /// The mouse wheel produces these.
    ScrollLines(i32),
    Undo,
    Redo,

    SetMode(Mode),
    ShowDashboard,
    Message(String),
    Quit,

    // --- settings, all reachable from Lisp ---
    SetFontSize(f32),
    SetBackground([f32; 3]),
    SetForeground([f32; 3]),
    SetSyntaxColor(String, [f32; 3]),
    SetLineNumbers(bool),
    SetTabWidth(usize),
    /// Columns of text to centre in the pane; 0 turns it off. See
    /// [`Settings::text_width`].
    SetTextWidth(usize),
    /// `"minibuffer"`, `"bottom"` (consult-like) or `"center"` (telescope-like).
    SetCompletionStyle(String),
    /// Negative sinks the modeline instead of raising it.
    SetModelineRelief(i32),
    SetModelinePad(i32),
    /// `"truncate"` or `"wrap"`.
    SetLineOverflow(String),
    SetRelativeLineNumbers(bool),

    /// Names offered by `M-x`. The Lisp image publishes these at startup and
    /// after a config reload.
    ClearCommands,
    RegisterCommand(String),
    /// Index into [`Editor::buffer_names`]; 0 is the current buffer.
    SwitchBuffer(usize),
    /// The same, by id. What the switcher's preview uses, because switching
    /// reorders the list and an index taken before one is stale after it.
    SwitchBufferId(BufferId),

    // --- major and minor modes ---
    /// Replace the current buffer's major mode. Fires `<name>-hook` in Lisp.
    SetMajorMode(String),
    /// Major modes whose buffers show no line-number gutter, space-separated.
    /// One string rather than a list because the write envelope carries one.
    SetNoGutterModes(String),
    /// Turn a minor mode on or off in the current buffer.
    SetMinorMode(String, bool),

    // --- windows and frames ---
    /// Split the focused window; the new one shows the same buffer.
    SplitWindow(frame::Split),
    CloseWindow,
    FocusNextWindow,
    FocusWindow(frame::WindowId),
    /// A new OS window, opening on the dashboard.
    NewFrame,
    CloseFrame,
    /// Move focus to another frame. Not a bare assignment to `focus_frame`:
    /// the live buffer belongs to the *focused* window, so switching frames
    /// has to park it and adopt the new frame's window, or clicking between
    /// two frames swaps the buffers they were showing.
    FocusFrame(usize),

    /// A git verb — `"status"`, `"stage"`, `"unstage"`, `"stage-all"`,
    /// `"unstage-all"`, `"commit"`, `"commit-finish"`, `"push"`, `"pull"`,
    /// `"refresh"`. Core has no git in it; the app runs these and feeds the
    /// result back as buffer text, the same shape as `OpenFile`.
    Git(String),

    /// A project verb — `"find-file"`, `"find-dir"`, `"switch"`, `"open"`,
    /// `"dired"`, `"compile"`, `"test"`, `"root"`, `"forget"`. Core has no
    /// filesystem in it.
    Project(String),
    /// A terminal verb — `"open"` or `"close"`. Core has no processes in it.
    Term(String),
    /// A keystroke bound for the shell rather than for the editor. Produced by
    /// `handle_key` in [`Mode::Terminal`] and consumed by the app, which owns
    /// the PTY; core never encodes it, since what a terminal wants for a given
    /// key is terminal knowledge.
    TermKey(Key),

    /// A dired verb — `"open"`, `"up"`, `"enter"`, `"mark"`, `"unmark"`,
    /// `"toggle-marks"`, `"flag-delete"`, `"execute"`, `"rename"`, `"copy"`,
    /// `"mkdir"`, `"toggle-hidden"`, `"refresh"`. Core has no filesystem in it.
    Dired(String),

    // --- dashboard, configured from Lisp ---
    SetDashboardBanner(String),
    /// The picture above the banner. `None` takes it away again.
    SetDashboardLogo(Option<ImageId>),
    ClearDashboardItems,
    AddDashboardItem {
        key: char,
        label: String,
        action: String,
    },

    // --- keymap, configured from Lisp ---
    BindKey {
        mode: String,
        keys: String,
        command: String,
    },
    /// Run a Lisp function by name. Produced by core, consumed by the app,
    /// which forwards it to the Lisp thread.
    CallLisp(String),

    // --- files ---
    OpenFile(PathBuf),
    /// Open a file and jump to a line, from a `path:line:text` hit — what
    /// ripgrep prints, and what `consult-ripgrep` picking a match means. One
    /// command rather than an open followed by a jump, because the jump has to
    /// happen after the file is read and core cannot do the reading.
    OpenAt(String),
    SaveFile(Option<PathBuf>),

    /// "The user has already said yes to this" — the answer to a
    /// [`PromptKind::Confirm`], carrying the command it was guarding.
    ///
    /// The wrapper exists because the guard lives at the same place that runs
    /// the action: a bare `SaveFile` coming back from the prompt would be
    /// checked again and ask again, forever. Wrapping is what lets one guarded
    /// site both ask the question and recognise its own answer, instead of every
    /// guarded command needing a twin variant that skips the check.
    Confirmed(Box<EditorCommand>),

    /// Change or remove an overlay. Deliberately *not* in
    /// [`EditorCommand::mutates_document`]: an overlay is drawing rather than
    /// text, so a face can be put on dired's listing or magit's status without
    /// the read-only guard refusing it.
    ///
    /// Making one is not here — it has to hand a handle back, which a command
    /// cannot do, so [`Editor::make_overlay`] is a method the way
    /// [`Editor::make_marker`] is.
    Overlay(OverlayEdit),

    // --- lisp-api: the variants the image could not work around --------------
    //
    // Each of these was a row of the "genuinely unreachable from Lisp" table in
    // `docs/boundary.org`. They are here rather than scattered because the enum
    // is the one file the Lisp side cannot route around, and because they share
    // an argument: none of them needs the app layer, so all of them are applied
    // on the spot and a read that follows one sees it.

    /// A buffer with no file behind it, named by the string — `*Messages*`,
    /// `*xref*`, a scratchpad. Switches to it if a buffer of that name is
    /// already open, so calling it twice is a *show*, not a second copy.
    ///
    /// Deliberately not [`BufferKind`]-flavoured: a generated buffer is
    /// read-only and re-rendered from state, and this one is an ordinary text
    /// buffer that Lisp owns. It is `*scratch*`, not `*magit*`.
    CreateBuffer(String),
    /// Kill the buffer at that index in [`Editor::buffer_names`] — 0 is the live
    /// one. The last remaining buffer is refused: Emacs always has one, and an
    /// editor showing nothing has no state to draw.
    ///
    /// No confirmation, ever, whatever the buffer's `modified` flag says. Asking
    /// is policy and policy is Lisp's — `(buffer-modified-p)` is a reader.
    KillBuffer(usize),
    /// The live buffer's tree-sitter language, or `None` for plain text. The
    /// missing writer beside [`EditorCommand::SetMajorMode`]: a mode is what
    /// Lisp dispatches on, a language is what gets highlighted, and a buffer
    /// created from Lisp had no way to ask for colour.
    SetLanguage(Option<String>),
    /// Refuse, or stop refusing, every edit to the live buffer.
    ///
    /// The writer that made [`BufferKind::is_generated`] stop being the only
    /// answer to "may I type here". A mode that renders rather than edits — the
    /// first is `org-frozen-mode` — sets it on entry and clears it on exit, and
    /// gets `i`, `x`, `p`, `u` and every Lisp `insert` refused for as long as it
    /// is on. See [`Buffer::read_only`] for why this is not a [`BufferKind`].
    ///
    /// Deliberately *not* [`EditorCommand::mutates_document`]: turning the flag
    /// off would then be the one edit a read-only buffer could never accept, and
    /// the mode could never be left.
    SetReadOnly(bool),
    /// Show a pixel-space page on the live buffer instead of its text, or —
    /// with `None` — go back to drawing the text.
    ///
    /// The whole of the scene integration, and there is only one verb because a
    /// scene is described in one form and swapped in whole: the alternative —
    /// mutate this node, now this one — is the API that makes every caller hold
    /// ids it has to keep in step with the document.
    ///
    /// Installing one claims the buffer read-only and clearing one gives the
    /// claim back; see [`Buffer::set_scene`], which is where that is spelled and
    /// why. No buffer is named because the live one is meant: `scene-set` is
    /// `(current-buffer)`-shaped in Lisp, and a command that could aim at a
    /// parked buffer would be the only one here that can.
    ///
    /// Deliberately *not* [`EditorCommand::mutates_document`]. Nothing about the
    /// text changes, and if it were, a buffer already showing a scene — and
    /// therefore already read-only — could never be handed a new one or have
    /// this one taken away.
    SetScene(Option<zemacs_gui::Scene>),

    /// Put a prompt up whose answer goes **back to Lisp**, as
    /// `(%prompt-reply ID ANSWER)` through [`EditorCommand::CallLisp`]. `id`
    /// names the continuation parked in the image's table; `completing` says
    /// whether to draw a candidate box, which the renderer asks
    /// [`PromptKind::completes`] and which cannot be inferred from the items
    /// because they arrive afterwards, one [`EditorCommand::PromptItem`] each.
    ///
    /// This is `read-string` and `completing-read`. It is a *continuation* and
    /// not a blocking read for the reason in `docs/threading.org`: the Lisp
    /// thread must never wait on the user, or a prompt would freeze the image
    /// the way a slow elisp function freezes Emacs.
    ReadFromMinibuffer {
        id: u64,
        label: String,
        completing: bool,
    },
    /// Append one candidate to the open Lisp prompt. Ignored for any other
    /// prompt — the file and grep pickers get their candidates from the app,
    /// and a stray item from a continuation that has already been cancelled
    /// must not land in somebody else's list.
    PromptItem(String),

    /// Append one `"KEY LABEL"` row to the which-key panel, or — with `None` —
    /// empty it. Rows arrive one at a time for the same reason
    /// [`EditorCommand::PromptItem`]'s do: the write envelope carries a single
    /// string, and a list is many.
    ///
    /// Deliberately *not* a prompt and deliberately not an overlay. A prompt
    /// eats the next keystroke and which-key's whole job is to help you aim one;
    /// an overlay anchors to a range of buffer text and moves with it, and a
    /// panel describing the keyboard is not about the document at all.
    WhichKey(Option<String>),

    // --- end of the lisp-api block -------------------------------------------
}

impl EditorCommand {
    /// True for anything that changes the text, or that is only a prelude to
    /// changing it. Used to keep generated buffers read-only.
    ///
    /// `SetMode(Insert)` counts: letting you into Insert on a buffer that
    /// refuses every keystroke afterwards is a worse experience than refusing
    /// the mode change itself.
    pub fn mutates_document(&self) -> bool {
        matches!(
            self,
            EditorCommand::InsertChar(_)
                | EditorCommand::InsertText(_)
                | EditorCommand::InsertNewline
                | EditorCommand::DeleteBackward
                | EditorCommand::DeleteForward
                | EditorCommand::DeleteRange(..)
                | EditorCommand::Paste { .. }
                | EditorCommand::Undo
                | EditorCommand::Redo
                | EditorCommand::Checkpoint
                | EditorCommand::SetMode(Mode::Insert)
        )
    }

    /// True for the commands `Editor::apply` cannot carry out alone — they need
    /// the filesystem, a subprocess, the Lisp image, or an OS window, all of
    /// which live in the app layer.
    ///
    /// This is the line the Lisp bridge splits on: everything else a primitive
    /// applies on the spot, so a read that follows a write sees it. These land
    /// on the next turn of the main loop instead, which is why
    /// `(find-file "x")` immediately followed by `(buffer-name)` still reports
    /// the old buffer. Making them synchronous would mean blocking Lisp on the
    /// UI thread, and that trade is the whole reason zemacs is not elisp.
    pub fn needs_app(&self) -> bool {
        matches!(
            self,
            EditorCommand::OpenFile(_)
                | EditorCommand::SaveFile(_)
                // Whatever it wraps was guarded by a layer that only the app
                // has — the filesystem, or git.
                | EditorCommand::Confirmed(_)
                | EditorCommand::Git(_)
                | EditorCommand::Dired(_)
                | EditorCommand::OpenAt(_)
                | EditorCommand::Project(_)
                | EditorCommand::Term(_)
                | EditorCommand::TermKey(_)
                | EditorCommand::CallLisp(_)
                | EditorCommand::CloseFrame
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub font_size: f32,
    pub background: [f32; 3],
    pub foreground: [f32; 3],
    pub line_numbers: bool,
    pub tab_width: usize,
    /// Where completing prompts (`M-x`, find-file, buffer switch) are drawn.
    pub completion_style: CompletionStyle,
    /// Modeline bevel, in pixels, following Emacs' `:box :line-width`:
    /// positive raises, **negative sinks** (highlight and shadow swap), zero is
    /// flat. The sign is the whole feature, so this is deliberately signed and
    /// deliberately not clamped at zero.
    pub modeline_relief: i32,
    /// Padding inside the modeline, in pixels.
    pub modeline_pad: i32,
    /// Truncate a too-wide line with a marker, or wrap it.
    pub line_overflow: LineOverflow,
    /// Count from the cursor rather than from the top of the file. Orthogonal
    /// to `line_numbers`, which is whether the gutter is drawn at all.
    pub relative_line_numbers: bool,
    /// Columns the text column is held to, centred in its pane — Emacs'
    /// `olivetti-mode`, and what makes a prose buffer read like a page rather
    /// than like a terminal. **0 is off** and means the full width of the pane,
    /// which is what every code buffer wants: indentation is structure, and
    /// centring it puts the structure in the middle of nowhere.
    ///
    /// In columns rather than pixels because a measure is a count of
    /// characters — "66 characters per line" is the typographic rule, and it
    /// stays true when the font size changes.
    pub text_width: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size: 18.0,
            background: [0.06, 0.06, 0.09],
            foreground: [0.86, 0.90, 1.00],
            line_numbers: true,
            tab_width: 4,
            completion_style: CompletionStyle::default(),
            modeline_relief: 2,
            modeline_pad: 8,
            line_overflow: LineOverflow::default(),
            relative_line_numbers: false,
            text_width: 0,
        }
    }
}

/// The major mode of a buffer nothing more specific applies to, as in Emacs.
pub const FUNDAMENTAL: &str = "fundamental-mode";

/// The major mode for a language id, by the same convention Emacs uses:
/// `"rust"` -> `"rust-mode"`. `None` means [`FUNDAMENTAL`].
pub fn major_mode_for(language: Option<&str>) -> String {
    match language {
        Some(l) => format!("{l}-mode"),
        None => FUNDAMENTAL.to_string(),
    }
}

/// What a buffer is for. The dashboard is a buffer rather than a mode so it
/// shows up in the buffer switcher and can be put in any window, exactly like
/// Emacs' `*scratch*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferKind {
    Text,
    Scratch,
    Dashboard,
    /// The git status buffer. Its text is generated, so it is never saved.
    Magit,
    /// A commit message being written. `C-c C-c` finishes it.
    CommitMessage,
    /// A directory listing.
    Dired,
    /// A shell. Its text is a flattening of the terminal grid, rewritten every
    /// time the child prints; the grid itself is what gets drawn.
    Terminal,
}

impl BufferKind {
    /// The editing mode a buffer of this kind is entered in.
    pub fn mode(self) -> Mode {
        match self {
            BufferKind::Dashboard => Mode::Dashboard,
            BufferKind::Magit => Mode::Magit,
            BufferKind::Dired => Mode::Dired,
            BufferKind::Terminal => Mode::Terminal,
            _ => Mode::Normal,
        }
    }

    /// Generated buffers have no file behind them and must never be written.
    pub fn is_generated(self) -> bool {
        matches!(
            self,
            BufferKind::Dashboard | BufferKind::Magit | BufferKind::Dired | BufferKind::Terminal
        )
    }
}

/// Why a buffer refuses an edit, or [`ReadOnly::No`].
///
/// Two reasons and they are genuinely different, which is the whole reason this
/// is not a bool: a *generated* buffer has no document to edit — its text is a
/// rendering of state and the next refresh would throw an edit away — while a
/// buffer Lisp has *frozen* has a perfectly good file behind it and is refusing
/// on purpose. The user needs to be told which, because one of them is
/// something they can turn off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOnly {
    No,
    /// `*dashboard*`, `*magit*`, dired, a terminal.
    Generated,
    /// [`EditorCommand::SetReadOnly`] — org-frozen, and anything else that
    /// wants a file on screen without a way to type into it.
    Claimed,
}

/// A text document plus a cursor expressed as a character index into the rope.
pub struct Buffer {
    /// Stable handle. Windows refer to buffers by this, never by index.
    pub id: BufferId,
    pub kind: BufferKind,
    /// lisp-api: this buffer refuses every edit, because a mode said so.
    ///
    /// Per buffer and not per mode, for the reason [`Buffer::line_numbers`]
    /// gives: a split routinely shows two documents, and "the mode the image
    /// last entered" is not a fact about the one you are typing into. A mode
    /// sets it on entry and clears it on exit, exactly as it would a claim on a
    /// setting, and the flag travels with the buffer between
    /// [`Editor::buffer`] and [`Editor::others`] like the undo history does.
    ///
    /// Deliberately *not* folded into [`BufferKind`]. A generated buffer is a
    /// view with no document behind it; this is an ordinary file buffer whose
    /// mode has decided you are reading rather than writing, and it must still
    /// save, still highlight, and still be an honest `.org` file when the mode
    /// is turned off. See [`ReadOnly`].
    pub read_only: bool,
    pub text: Rope,
    pub cursor: usize,
    pub path: Option<PathBuf>,
    /// lisp-api: the name Lisp asked for, for a buffer with no file behind it.
    /// Outranks everything in [`Buffer::name`] — a `*Messages*` made by
    /// `create-buffer` would otherwise be called `*untitled*` like every other
    /// pathless text buffer. `None` for every buffer the editor made itself, so
    /// the existing naming rules are untouched.
    pub given_name: Option<String>,
    pub modified: bool,
    /// Language id for tree-sitter, e.g. `"rust"`, `"lisp"`. `None` = plain text.
    pub language: Option<String>,
    /// The buffer's major mode: `"org-mode"`, `"rust-mode"`, … Exactly one,
    /// derived from the file name unless Lisp sets it. This is a *different*
    /// axis from [`Mode`], which is the modal editing state — a buffer is in
    /// org-mode whether you are in Normal or Insert.
    pub major_mode: String,
    /// Whether *this buffer* shows a line-number gutter. `None` follows the
    /// editor-wide [`Settings::line_numbers`].
    ///
    /// Per buffer and not per editor, because the gutter is drawn per *pane*
    /// and two panes routinely show different kinds of thing. The mode-local
    /// machinery in `runtime/modes/modes.lisp` cannot express that: its
    /// settings are global by construction, so `org-mode` claiming the gutter
    /// off took it off in the code buffer beside it too, and which pane won
    /// depended on which mode was entered last.
    ///
    /// Recomputed whenever the buffer's major mode is set — see
    /// [`Editor::gutter_for_mode`] — so it is a cache of a decision, never a
    /// second place to state one.
    pub line_numbers: Option<bool>,
    /// Minor modes, on top of the major one. Order is the order enabled.
    pub minor_modes: Vec<String>,
    /// Scroll position, parked here while another buffer is on screen.
    pub saved_scroll: usize,
    /// Unix mode bits of the file behind this buffer, for the modeline. Set by
    /// the app when the file is read or written; core cannot stat anything.
    pub file_mode: Option<u32>,
    /// The file's modification time as of the last read or write *by this
    /// buffer* — Emacs' `visited-file-modtime`, and the answer to "has anyone
    /// else touched this since I last looked?"
    ///
    /// Set by the app for the same reason [`Buffer::file_mode`] is: core cannot
    /// stat anything. It lives on the buffer rather than in a side table keyed
    /// by path because the question is about *this document's* view of the file
    /// — two buffers on one path can honestly disagree about when they last
    /// synced, and a path-keyed table would have to pick one of them.
    ///
    /// `None` for a buffer with no file behind it yet, which is what makes
    /// `:w newfile` skip the check instead of asking about a file nobody has
    /// read.
    pub visited: Option<std::time::SystemTime>,
    /// Syntax spans for *this* buffer's text, recomputed by the app.
    ///
    /// Per buffer rather than per editor, and that is the whole point: a split
    /// showing two files draws both, and parking a buffer keeps its colours
    /// rather than throwing them away because some other buffer became live.
    /// Only the live buffer can be edited, so a parked buffer's spans stay
    /// valid for as long as it is parked.
    pub highlights: Vec<Span>,
    /// Undo history lives with the buffer, not the editor: switching files
    /// must not let one buffer's `u` restore another's text.
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Markers into *this* text, and they travel with it: the buffer is moved
    /// between [`Editor::buffer`] and [`Editor::others`] on every switch, so
    /// riding along is what keeps a marker naming the document it was made in.
    markers: marker::Markers,
    /// Overlays over *this* text, travelling with it for the same reason, and
    /// adjusted by the same `splice`. Per buffer rather than per editor, exactly
    /// as the undo history is — an overlay is about a document, not about the
    /// editor that happens to be showing it.
    overlays: overlay::Overlays,
    /// A pixel-space page drawn *instead of* this buffer's text, or `None` for
    /// every buffer that is a grid of cells — which is nearly all of them.
    ///
    /// On the buffer rather than on the [`Window`], and that is the whole
    /// integration: a scene follows its document from pane to pane, which is
    /// what a split showing the same document twice needs; the modeline, the
    /// buffer list, `switch-to-buffer` and killing all keep working unchanged,
    /// because it is still a buffer; and there is no id space and no editor-side
    /// table, because Lisp never names a scene, it names a buffer. See
    /// [`scene`] for the argument written out.
    ///
    /// Set only through [`EditorCommand::SetScene`], which is also what claims
    /// the buffer read-only: a scene is not an editing surface, and a document
    /// you could type into while looking at a rendering of it would be two
    /// documents.
    pub scene: Option<zemacs_gui::Scene>,
    /// What [`Buffer::read_only`] said before a scene claimed this buffer.
    ///
    /// A scene freezes the buffer it is installed on, so taking one away has to
    /// hand back whatever was there before rather than simply thawing: a mode
    /// that shows a page over a file `org-frozen` had already frozen must not
    /// unfreeze it on the way out. Meaningless while [`Buffer::scene`] is
    /// `None`, which is why it is private — nothing outside this file should be
    /// reading a flag that is only half of a pair.
    read_only_before_scene: bool,
}

impl Buffer {
    pub fn from_str(s: &str) -> Self {
        Self {
            id: 0,
            kind: BufferKind::Text,
            read_only: false,
            text: Rope::from_str(s),
            cursor: 0,
            path: None,
            given_name: None,
            modified: false,
            language: None,
            // Follow the editor until something decides otherwise, which is
            // what makes a buffer nobody has an opinion about look like every
            // other one.
            line_numbers: None,
            major_mode: FUNDAMENTAL.into(),
            minor_modes: Vec::new(),
            saved_scroll: 0,
            file_mode: None,
            visited: None,
            highlights: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            markers: marker::Markers::default(),
            overlays: overlay::Overlays::default(),
            scene: None,
            read_only_before_scene: false,
        }
    }

    /// Show a scene instead of this buffer's text, or — with `None` — stop.
    ///
    /// The read-only flag moves with it, because `docs/gui.org` says a scene is
    /// not an editing surface and this is where that is enforced: installing
    /// one claims the buffer exactly as `org-frozen` does, and clearing one
    /// hands back the claim the buffer had before, so a page shown over a file
    /// does not leave the file frozen once the page is gone.
    ///
    /// Idempotent in both directions. Replacing one scene with the next does
    /// *not* re-record the flag — a mode that rebuilds its page whenever
    /// anything changes would otherwise record the read-only it imposed itself
    /// and could never let go — and clearing a scene that was never there
    /// leaves the flag alone, because a mode's exit hook runs whether or not
    /// its entry did.
    ///
    /// **A swap carries the reader's place across.** A scene is built whole and
    /// swapped in, which is the right API and would otherwise mean that a
    /// curriculum re-rendering because one problem's state changed throws
    /// somebody on page nine back to page one. So replacing a scene keeps the
    /// outgoing offset and ignores whatever the incoming one carried; clearing
    /// and installing later starts at the top, which is right, because that is
    /// a different document arriving rather than the same one re-rendered.
    fn set_scene(&mut self, scene: Option<zemacs_gui::Scene>) {
        match (self.scene.is_some(), scene) {
            (false, Some(s)) => {
                self.read_only_before_scene = self.read_only;
                self.read_only = true;
                self.scene = Some(s);
            }
            (true, Some(mut s)) => {
                // Carried raw and deliberately *not* clamped: the new page may
                // be shorter than the offset, and the height that would say so
                // belongs to a laid-out scene, which needs a font this crate
                // cannot see. So this number is a request rather than a
                // position until the app has clamped it — see `scroll_scene` in
                // `crates/app`, which is where every write of a scroll offset on
                // that side goes through.
                s.scroll = self.scene.as_ref().map_or(0, |old| old.scroll);
                self.scene = Some(s);
            }
            (true, None) => {
                self.scene = None;
                self.read_only = self.read_only_before_scene;
            }
            (false, None) => {}
        }
    }

    /// Why this buffer refuses an edit, if it does.
    ///
    /// The one predicate, asked by [`Editor::apply`], by the `i` guard in
    /// `evil.rs` and by `(buffer-read-only-p)` — the same reason
    /// [`overlay::fold_hiding`] is one predicate. A buffer the renderer draws
    /// as read-only and a buffer the keyboard treats as read-only must be the
    /// same buffer, or the modeline lies.
    pub fn read_only(&self) -> ReadOnly {
        match (self.kind.is_generated(), self.read_only) {
            (true, _) => ReadOnly::Generated,
            (false, true) => ReadOnly::Claimed,
            (false, false) => ReadOnly::No,
        }
    }

    /// Every overlay on this buffer, in creation order — which is also the
    /// order the renderer resolves conflicts in, most recent winning.
    ///
    /// Public where the markers are not, because the renderer draws *inactive*
    /// panes too and reaches their buffers through
    /// [`Editor::buffer_by_id`](Editor::buffer_by_id).
    pub fn overlays(&self) -> &[overlay::Overlay] {
        self.overlays.all()
    }

    /// Display name for the status line and the buffer switcher.
    pub fn name(&self) -> String {
        // lisp-api: a name Lisp gave wins over both, because a buffer it made
        // has neither a path nor a kind of its own to be named after.
        if let Some(name) = &self.given_name {
            return name.clone();
        }
        match (&self.path, self.kind) {
            (Some(p), _) => p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string()),
            (None, BufferKind::Dashboard) => "*dashboard*".into(),
            (None, BufferKind::Scratch) => "*scratch*".into(),
            (None, BufferKind::Magit) => "*magit*".into(),
            (None, BufferKind::Dired) => "*dired*".into(),
            (None, BufferKind::Terminal) => "*terminal*".into(),
            (None, BufferKind::CommitMessage) => "COMMIT_EDITMSG".into(),
            (None, BufferKind::Text) => "*untitled*".into(),
        }
    }

    /// True for a throwaway buffer that opening a file should replace rather
    /// than stack behind. `*dashboard*` and `*scratch*` are never throwaway,
    /// however empty they look — they are named buffers you can come back to.
    fn is_pristine(&self) -> bool {
        self.kind == BufferKind::Text
            && self.path.is_none()
            // lisp-api: and neither is a buffer Lisp named. `(create-buffer
            // "*xref*")` then `find-file` must not quietly reuse the buffer
            // whose name something is about to look up.
            && self.given_name.is_none()
            && !self.modified
            && self.len_chars() == 0
    }

    /// Move one step through history, pushing the current state onto the other
    /// stack. `false` when that stack is empty.
    fn step_history(&mut self, undo: bool) -> bool {
        let (from, to) = if undo {
            (&mut self.undo, &mut self.redo)
        } else {
            (&mut self.redo, &mut self.undo)
        };
        let Some(snap) = from.pop() else {
            return false;
        };
        to.push(Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        });
        self.text = snap.text;
        self.cursor = snap.cursor.min(self.text.len_chars());
        // A snapshot is a whole rope, not a diff, so there is no edit to run the
        // markers through: they keep the positions they had and are pulled back
        // inside the restored text. Undoing the very edit that moved a marker
        // therefore leaves it approximately rather than exactly where it was —
        // but never outside the document, which is the invariant a marker owes
        // its holder. ponytail: exact would mean recording edits instead of
        // snapshots, which is a different undo system.
        self.markers.clamp(self.text.len_chars());
        // Same argument, and one more consequence: an overlay clamped down to
        // nothing is dropped, so undoing the insertion of a LaTeX fragment takes
        // its preview with it rather than leaving an image over other text.
        self.overlays.clamp(self.text.len_chars());
        self.modified = true;
        true
    }

    pub fn len_chars(&self) -> usize {
        self.text.len_chars()
    }

    pub fn len_lines(&self) -> usize {
        self.text.len_lines()
    }

    /// (line, column) of the cursor, both zero-based.
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let line = self.text.char_to_line(self.cursor.min(self.len_chars()));
        let line_start = self.text.line_to_char(line);
        (line, self.cursor - line_start)
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.text.line_to_char(line.min(self.len_lines().saturating_sub(1)))
    }

    /// Number of characters on `line`, excluding the trailing newline.
    pub fn line_len(&self, line: usize) -> usize {
        if line >= self.len_lines() {
            return 0;
        }
        let slice = self.text.line(line);
        let mut n = slice.len_chars();
        if n > 0 && slice.char(n - 1) == '\n' {
            n -= 1;
        }
        n
    }

    /// Char index just past the last character of `line`, newline excluded.
    pub fn line_end(&self, line: usize) -> usize {
        self.line_start(line) + self.line_len(line)
    }

    pub fn char_at(&self, i: usize) -> Option<char> {
        (i < self.len_chars()).then(|| self.text.char(i))
    }

    /// First non-whitespace column of `line`.
    pub fn first_non_blank(&self, line: usize) -> usize {
        let start = self.line_start(line);
        let len = self.line_len(line);
        for i in 0..len {
            if !self.text.char(start + i).is_whitespace() {
                return start + i;
            }
        }
        start
    }

    /// Replace `removed` characters at `start` with `text` — the only place the
    /// rope is edited, so it is also the only place markers have to be moved.
    ///
    /// Every editing operation is a splice, and funnelling them here rather than
    /// adjusting at each call site is the point: a mutation path that forgets to
    /// adjust is exactly the bug markers exist to prevent, and one that forgets
    /// to *splice* does not compile.
    fn splice(&mut self, start: usize, removed: usize, text: &str) {
        let start = start.min(self.len_chars());
        let removed = removed.min(self.len_chars() - start);
        if removed > 0 {
            self.text.remove(start..start + removed);
        }
        if !text.is_empty() {
            self.text.insert(start, text);
        }
        let inserted = text.chars().count();
        self.markers.adjust(start, removed, inserted);
        self.overlays.adjust(start, removed, inserted);
        self.modified = true;
    }

    fn insert_char(&mut self, c: char) {
        self.insert_text(c.encode_utf8(&mut [0; 4]));
    }

    fn insert_text(&mut self, s: &str) {
        let at = self.cursor.min(self.len_chars());
        self.splice(at, 0, s);
        self.cursor = at + s.chars().count();
    }

    fn delete_backward(&mut self) {
        if self.cursor > 0 {
            self.splice(self.cursor - 1, 1, "");
            self.cursor -= 1;
        }
    }

    fn delete_forward(&mut self) {
        let (line, col) = self.cursor_line_col();
        if col < self.line_len(line) {
            self.splice(self.cursor, 1, "");
        }
    }

    fn delete_range(&mut self, start: usize, end: usize) {
        let n = self.len_chars();
        let (start, end) = (start.min(n), end.min(n));
        if start >= end {
            return;
        }
        self.splice(start, end - start, "");
        self.cursor = start.min(self.len_chars());
    }

    /// The last complete top-level `(...)` form ending at or before `pos`.
    ///
    /// Scanned *forward* from the start of the buffer rather than backward from
    /// the cursor, because `;` comments can only be recognised by reading a
    /// line from its beginning — scanning backward, a `)` inside a comment is
    /// indistinguishable from a real one.
    pub fn last_top_level_form(&self, pos: usize) -> Option<(usize, usize)> {
        let pos = pos.min(self.len_chars());
        let (mut depth, mut start) = (0usize, 0usize);
        let (mut in_string, mut in_comment, mut escaped) = (false, false, false);
        let mut found = None;

        for (i, c) in self.text.chars().take(pos).enumerate() {
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' if in_string => escaped = true,
                '"' if !in_comment => in_string = !in_string,
                '\n' if in_comment => in_comment = false,
                ';' if !in_string => in_comment = true,
                _ if in_string || in_comment => {}
                '(' => {
                    if depth == 0 {
                        start = i;
                    }
                    depth += 1;
                }
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        found = Some((start, i + 1));
                    }
                }
                _ => {}
            }
        }
        found
    }

    pub fn slice_string(&self, start: usize, end: usize) -> String {
        let n = self.len_chars();
        let (start, end) = (start.min(n), end.min(n));
        if start >= end {
            return String::new();
        }
        self.text.slice(start..end).to_string()
    }

    fn move_cursor(&mut self, dir: Direction) {
        let (line, col) = self.cursor_line_col();
        match dir {
            Direction::Left => {
                if col > 0 {
                    self.cursor -= 1;
                }
            }
            Direction::Right => {
                if col < self.line_len(line) {
                    self.cursor += 1;
                }
            }
            Direction::Up => {
                if line > 0 {
                    self.move_to_line_col(line - 1, col);
                }
            }
            Direction::Down => {
                if line + 1 < self.len_lines() {
                    self.move_to_line_col(line + 1, col);
                }
            }
        }
    }

    pub fn move_to_line_col(&mut self, line: usize, col: usize) {
        let line = line.min(self.len_lines().saturating_sub(1));
        let target_col = col.min(self.line_len(line));
        self.cursor = self.line_start(line) + target_col;
    }
}

/// One point in the undo history.
struct Snapshot {
    text: Rope,
    cursor: usize,
}

/// The whole editor model: document, mode, settings, and ephemeral input state.
pub struct Editor {
    /// The buffer on screen. Kept as a plain field (rather than an index into
    /// a list) so the renderer and every motion stay written against one
    /// obvious thing.
    pub buffer: Buffer,
    /// The other open buffers, most recently visited first.
    pub others: Vec<Buffer>,
    /// One per OS window. Always at least one.
    pub frames: Vec<frame::Frame>,
    pub focus_frame: usize,
    next_buffer_id: BufferId,
    next_marker_id: MarkerId,
    next_overlay_id: OverlayId,
    /// Every bitmap an overlay can point at, keyed by what produced it — a hash
    /// of the LaTeX source, the dpi and the colour, so previewing the same
    /// fragment twice is one entry and the renderer's texture for it survives.
    ///
    /// Per *editor* rather than per buffer: it is a cache of rendered pixels,
    /// like the theme is a table of colours, and nothing about it belongs to one
    /// document. ponytail: nothing evicts. The set is bounded by the distinct
    /// fragments a session actually previews, which is small — the day it is
    /// not, drop entries no buffer's overlays still name.
    images: HashMap<ImageId, Image>,
    /// The em, in the device pixels the renderer draws with — `font_size` times
    /// the display's scale factor, which core cannot know and the renderer parks
    /// here once a frame, exactly as it does `viewport_lines`. A LaTeX preview
    /// has to be rasterised at this size or it comes out half-height on a
    /// Retina display.
    pub font_px: f32,
    /// Command names published by the Lisp image, for `M-x` completion.
    pub commands: Vec<String>,
    /// Mode hooks waiting to be run. Core records that a hook is due; the app
    /// drains this and asks the Lisp image to run each one, since core cannot
    /// call Lisp itself.
    pub pending_hooks: Vec<String>,
    pub mode: Mode,
    pub settings: Settings,
    pub theme: Theme,
    pub dashboard: Dashboard,

    /// User keymap: (mode, "g d") -> Lisp function name. Consulted before the
    /// built-in Evil grammar, which is how Lisp config wins.
    pub keymap: HashMap<(Mode, String), String>,
    /// Bindings for a *major or minor* mode, keyed by its name. `define-key`
    /// picks this map when the mode name is not an editing mode, so
    /// `(define-key "org-mode" ...)` needs no new primitive.
    pub mode_keymap: HashMap<(String, String), String>,
    /// Major modes whose buffers show no gutter, lowercased. Set from Lisp; see
    /// [`Editor::gutter_for_mode`].
    pub no_gutter_modes: Vec<String>,

    /// `Some` while the `:` or `/` prompt is active.
    pub prompt: Option<Prompt>,
    /// What a [`PromptKind::Confirm`] runs if the answer is `yes`. Parked here
    /// for exactly as long as that prompt is open — [`Editor::confirm`] sets it,
    /// and both accepting and cancelling take it back out, so a question that
    /// was answered "no" cannot leave a loaded gun behind for the next one.
    pub pending_confirm: Option<Box<EditorCommand>>,
    /// Last message, shown in the status line.
    pub status: String,
    /// Every message, oldest first, capped at [`MESSAGE_LIMIT`]. The status line
    /// only ever shows the last one, so this is the only record that a message
    /// which was immediately replaced was ever produced at all.
    pub messages: Vec<String>,
    pub should_quit: bool,

    /// Bumped on every document mutation; the app re-highlights when it moves.
    pub revision: u64,

    /// First visible line, and how many lines fit (set by the renderer).
    pub scroll: usize,
    pub viewport_lines: usize,
    /// Cells of text the focused window has across — the renderer parks it here
    /// every frame beside `viewport_lines`, and for the same reason: a *visual*
    /// line is a row of cells, and how many cells fit is a fact about a font and
    /// a pane that core cannot possibly know.
    ///
    /// This is the entire channel by which layout reaches the command loop. `0`
    /// means nothing has drawn yet, which every reader takes as "no wrapping" —
    /// so a headless `Editor` (every test, every Lisp-only path) moves by buffer
    /// line exactly as it always did.
    pub wrap_cols: usize,

    /// Evil pending state: counts, operators, multi-key prefixes.
    pub(crate) pending: evil::Pending,
    /// Anchor of the visual selection.
    pub(crate) visual_anchor: Option<usize>,
    /// Window labels waiting to be picked — `ace-window` is up. The renderer
    /// draws these over each pane; the next key chooses one.
    pub ace: Option<Vec<(char, WindowId)>>,
    /// which-key panel: one `"KEY LABEL"` row per continuation of the key
    /// sequence currently half-typed, composed by the image. Empty means no
    /// panel, which is every frame in which nothing is pending.
    ///
    /// Filed beside `ace` rather than beside `prompt`, and that placement is the
    /// design: this is a *label drawn over the frame*, like the ace-window
    /// letters, and not a thing that owns the keyboard. A prompt would swallow
    /// the next keystroke, and the next keystroke is exactly what which-key
    /// exists to help you choose.
    ///
    /// Core only ever *empties* it — [`Editor::handle_key`], the moment nothing
    /// is pending any more. Filling it is the image's job, because what a prefix
    /// means is Lisp's opinion and not core's.
    pub which_key: Vec<String>,
    /// Column a run of `j`/`k` is trying to hold.
    ///
    /// Without it, passing through a short line permanently forgets how far
    /// right you were — and a block selection spanning a ragged region is
    /// impossible, because getting to the far side clamps the column on the way.
    pub(crate) desired_col: Option<usize>,

    register: String,
    register_linewise: bool,
    /// clipboard: bumped on every write to the unnamed register, so the app
    /// layer can tell "the register changed" from "the register is big" with an
    /// integer compare rather than by diffing the text once a frame.
    register_revision: u64,
    last_search: String,
    /// vim-agent: named registers, macros and marks. One field rather than
    /// five, and the struct lives in [`evil`] with everything else that reads
    /// it — none of this is the document, so none of it belongs to `apply`.
    pub(crate) vim: evil::Vim,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

const UNDO_LIMIT: usize = 500;
/// How many messages [`Editor::messages`] keeps before dropping the oldest.
pub const MESSAGE_LIMIT: usize = 500;

impl Editor {
    pub fn new() -> Self {
        // Buffer 0 is *dashboard*, buffer 1 is *scratch*. Both are real
        // buffers, so both appear in the switcher.
        let mut dash = Buffer::from_str("");
        dash.kind = BufferKind::Dashboard;
        let mut scratch = Buffer::from_str("");
        scratch.id = 1;
        scratch.kind = BufferKind::Scratch;
        scratch.language = Some("lisp".into());

        Self {
            buffer: dash,
            others: vec![scratch],
            pending_confirm: None,
            frames: vec![frame::Frame::new(0)],
            focus_frame: 0,
            next_buffer_id: 2,
            // 1, so 0 is never a live marker and whatever Lisp coerces to zero
            // reads as "gone" rather than as somebody else's position.
            next_marker_id: 1,
            next_overlay_id: 1,
            images: HashMap::new(),
            font_px: Settings::default().font_size,
            commands: Vec::new(),
            pending_hooks: Vec::new(),
            mode: Mode::Dashboard,
            settings: Settings::default(),
            theme: Theme::default(),
            dashboard: Dashboard::default(),
            keymap: HashMap::new(),
            mode_keymap: HashMap::new(),
            no_gutter_modes: Vec::new(),
            prompt: None,
            status: String::from("zemacs — Common Lisp inside."),
            messages: Vec::new(),
            should_quit: false,
            revision: 0,
            scroll: 0,
            viewport_lines: 24,
            wrap_cols: 0,
            pending: evil::Pending::default(),
            visual_anchor: None,
            ace: None,
            which_key: Vec::new(), // which-key panel
            desired_col: None,
            register: String::new(),
            register_linewise: false,
            register_revision: 0,
            last_search: String::new(),
            vim: evil::Vim::default(), // vim-agent
        }
    }

    /// Put a generated buffer on screen with `text`, creating it if this is the
    /// first time and reusing it afterwards — one `*magit*`, not one per
    /// refresh. The mode follows the kind, so the magit keymap comes with it.
    ///
    /// The cursor line is preserved across a refresh where it still exists,
    /// which is what stops the cursor jumping to the top every time you stage
    /// something.
    pub fn show_special(&mut self, kind: BufferKind, text: &str) {
        self.show_named(kind, None, text);
    }

    // --- term-agent: generated buffers that come in more than one -------------

    /// [`Editor::show_special`], addressed by *name* as well as by kind.
    ///
    /// One `*magit*` is the right answer for a rendered view of one repository.
    /// It is the wrong answer for a terminal: a shell, a `claude` session and an
    /// `opencode` session are three live children, and folding them onto one
    /// buffer is what made the second one replace the first. So the match is on
    /// `(kind, given_name)` — `None` keeps the old behaviour exactly, which is
    /// why every existing caller is untouched, and `Some(name)` gives one buffer
    /// per session that `buffer-list` and the switcher find like any other.
    ///
    /// Answers the buffer's id, because the caller running a process needs a
    /// handle to route keystrokes and screen refreshes by.
    pub fn show_named(&mut self, kind: BufferKind, name: Option<&str>, text: &str) -> BufferId {
        let wanted = |b: &Buffer| {
            b.kind == kind && (name.is_none() || b.given_name.as_deref() == name)
        };
        let live = wanted(&self.buffer);
        let line = if live {
            self.buffer.cursor_line_col().0
        } else {
            0
        };
        if !live {
            match self.others.iter().position(wanted) {
                Some(i) => self.switch_buffer(i + 1),
                None => {
                    self.sync_window();
                    self.buffer.saved_scroll = self.scroll;
                    let mut fresh = Buffer::from_str("");
                    fresh.id = self.next_buffer_id;
                    fresh.kind = kind;
                    self.next_buffer_id += 1;
                    let previous = std::mem::replace(&mut self.buffer, fresh);
                    self.others.insert(0, previous);
                }
            }
        }
        // Set on every call, not only on creation: `name()` reads it, and a
        // session buffer that lost its name would answer `*terminal*` like all
        // the others and stop being tellable apart in the switcher.
        if let Some(name) = name {
            self.buffer.given_name = Some(name.to_string());
        }
        self.buffer.kind = kind;
        self.buffer.text = Rope::from_str(text);
        self.buffer.modified = false;
        self.buffer.undo.clear();
        self.buffer.redo.clear();
        // Regenerated text, so every marker into the old listing names a line
        // that may not even be there any more. Overlays go the same way: dired
        // and magit put their faces back on every refresh anyway.
        self.buffer.markers.clear();
        self.buffer.overlays.clear();
        self.buffer.move_to_line_col(line, 0);
        self.mode = kind.mode();
        self.buffer.highlights.clear();
        self.revision += 1;

        let (id, cursor) = (self.buffer.id, self.buffer.cursor);
        let w = self.frame_mut().current_window_mut();
        w.buffer = id;
        w.cursor = cursor;
        id
    }

    /// Replace buffer `id`'s text with `text`, keeping point on the line it was
    /// on. False when nothing has that id.
    ///
    /// This is the revert path for a buffer that is *not* live. `Editor::apply`
    /// only ever touches `self.buffer`, which is exactly why auto-revert watched
    /// one buffer and why that stopped being good enough: a coding agent
    /// rewrites six files in a burst, and finding out about them one buffer
    /// switch at a time is not "quickly".
    ///
    /// It is the `EditorCommand::Revert(BufferId)` the ponytail note in the app
    /// asked for, written as a *method* rather than a variant — it performs no
    /// effect the pure core cannot, so it has no business on the command channel
    /// and no business being a keystroke's worth of latency away.
    ///
    /// Goes through `splice` like every other edit, so markers and overlays into
    /// this buffer are adjusted rather than left naming offsets in text that no
    /// longer exists. Point moves by *line and column*, not by offset: the whole
    /// premise is that the text moved, so an offset would drift by however many
    /// characters were inserted above the cursor, and the line someone was
    /// reading is what they mean by "where I was".
    pub fn revert_buffer(&mut self, id: BufferId, text: &str) -> bool {
        let Some(buffer) = std::iter::once(&mut self.buffer)
            .chain(self.others.iter_mut())
            .find(|b| b.id == id)
        else {
            return false;
        };
        let (line, col) = buffer.cursor_line_col();
        let end = buffer.len_chars();
        buffer.splice(0, end, text);
        buffer.move_to_line_col(line, col);
        // `splice` sets it and is wrong to: the buffer now holds exactly what is
        // on disk, which is the definition of unmodified.
        buffer.modified = false;
        // Spans describe text that is gone. The live buffer gets a fresh parse
        // from the revision bump below; a parked one is colourless until it is
        // looked at, which beats colouring the wrong characters.
        buffer.highlights.clear();
        self.revision += 1;
        true
    }

    // --- end of the term-agent block ------------------------------------------

    /// The focused frame.
    pub fn frame(&self) -> &frame::Frame {
        &self.frames[self.focus_frame.min(self.frames.len() - 1)]
    }

    pub fn frame_mut(&mut self) -> &mut frame::Frame {
        let i = self.focus_frame.min(self.frames.len() - 1);
        &mut self.frames[i]
    }

    fn dashboard_buffer_id(&self) -> BufferId {
        std::iter::once(&self.buffer)
            .chain(self.others.iter())
            .find(|b| b.kind == BufferKind::Dashboard)
            .map(|b| b.id)
            .unwrap_or(0)
    }

    /// Any buffer by handle — the renderer needs this to draw the *inactive*
    /// windows, whose buffers are not `self.buffer`.
    pub fn buffer_by_id(&self, id: BufferId) -> Option<&Buffer> {
        std::iter::once(&self.buffer)
            .chain(self.others.iter())
            .find(|b| b.id == id)
    }

    /// Park the live cursor and scroll onto the focused window so every window
    /// can be read the same way.
    ///
    /// The focused window's position lives in `buffer.cursor`/`self.scroll`
    /// while it is being edited; the others keep theirs in their own `Window`.
    /// The renderer would otherwise need to special-case the focused pane, so
    /// the app calls this once per frame before drawing.
    pub fn sync_focused_window(&mut self) {
        self.sync_window();
    }

    /// Park the live cursor and scroll on the focused window. Must run before
    /// anything changes which window is focused.
    fn sync_window(&mut self) {
        let (cursor, scroll, lines, cols, id) = (
            self.buffer.cursor,
            self.scroll,
            self.viewport_lines,
            self.wrap_cols,
            self.buffer.id,
        );
        let w = self.frame_mut().current_window_mut();
        w.cursor = cursor;
        w.scroll = scroll;
        w.viewport_lines = lines;
        w.wrap_cols = cols;
        w.buffer = id;
    }

    /// Make the focused window's buffer and position the live ones. The
    /// inverse of [`Editor::sync_window`].
    fn adopt_window(&mut self) {
        let w = self.frame().current_window().clone();
        if w.buffer != self.buffer.id {
            if let Some(i) = self.others.iter().position(|b| b.id == w.buffer) {
                let incoming = self.others.remove(i);
                let outgoing = std::mem::replace(&mut self.buffer, incoming);
                self.others.insert(0, outgoing);
            }
        }
        self.buffer.cursor = w.cursor.min(self.buffer.len_chars());
        self.scroll = w.scroll;
        self.viewport_lines = w.viewport_lines;
        self.wrap_cols = w.wrap_cols;
        // Deliberately *not* clearing highlights: this is a buffer swap, not
        // an edit, and the incoming buffer's spans still describe its own
        // unchanged text. Clearing here is what made a split lose its colours
        // the moment focus moved between panes.  The revision bump below still
        // invalidates any parse that was in flight for the outgoing buffer.
        self.revision += 1;
        self.mode = self.buffer.kind.mode();
    }

    /// Open buffer names, active first — the candidate list for the switcher.
    /// The ids behind [`Editor::buffer_names`], in the same order.
    ///
    /// The switcher needs these because [`Editor::switch_buffer`] takes an index
    /// and *reorders the list as it goes* — so an index is only good until the
    /// next switch, which is exactly one keystroke when the switcher previews.
    pub fn buffer_ids(&self) -> Vec<BufferId> {
        std::iter::once(&self.buffer)
            .chain(self.others.iter())
            .map(|b| b.id)
            .collect()
    }

    /// Switch to the buffer with `id`, wherever it has drifted to. A no-op when
    /// it is already live, which is what keeps a preview from churning the
    /// most-recently-used order on every arrow key.
    pub fn switch_buffer_id(&mut self, id: BufferId) {
        if self.buffer.id == id {
            return;
        }
        if let Some(i) = self.others.iter().position(|b| b.id == id) {
            self.switch_buffer(i + 1);
        }
    }

    pub fn buffer_names(&self) -> Vec<String> {
        std::iter::once(&self.buffer)
            .chain(self.others.iter())
            .map(|b| {
                let mark = if b.modified { " [+]" } else { "" };
                format!("{}{mark}", b.name())
            })
            .collect()
    }

    /// The switcher's rows: each name, padded to a column, then the mode the
    /// modeline would name for that buffer.
    ///
    /// Separate from [`Editor::buffer_names`] rather than folded into it,
    /// because that list is what Lisp's `buffer-list` answers with — a name
    /// there has to still be a name you can hand back to `switch-to-buffer`.
    /// Here nothing reads the string back: accepting goes by
    /// [`Prompt::ids`](crate::Prompt::ids), so the row is free to be legible.
    pub fn buffer_candidates(&self) -> Vec<String> {
        let names = self.buffer_names();
        let width = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
        std::iter::once(&self.buffer)
            .chain(self.others.iter())
            .zip(&names)
            .map(|(b, name)| {
                let pad = width - name.chars().count();
                format!("{name}{:pad$}  {}", "", modeline::major_mode_label(b))
            })
            .collect()
    }

    /// Switch to the buffer at `index` in [`Editor::buffer_names`]. Index 0 is
    /// the current buffer, so switching to it is a no-op.
    pub fn switch_buffer(&mut self, index: usize) {
        if index == 0 || index > self.others.len() {
            return;
        }
        self.sync_window();
        self.buffer.saved_scroll = self.scroll;
        let mut incoming = self.others.remove(index - 1);
        std::mem::swap(&mut self.buffer, &mut incoming);
        self.others.insert(0, incoming);
        self.scroll = self.buffer.saved_scroll;
        // Deliberately *not* clearing highlights: this is a buffer swap, not
        // an edit, and the incoming buffer's spans still describe its own
        // unchanged text. Clearing here is what made a split lose its colours
        // the moment focus moved between panes.  The revision bump below still
        // invalidates any parse that was in flight for the outgoing buffer.
        self.revision += 1;
        self.status = format!("switched to {}", self.buffer.name());
        self.mode = self.buffer.kind.mode();
        // The window now shows this buffer — otherwise the next focus change
        // would swap the old one straight back in.
        let (id, cursor, scroll) = (self.buffer.id, self.buffer.cursor, self.scroll);
        let w = self.frame_mut().current_window_mut();
        w.buffer = id;
        w.cursor = cursor;
        w.scroll = scroll;
    }

    // --- lisp-api: buffers with no file behind them --------------------------
    //
    // `show_special` above makes a *generated* buffer — read-only, one per kind,
    // re-rendered from state. These two make and unmake ordinary ones, which is
    // what a Lisp author needs and what `*scratch*` already is.

    /// Show a buffer called `name`, making it if it is not open yet.
    ///
    /// Idempotent by *name*, which is the contract every caller wants: a
    /// `messages-buffer` command run twice shows one buffer and rewrites it,
    /// rather than stacking a second one nobody can tell from the first. It is
    /// also the contract `switch-to-buffer` and `with-current-buffer` already
    /// match on, so a created buffer is reachable by every route the others are.
    pub fn create_buffer(&mut self, name: String) {
        if self.buffer.name() == name {
            return;
        }
        if let Some(i) = self.others.iter().position(|b| b.name() == name) {
            self.switch_buffer(i + 1);
            return;
        }
        // Stack the outgoing buffer unless it is a throwaway — the same rule
        // `load` follows, so opening a scratchpad from an empty editor does not
        // leave an untitled husk in the switcher.
        self.sync_window();
        if !self.buffer.is_pristine() {
            self.buffer.saved_scroll = self.scroll;
            let mut fresh = Buffer::from_str("");
            fresh.id = self.next_buffer_id;
            self.next_buffer_id += 1;
            let previous = std::mem::replace(&mut self.buffer, fresh);
            self.others.insert(0, previous);
        }
        self.buffer.given_name = Some(name);
        self.buffer.kind = BufferKind::Text;
        self.buffer.path = None;
        self.buffer.text = Rope::from_str("");
        self.buffer.cursor = 0;
        self.buffer.modified = false;
        self.buffer.undo.clear();
        self.buffer.redo.clear();
        self.buffer.markers.clear();
        self.buffer.overlays.clear();
        // A scene is about a document too, and this path *reuses* the live
        // buffer when it was pristine — so a page left over from the buffer
        // that was here would be drawn over a scratchpad that has nothing to do
        // with it, and would keep the read-only claim that came with it.
        self.buffer.set_scene(None);
        self.buffer.highlights.clear();
        // No hook: `fundamental-mode-hook` firing on every scratchpad would be a
        // surprise, and Lisp that wants one calls `set-major-mode` itself — it
        // is one line, and it is the line that says which mode it meant.
        self.buffer.major_mode = FUNDAMENTAL.into();
        self.buffer.minor_modes.clear();
        self.scroll = 0;
        self.revision += 1;
        self.mode = Mode::Normal;
        let id = self.buffer.id;
        let w = self.frame_mut().current_window_mut();
        w.buffer = id;
        w.cursor = 0;
        w.scroll = 0;
    }

    /// Kill the buffer at `index` in [`Editor::buffer_names`]; 0 is the live one.
    ///
    /// The last buffer is refused, as in Emacs: something has to be on screen.
    /// Every window still showing the dead buffer — in *any* frame, not only the
    /// focused one — is moved onto the live buffer, because a `Window.buffer`
    /// naming nothing is a pane the renderer cannot draw.
    pub fn kill_buffer(&mut self, index: usize) {
        if self.others.is_empty() {
            self.status = "cannot kill the last buffer".into();
            return;
        }
        if index > self.others.len() {
            self.status = "no such buffer".into();
            return;
        }
        let gone = if index == 0 {
            // Killing the live one: the most recently visited other takes its
            // place, which is where `switch_buffer` would have gone anyway.
            let incoming = self.others.remove(0);
            let outgoing = std::mem::replace(&mut self.buffer, incoming);
            self.scroll = self.buffer.saved_scroll;
            self.mode = self.buffer.kind.mode();
            outgoing
        } else {
            self.others.remove(index - 1)
        };
        let (live, cursor, scroll) = (self.buffer.id, self.buffer.cursor, self.scroll);
        for frame in &mut self.frames {
            for w in &mut frame.windows {
                if w.buffer == gone.id {
                    w.buffer = live;
                    w.cursor = cursor;
                    w.scroll = scroll;
                }
            }
        }
        self.revision += 1;
        self.status = format!("killed {}", gone.name());
    }

    // --- end of the lisp-api block -------------------------------------------

    /// The one and only document mutator.
    pub fn apply(&mut self, cmd: EditorCommand) {
        // Generated buffers are views, not documents: *dashboard* and *magit*
        // are re-rendered from state, so an edit would be silently discarded on
        // the next refresh and `:w` would write a screenshot of a status list.
        // Refuse the edit and say so, rather than letting it look like it took.
        //
        // ...and a buffer a *mode* has frozen is refused by the same line, which
        // is the whole of `SetReadOnly` reaching the keyboard: there is one
        // place the document is mutated, so there is one place to guard.
        if self.buffer.read_only() != ReadOnly::No && cmd.mutates_document() {
            self.status = format!("{} is read-only", self.buffer.name());
            return;
        }
        let before = self.revision;
        // Undo/redo move the revision too, but must not wipe the stack they feed.
        let history = matches!(cmd, EditorCommand::Undo | EditorCommand::Redo);
        match cmd {
            EditorCommand::Checkpoint => self.checkpoint(),
            EditorCommand::InsertChar(c) => {
                self.buffer.insert_char(c);
                self.revision += 1;
            }
            EditorCommand::InsertText(s) => {
                self.buffer.insert_text(&s);
                self.revision += 1;
            }
            EditorCommand::InsertNewline => {
                self.buffer.insert_char('\n');
                self.revision += 1;
            }
            EditorCommand::DeleteBackward => {
                self.buffer.delete_backward();
                self.revision += 1;
            }
            EditorCommand::DeleteForward => {
                self.buffer.delete_forward();
                self.revision += 1;
            }
            EditorCommand::DeleteRange(a, b) => {
                self.buffer.delete_range(a, b);
                self.revision += 1;
            }
            EditorCommand::Yank {
                start,
                end,
                linewise,
            } => {
                self.register = self.buffer.slice_string(start, end);
                self.register_linewise = linewise;
                self.register_revision += 1; // clipboard
                let n = self.register.chars().count();
                self.status = format!("yanked {n} chars");
            }
            EditorCommand::SetRegister { text, linewise } => {
                let n = text.chars().count();
                self.register = text;
                self.register_linewise = linewise;
                self.register_revision += 1; // clipboard
                self.status = format!("yanked {n} chars");
            }
            EditorCommand::Paste { after } => self.paste(after),
            // Arrows are the same motion `j`/`k` are, so they follow the same
            // rows: which key you reached for must not change what a wrapped
            // paragraph does.
            EditorCommand::MoveCursor(d @ (Direction::Up | Direction::Down))
                if self.visual_lines() =>
            {
                self.buffer.cursor = self.visual_target(d == Direction::Down, 1, self.cursor_vcol());
            }
            EditorCommand::MoveCursor(d) => self.buffer.move_cursor(d),
            EditorCommand::MoveTo(i) => self.buffer.cursor = i.min(self.buffer.len_chars()),
            EditorCommand::ScrollLines(d) => self.scroll_lines(d),
            EditorCommand::Undo => self.undo(),
            EditorCommand::Redo => self.redo(),

            EditorCommand::SetMode(m) => self.set_mode(m),
            EditorCommand::ShowDashboard => {
                self.mode = Mode::Dashboard;
                self.dashboard.selected = 0;
            }
            EditorCommand::Message(m) => {
                // The status line shows one message; the log keeps the rest.
                // Without it a burst — a config load, a command that reports
                // twice — is indistinguishable from its last line, and there is
                // nothing to look at afterwards. This is what `*Messages*` is.
                if self.messages.len() == MESSAGE_LIMIT {
                    self.messages.remove(0);
                }
                self.messages.push(m.clone());
                self.status = m;
            }
            EditorCommand::Quit => self.should_quit = true,

            EditorCommand::SetFontSize(s) => self.settings.font_size = s.clamp(4.0, 400.0),
            EditorCommand::SetBackground(c) => self.settings.background = clamp3(c),
            EditorCommand::SetForeground(c) => {
                self.settings.foreground = clamp3(c);
                self.theme.set(HlKind::Default, c);
            }
            EditorCommand::SetSyntaxColor(name, rgb) => match HlKind::from_name(&name) {
                Some(k) => self.theme.set(k, rgb),
                None => self.status = format!("unknown syntax face: {name}"),
            },
            EditorCommand::SetLineNumbers(on) => self.settings.line_numbers = on,
            EditorCommand::SetTabWidth(n) => self.settings.tab_width = n.clamp(1, 16),
            // Not clamped at the low end past zero, which is the "off" value:
            // a measure of one or two columns is silly rather than dangerous,
            // and the renderer never lets the inset exceed the pane anyway.
            EditorCommand::SetTextWidth(n) => self.settings.text_width = n.min(1000),
            EditorCommand::SetCompletionStyle(name) => match CompletionStyle::from_name(&name) {
                Some(s) => self.settings.completion_style = s,
                None => self.status = format!("unknown completion style: {name}"),
            },
            // Clamped only for sanity, and symmetrically: the sign carries
            // meaning, so squashing negatives would silently drop "sunken".
            EditorCommand::SetModelineRelief(n) => {
                self.settings.modeline_relief = n.clamp(-16, 16)
            }
            EditorCommand::SetModelinePad(n) => self.settings.modeline_pad = n.clamp(0, 64),
            EditorCommand::SetRelativeLineNumbers(on) => self.settings.relative_line_numbers = on,
            EditorCommand::SetLineOverflow(name) => match LineOverflow::from_name(&name) {
                Some(o) => self.settings.line_overflow = o,
                None => self.status = format!("unknown line overflow: {name}"),
            },
            EditorCommand::ClearCommands => self.commands.clear(),
            EditorCommand::RegisterCommand(name) => {
                if !self.commands.contains(&name) {
                    self.commands.push(name);
                }
            }
            EditorCommand::SwitchBuffer(i) => self.switch_buffer(i),
            EditorCommand::SwitchBufferId(id) => self.switch_buffer_id(id),
            EditorCommand::SetMajorMode(name) => {
                self.buffer.major_mode = name.clone();
                self.buffer.line_numbers = Some(self.gutter_for_mode(&name));
                self.pending_hooks.push(format!("{name}-hook"));
                self.status = format!("major mode: {name}");
            }
            // Set the policy, then re-decide for every buffer that already
            // exists — a config reload must not leave the buffers you opened
            // before it obeying the old list.
            EditorCommand::SetNoGutterModes(modes) => {
                self.no_gutter_modes = modes
                    .split_whitespace()
                    .map(|s| s.to_ascii_lowercase())
                    .collect();
                let decide = |m: &str| !self.no_gutter_modes.iter().any(|x| x == m);
                self.buffer.line_numbers = Some(decide(&self.buffer.major_mode));
                for b in &mut self.others {
                    b.line_numbers = Some(!self.no_gutter_modes.iter().any(|x| *x == b.major_mode));
                }
            }
            EditorCommand::SetMinorMode(name, on) => {
                self.buffer.minor_modes.retain(|m| *m != name);
                if on {
                    self.buffer.minor_modes.push(name.clone());
                }
                self.pending_hooks
                    .push(format!("{name}-{}-hook", if on { "on" } else { "off" }));
            }
            EditorCommand::SplitWindow(dir) => {
                self.sync_window();
                let id = self.frames[self.focus_frame].split(dir);
                self.status = format!("split window {id}");
            }
            EditorCommand::CloseWindow => {
                self.sync_window();
                if self.frames[self.focus_frame].close_current() {
                    self.adopt_window();
                } else {
                    self.status = "cannot close the last window".into();
                }
            }
            EditorCommand::FocusNextWindow => {
                self.sync_window();
                self.frames[self.focus_frame].focus_next();
                self.adopt_window();
            }
            EditorCommand::FocusWindow(id) => {
                if self.frames[self.focus_frame].window(id).is_some()
                    && self.frames[self.focus_frame].current != id
                {
                    self.sync_window();
                    self.frames[self.focus_frame].focus(id);
                    self.adopt_window();
                }
            }
            EditorCommand::NewFrame => {
                self.sync_window();
                let dashboard = self.dashboard_buffer_id();
                self.frames.push(frame::Frame::new(dashboard));
                self.focus_frame = self.frames.len() - 1;
                self.adopt_window();
            }
            EditorCommand::FocusFrame(i) => {
                if i < self.frames.len() && i != self.focus_frame {
                    self.sync_window();
                    self.focus_frame = i;
                    self.adopt_window();
                }
            }
            EditorCommand::CloseFrame => {
                if self.frames.len() > 1 {
                    self.frames.remove(self.focus_frame);
                    self.focus_frame = self.focus_frame.min(self.frames.len() - 1);
                    self.adopt_window();
                } else {
                    self.should_quit = true;
                }
            }

            EditorCommand::SetDashboardBanner(b) => self.dashboard.banner = b,
            EditorCommand::SetDashboardLogo(id) => self.dashboard.logo = id,
            // Both of these can strand `selected` past the end of the list —
            // init.lisp rewrites the items several hundred ms after startup,
            // by which time you may already have moved the selection down.
            // A stranded index highlights nothing and activates nothing.
            EditorCommand::ClearDashboardItems => {
                self.dashboard.items.clear();
                self.dashboard.clamp_selection();
            }
            EditorCommand::AddDashboardItem { key, label, action } => {
                self.dashboard
                    .items
                    .push(dashboard::Item { key, label, action });
                self.dashboard.clamp_selection();
            }

            EditorCommand::BindKey {
                mode,
                keys,
                command,
            } => match Mode::from_name(&mode) {
                Some(m) => {
                    self.keymap.insert((m, normalize_keys(&keys)), command);
                }
                // Not an editing mode, so it names a major or minor mode.
                // Unknown names are *not* an error: a binding may be made
                // before the mode it belongs to is ever entered.
                None => {
                    self.mode_keymap
                        .insert((mode, normalize_keys(&keys)), command);
                }
            },
            EditorCommand::Overlay(edit) => {
                let drops = matches!(
                    edit,
                    OverlayEdit::Delete(_) | OverlayEdit::RemoveIn(..) | OverlayEdit::Image(_, None)
                );
                self.buffer.overlays.edit(edit);
                // Only when an overlay could have let go of one: an image nobody
                // points at is a few hundred KB and a texture the renderer will
                // never draw again.
                if drops {
                    self.prune_images();
                }
            }

            // --- lisp-api ----------------------------------------------------
            EditorCommand::CreateBuffer(name) => self.create_buffer(name),
            EditorCommand::KillBuffer(index) => self.kill_buffer(index),
            EditorCommand::SetLanguage(lang) => {
                self.buffer.language = lang;
                // The app re-highlights off the revision counter, so a language
                // that changed with the text untouched would not be seen until
                // the next keystroke.
                self.buffer.highlights.clear();
                self.revision += 1;
            }
            // The revision is deliberately *not* bumped: nothing about the text
            // changed, and moving it would throw away the highlight spans the
            // syntax thread has already computed for a buffer that is about to
            // be typeset rather than edited.
            EditorCommand::SetReadOnly(on) => self.buffer.read_only = on,
            // The revision is left alone for the same reason `SetReadOnly`
            // leaves it alone, and it matters more here: a buffer showing a
            // scene is being typeset rather than edited, and bumping the
            // counter would throw away the highlight spans the syntax thread
            // computed for text that has not changed — the very spans a mode
            // built the scene's runs out of. What has to notice a new scene is
            // the *renderer*, and it notices through its frame digest.
            EditorCommand::SetScene(scene) => {
                // The sizes are filled in *before* the scene is installed, so
                // nothing downstream — layout, hit testing, paint — ever sees a
                // figure of no size and has to decide what to do about it.
                let scene = scene.map(|s| self.with_image_sizes(s));
                self.buffer.set_scene(scene);
            }
            EditorCommand::ReadFromMinibuffer {
                id,
                label,
                completing,
            } => {
                let mut prompt = Prompt::new(PromptKind::Lisp { id, completing }, &label, Vec::new());
                // Nothing previews, so nothing has to be restored on Escape.
                prompt.origin = None;
                self.prompt = Some(prompt);
            }
            EditorCommand::PromptItem(text) => {
                if let Some(p) = self.prompt.as_mut() {
                    if matches!(p.kind, PromptKind::Lisp { .. }) {
                        p.push_item(text);
                    }
                }
            }
            EditorCommand::WhichKey(row) => match row {
                Some(r) => self.which_key.push(r),
                None => self.which_key.clear(),
            },
            // --- end of the lisp-api block ------------------------------------

            // The app intercepts these; reaching `apply` means nothing is listening.
            EditorCommand::CallLisp(name) => {
                self.status = format!("no Lisp runtime to call {name}")
            }
            EditorCommand::Git(verb) => self.status = format!("no git backend for {verb}"),
            EditorCommand::Dired(verb) => {
                self.status = format!("no filesystem backend for {verb}")
            }
            EditorCommand::Term(verb) => self.status = format!("no terminal backend for {verb}"),
            EditorCommand::Project(verb) => {
                self.status = format!("no project backend for {verb}")
            }
            // Only reachable with no app under core — a keystroke aimed at a
            // shell that is not there is nothing, not an error worth reporting
            // on every key.
            EditorCommand::TermKey(_) => {}
            EditorCommand::OpenFile(p) => self.status = format!("cannot open {}", p.display()),
            EditorCommand::OpenAt(hit) => self.status = format!("cannot open {hit}"),
            EditorCommand::SaveFile(_) => self.status = "cannot save: no file backend".into(),
            EditorCommand::Confirmed(_) => {
                self.status = "cannot confirm: no file backend".into()
            }
        }
        if self.revision != before && !history {
            self.buffer.redo.clear();
        }
        self.clamp_cursor();
        self.ensure_cursor_visible();
    }

    /// Replace the document wholesale — used by file loading in the app layer.
    pub fn load(&mut self, text: &str, path: Option<PathBuf>, language: Option<String>) {
        // Already open? Switch rather than open a second copy.
        if path.is_some() {
            if self.buffer.path == path {
                self.mode = Mode::Normal;
                return;
            }
            if let Some(i) = self.others.iter().position(|b| b.path == path) {
                self.switch_buffer(i + 1);
                return;
            }
        }
        // Stack the outgoing buffer, unless it is a throwaway.
        if !self.buffer.is_pristine() {
            self.buffer.saved_scroll = self.scroll;
            let mut fresh = Buffer::from_str("");
            fresh.id = self.next_buffer_id;
            self.next_buffer_id += 1;
            let previous = std::mem::replace(&mut self.buffer, fresh);
            self.others.insert(0, previous);
        }
        // A new document gets a new history — and note the outgoing buffer took
        // its own undo stack with it, so one `u` here can never restore the
        // file you were looking at a moment ago.
        self.buffer.text = Rope::from_str(text);
        self.buffer.cursor = 0;
        self.buffer.path = path;
        self.buffer.language = language;
        self.buffer.modified = false;
        self.buffer.undo.clear();
        self.buffer.redo.clear();
        // A different document: the markers of the buffer that was here went
        // with it onto the stack, and any left are stale by the same argument
        // that clears the undo history.
        self.buffer.markers.clear();
        self.buffer.overlays.clear();
        // ...and the scene, for the reason `create_buffer` clears it: a
        // pristine buffer is reused rather than stacked, and the page that was
        // on it is about the document that just left.
        self.buffer.set_scene(None);
        self.buffer.kind = BufferKind::Text;
        // The major mode follows the file, and its hook fires so `init.lisp`
        // can react — `(defun org-mode-hook () ...)` is the whole extension
        // point, exactly as in Emacs.
        let major = major_mode_for(self.buffer.language.as_deref());
        self.buffer.major_mode = major.clone();
        self.buffer.minor_modes.clear();
        self.pending_hooks.push(format!("{major}-hook"));
        self.buffer.highlights.clear();
        self.revision += 1;
        self.scroll = 0;
        self.mode = Mode::Normal;
        let id = self.buffer.id;
        let w = self.frame_mut().current_window_mut();
        w.buffer = id;
        w.cursor = 0;
        w.scroll = 0;
    }

    fn set_mode(&mut self, m: Mode) {
        match m {
            // Switching *between* visual modes keeps the anchor, so `v` then
            // `C-v` reshapes the same selection rather than restarting it.
            m if m.is_visual() && !self.mode.is_visual() => {
                self.visual_anchor = Some(self.buffer.cursor);
            }
            Mode::Normal | Mode::Insert | Mode::Dashboard => self.visual_anchor = None,
            _ => {}
        }
        if m == Mode::Normal && self.mode == Mode::Insert {
            // vim pulls the cursor back one on leaving insert
            let (_, col) = self.buffer.cursor_line_col();
            if col > 0 {
                self.buffer.cursor -= 1;
            }
        }
        self.mode = m;
        self.pending.clear();
        self.desired_col = None;
    }

    /// Every char range the selection covers — one per line in block mode, and
    /// a single range otherwise.
    ///
    /// This, not [`Editor::selection`], is what the renderer draws and what a
    /// block operator works on: a rectangle is genuinely several disjoint runs
    /// of text, and flattening it to its bounding span would select the middle
    /// of every line it spans.
    pub fn selection_ranges(&self) -> Vec<(usize, usize)> {
        let Some((start, end)) = self.selection() else {
            return Vec::new();
        };
        if self.mode != Mode::VisualBlock {
            return vec![(start, end)];
        }
        let anchor = match self.visual_anchor {
            Some(a) => a,
            None => return Vec::new(),
        };
        let buf = &self.buffer;
        let n = buf.len_chars();
        let (al, ac) = line_col_of(buf, anchor.min(n));
        let (cl, cc) = line_col_of(buf, buf.cursor.min(n));
        let (first, last) = (al.min(cl), al.max(cl));
        let (left, right) = (ac.min(cc), ac.max(cc));

        (first..=last)
            .filter_map(|line| {
                let len = buf.line_len(line);
                // A line shorter than the block's left edge contributes
                // nothing — vim skips it rather than selecting its newline.
                if left >= len {
                    return None;
                }
                let s = buf.line_start(line) + left;
                let e = buf.line_start(line) + (right + 1).min(len);
                (s < e).then_some((s, e))
            })
            .collect()
    }

    /// The inclusive char range covered by the visual selection, if any.
    /// In block mode this is the *bounding* span; see [`Editor::selection_ranges`].
    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.visual_anchor?;
        if !self.mode.is_visual() {
            return None;
        }
        let (a, b) = if anchor <= self.buffer.cursor {
            (anchor, self.buffer.cursor)
        } else {
            (self.buffer.cursor, anchor)
        };
        if self.mode == Mode::VisualLine {
            let first = self.buffer.text.char_to_line(a.min(self.buffer.len_chars()));
            let last = self.buffer.text.char_to_line(b.min(self.buffer.len_chars()));
            let end = (self.buffer.line_end(last) + 1).min(self.buffer.len_chars());
            Some((self.buffer.line_start(first), end))
        } else {
            Some((a, (b + 1).min(self.buffer.len_chars())))
        }
    }

    /// A marker at `pos` in the live buffer, as a handle Lisp can hold.
    ///
    /// Ids are handed out per *editor* rather than per buffer, so a handle can
    /// never name a marker in the buffer you have since switched to: the answer
    /// for a marker belonging elsewhere is "gone", never someone else's offset.
    pub fn make_marker(&mut self, pos: usize, insertion: Insertion) -> MarkerId {
        let id = self.next_marker_id;
        self.next_marker_id += 1;
        let pos = pos.min(self.buffer.len_chars());
        self.buffer.markers.add(id, pos, insertion);
        id
    }

    /// Where a marker is now, or `None` if it was deleted, never existed, or
    /// belongs to a buffer that is not the live one.
    ///
    /// Clamped on the way out: the rope is replaced whole by undo and by a
    /// reload, so the buffer's current length has the last word on any position.
    pub fn marker_position(&self, id: MarkerId) -> Option<usize> {
        let n = self.buffer.len_chars();
        self.buffer.markers.position(id).map(|p| p.min(n))
    }

    pub fn set_marker(&mut self, id: MarkerId, pos: usize) {
        let pos = pos.min(self.buffer.len_chars());
        self.buffer.markers.set(id, pos);
    }

    pub fn delete_marker(&mut self, id: MarkerId) {
        self.buffer.markers.remove(id);
    }

    // --- overlays ---------------------------------------------------------
    //
    // Everything that *changes* one is an `EditorCommand::Overlay`, since it
    // needs no answer. These three are the exceptions: one hands a handle back,
    // and two are questions.

    /// An overlay over `[start, end)` in the live buffer, as a handle Lisp can
    /// hold. `0` — never a live overlay — for an empty or inverted range, which
    /// is what arithmetic in Lisp produces and not something to panic on.
    ///
    /// Ids come from the *editor*, like marker ids, so a handle can never name
    /// an overlay in the buffer you have since switched to.
    pub fn make_overlay(&mut self, start: usize, end: usize) -> OverlayId {
        let n = self.buffer.len_chars();
        let (start, end) = (start.min(n), end.min(n));
        if start >= end {
            return 0;
        }
        let id = self.next_overlay_id;
        self.next_overlay_id += 1;
        self.buffer.overlays.add(id, start, end);
        id
    }

    /// `(start, end)` now, or `None` if it was deleted, never existed, or
    /// belongs to a buffer that is not the live one — the same contract
    /// [`Editor::marker_position`] has.
    pub fn overlay_span(&self, id: OverlayId) -> Option<(usize, usize)> {
        self.buffer.overlays.span(id)
    }

    /// Every overlay overlapping `[start, end)` of the live buffer, oldest
    /// first, as `(id, start, end)`.
    pub fn overlays_in(&self, start: usize, end: usize) -> Vec<(OverlayId, usize, usize)> {
        self.buffer
            .overlays
            .in_range(start, end)
            .map(|o| (o.id, o.start, o.end))
            .collect()
    }

    /// Remember a rendered bitmap under `id`, which the caller derived from
    /// whatever produced it. Already present is a no-op, which is the point:
    /// previewing the same fragment twice reuses the renderer's texture.
    pub fn add_image(&mut self, id: ImageId, image: Image) {
        self.images.entry(id).or_insert(image);
    }

    pub fn image(&self, id: ImageId) -> Option<&Image> {
        self.images.get(&id)
    }

    pub fn has_image(&self, id: ImageId) -> bool {
        self.images.contains_key(&id)
    }

    /// Whether a buffer in major mode `name` should show a gutter.
    ///
    /// The whole of the policy, and it is a *list of modes* rather than a flag
    /// per buffer because that is the shape the question actually has: nobody
    /// decides gutters buffer by buffer, they decide "not in prose". Lisp owns
    /// the list; this only applies it.
    pub fn gutter_for_mode(&self, name: &str) -> bool {
        !self.no_gutter_modes.iter().any(|m| m == name)
    }

    /// Drop bitmaps nothing in the editor still names.
    ///
    /// Three roots, and every one of them is a way a bitmap gets pointed at
    /// rather than a place bitmaps are kept:
    ///
    /// - the overlays of every buffer, which is where a LaTeX preview over text
    ///   lives;
    /// - the **scene** of every buffer, because a figure or an inline fragment
    ///   on a page is named by no overlay at all. Without this root a preview
    ///   that only a scene points at is dropped by the next prune — which is
    ///   triggered by deleting some *unrelated* overlay, in some other buffer —
    ///   and the page then paints an id that resolves to nothing;
    /// - the dashboard's logo, which belongs to the editor rather than to any
    ///   document, and would otherwise survive until the first prune and then
    ///   vanish.
    ///
    /// A miss in any of the three is the same shape of bug and it is not a
    /// crash: the image simply stops being drawn, some time later, for a reason
    /// nowhere near where it went.
    fn prune_images(&mut self) {
        if self.images.is_empty() {
            return;
        }
        let live: std::collections::HashSet<ImageId> = std::iter::once(&self.buffer)
            .chain(self.others.iter())
            .flat_map(|b| {
                let scene = b.scene.iter().flat_map(|s| scene::images(s).map(|(id, ..)| id));
                b.overlays.images().chain(scene)
            })
            .chain(self.dashboard.logo)
            .collect();
        self.images.retain(|id, _| live.contains(id));
    }

    fn checkpoint(&mut self) {
        let buf = &mut self.buffer;
        if let Some(last) = buf.undo.last() {
            if last.text.len_chars() == buf.text.len_chars() && last.text == buf.text {
                return;
            }
        }
        buf.undo.push(Snapshot {
            text: buf.text.clone(),
            cursor: buf.cursor,
        });
        if buf.undo.len() > UNDO_LIMIT {
            buf.undo.remove(0);
        }
    }

    fn undo(&mut self) {
        self.status = match self.buffer.step_history(true) {
            true => {
                self.revision += 1;
                "undo".into()
            }
            false => "already at oldest change".into(),
        };
    }

    fn redo(&mut self) {
        self.status = match self.buffer.step_history(false) {
            true => {
                self.revision += 1;
                "redo".into()
            }
            false => "already at newest change".into(),
        };
    }

    // --- the unnamed register --------------------------------------------
    //
    // clipboard: `select-enable-clipboard t` is set in the config, which in
    // Emacs means the kill ring *is* the system clipboard. Core cannot talk to
    // the window system, so it exposes the register and the app layer does the
    // mirroring — the same division as reading a file.

    /// vim's `""` — what `p` pastes, and what every yank and delete fills.
    pub fn register(&self) -> (&str, bool) {
        (&self.register, self.register_linewise)
    }

    /// Bumped on every write, so a mirror can poll with an integer compare
    /// rather than by diffing a possibly large string once a frame.
    pub fn register_revision(&self) -> u64 {
        self.register_revision
    }

    /// Fill the register *without* the "yanked N chars" status `SetRegister`
    /// sets: text arriving from the system clipboard was not yanked here, and
    /// claiming it was would wipe whatever the last real command reported.
    pub fn adopt_register(&mut self, text: String, linewise: bool) {
        self.register = text;
        self.register_linewise = linewise;
        self.register_revision += 1;
    }

    fn paste(&mut self, after: bool) {
        if self.register.is_empty() {
            return;
        }
        let text = self.register.clone();
        if self.register_linewise {
            let (line, _) = self.buffer.cursor_line_col();
            let body = text.strip_suffix('\n').unwrap_or(&text).to_string();
            let line_end = self.buffer.line_end(line);
            let at = match (after, line_end < self.buffer.len_chars()) {
                // Pasting after the last line of a file with no trailing
                // newline: there is no newline to paste past, so open one.
                (true, false) => {
                    self.buffer.cursor = line_end;
                    self.buffer.insert_text(&format!("\n{body}"));
                    line_end + 1
                }
                (true, true) => {
                    self.buffer.cursor = line_end + 1;
                    self.buffer.insert_text(&format!("{body}\n"));
                    line_end + 1
                }
                (false, _) => {
                    let start = self.buffer.line_start(line);
                    self.buffer.cursor = start;
                    self.buffer.insert_text(&format!("{body}\n"));
                    start
                }
            };
            self.buffer.cursor = at;
        } else {
            if after && self.buffer.cursor < self.buffer.len_chars() {
                self.buffer.cursor += 1;
            }
            self.buffer.insert_text(&text);
            if self.buffer.cursor > 0 {
                self.buffer.cursor -= 1;
            }
        }
        self.revision += 1;
    }

    // --- visual lines -----------------------------------------------------
    //
    // A *visual* line is one row of the grid: a buffer line that does not fit
    // across the window occupies several. The config's `j` is
    // `evil-next-visual-line`, so a wrapped paragraph moves the way it reads
    // rather than the way it is stored — press `j` in the middle of a long line
    // and you land on the row below, not four screenfuls of prose later.
    //
    // Everything here is arithmetic over `display::expand_line`, the *same*
    // function the renderer lays a line out with, because a `j` that computed
    // cells differently would put the cursor where the block is not drawn. The
    // only thing core cannot work out is how many cells fit — `wrap_cols`, which
    // the renderer parks alongside `viewport_lines`.

    /// Whether motion counts screen rows. Wrapping off means one row per line
    /// and this whole path is the identity; `wrap_cols == 0` means nothing has
    /// drawn yet, which is every headless test.
    pub(crate) fn visual_lines(&self) -> bool {
        self.settings.line_overflow == LineOverflow::Wrap && self.wrap_cols > 0
    }

    /// The cells of buffer line `line`, exactly as the renderer expands them.
    ///
    /// ponytail: overlays are not applied, so a `display` substitution — an
    /// org-modern bullet, ghost text — shifts the renderer's columns and not
    /// these, and `j` down a line carrying one can land a character off. Ceiling:
    /// only lines with a substitution, and only by the length difference.
    /// Upgrade path: `substitute` moves into `display` too and this takes the
    /// buffer's overlays, which is the same list the renderer already walks.
    fn line_cells(&self, line: usize) -> Vec<display::Cell> {
        let start = self.buffer.line_start(line);
        let text = self
            .buffer
            .slice_string(start, start + self.buffer.line_len(line));
        display::expand_line(&text, self.settings.tab_width)
    }

    /// Cell column the cursor is drawn in, *within its display row* — the thing
    /// a vertical run holds on to. Not a character column: on a line of CJK the
    /// two differ by a factor of two, and holding the wrong one walks the cursor
    /// sideways down the screen.
    pub(crate) fn cursor_vcol(&self) -> usize {
        let (line, col) = self.buffer.cursor_line_col();
        display::visual_col(&self.line_cells(line), col) % self.wrap_cols.max(1)
    }

    /// Char offset `n` display rows below (`down`) or above the cursor, landing
    /// in cell column `vcol`.
    ///
    /// Walks row by row rather than computing an index, because a buffer line's
    /// height depends on its own contents and there is no closed form. `n` is a
    /// count and counts are small; the walk touches one line per row crossed.
    pub(crate) fn visual_target(&self, down: bool, n: usize, vcol: usize) -> usize {
        let cols = self.wrap_cols.max(1);
        let buf = &self.buffer;
        let (mut line, col) = buf.cursor_line_col();
        let mut cells = self.line_cells(line);
        let mut row = display::visual_col(&cells, col) / cols;
        for _ in 0..n {
            // Crossing into another buffer line goes through `step_line`, which
            // steps over anything a fold is hiding — the rows of a folded line
            // are not drawn, so they are not rows to move through either.
            match (down, row) {
                (true, r) if r + 1 < display::wrap_row_count(cells.len(), cols) => row += 1,
                (false, r) if r > 0 => row -= 1,
                _ => match self.step_line(line, down) {
                    // The first or last visible row: stop, exactly as `j` on the
                    // last line already does.
                    l if l == line => break,
                    l => {
                        line = l;
                        cells = self.line_cells(line);
                        row = match down {
                            true => 0,
                            false => display::wrap_row_count(cells.len(), cols) - 1,
                        };
                    }
                },
            }
        }
        // Past the end of a short row is the end of it, which on the last row of
        // a line is the end of the line — the same clamp `move_to_line_col` does
        // in characters, done in cells because that is the unit `vcol` is in.
        let want = (row * cols + vcol).min(cells.len());
        buf.line_start(line) + display::char_at_cell(&cells, want, buf.line_len(line))
    }

    /// In Normal mode the cursor sits *on* a character, never past the last one.
    fn clamp_cursor(&mut self) {
        // ...and never on a line a fold is hiding, whatever put it there — a
        // fold made around point, an undo, a Lisp `goto-char`. It comes back to
        // the fold's own first line, which is the one still drawn. This runs at
        // the end of every `apply`, so it is the backstop behind `step_line`
        // rather than a second copy of it.
        let start = self.buffer.line_start(self.buffer.cursor_line_col().0);
        if let Some(head) = fold_hiding(self.buffer.overlays(), start) {
            self.buffer.cursor = head;
        }
        if self.mode == Mode::Insert {
            self.buffer.cursor = self.buffer.cursor.min(self.buffer.len_chars());
            return;
        }
        let (line, col) = self.buffer.cursor_line_col();
        let max = self.buffer.line_len(line).saturating_sub(1);
        if col > max && self.buffer.line_len(line) > 0 {
            self.buffer.cursor = self.buffer.line_start(line) + max;
        }
    }

    /// Move the *view*, dragging the cursor only as far as it takes to keep it
    /// on screen — the vim mouse-wheel feel, where scrolling is not a motion.
    ///
    /// The cursor has to end up inside the viewport, because
    /// `ensure_cursor_visible` runs right after this and would otherwise pull
    /// the view straight back to wherever the cursor was.
    fn scroll_lines(&mut self, delta: i32) {
        let h = self.viewport_lines.max(1);
        let max = self.buffer.len_lines().saturating_sub(h) as i64;
        self.scroll = (self.scroll as i64 + delta as i64).clamp(0, max.max(0)) as usize;

        let (line, col) = self.buffer.cursor_line_col();
        let bottom = (self.scroll + h).saturating_sub(1);
        let target = line.clamp(self.scroll, bottom);
        if target != line {
            self.buffer.move_to_line_col(target, col);
        }
    }

    fn ensure_cursor_visible(&mut self) {
        let (line, _) = self.buffer.cursor_line_col();
        let h = self.viewport_lines.max(1);
        if line < self.scroll {
            self.scroll = line;
        } else if line >= self.scroll + h {
            self.scroll = line + 1 - h;
        }
        // Never scroll past the last line sitting at the bottom of the window.
        // `C-d` adds to `scroll` unconditionally, so without this a file
        // shorter than the viewport walks straight off the top of the screen.
        self.scroll = self.scroll.min(self.buffer.len_lines().saturating_sub(h));
    }

    /// The text the status / prompt line should display.
    /// The half-typed key sequence, for the modeline. `pending` is private, so
    /// this is how anything outside core sees a which-key trail.
    pub fn pending_hint(&self) -> String {
        self.pending.hint()
    }

    pub fn status_line(&self) -> String {
        if let Some(p) = &self.prompt {
            return p.line();
        }
        let name = self.buffer.name();
        let dirty = if self.buffer.modified { " [+]" } else { "" };
        let (line, col) = self.buffer.cursor_line_col();
        let pending = self.pending.hint();
        format!(
            "-- {} --  {name}{dirty}  {}:{}  {}{pending}",
            self.mode.label(),
            line + 1,
            col + 1,
            self.status,
        )
    }
}

/// Canonical spacing for a key sequence: `"g  d"` and `"gd"` both become `"g d"`.
pub fn normalize_keys(s: &str) -> String {
    // Spaces already separate the tokens.
    if s.contains(' ') {
        return s.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    // A single token that merely *looks* like several characters: a bracketed
    // name (`<tab>`), a modifier chord (`C-x`, `C-M-j`, `M-+`), or the leader.
    // Splitting these per character turned `<tab>` into `< t a b >`, which no
    // keystroke could ever produce.
    if s.starts_with('<') || s.contains('-') || s == "SPC" {
        return s.to_string();
    }
    // ponytail: `gg` means the sequence `g g`, so an unseparated mix like
    // `g<tab>` is not supported — write `g <tab>`.
    s.chars().map(|c| c.to_string()).collect::<Vec<_>>().join(" ")
}

fn line_col_of(buf: &Buffer, at: usize) -> (usize, usize) {
    let line = buf.text.char_to_line(at);
    (line, at - buf.line_start(line))
}

fn clamp3(c: [f32; 3]) -> [f32; 3] {
    [
        c[0].clamp(0.0, 1.0),
        c[1].clamp(0.0, 1.0),
        c[2].clamp(0.0, 1.0),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed keys, applying every produced command — the real app loop.
    pub(crate) fn feed(ed: &mut Editor, keys: &[Key]) {
        for &k in keys {
            for cmd in ed.handle_key(k) {
                ed.apply(cmd);
            }
        }
    }

    pub(crate) fn fresh(text: &str) -> Editor {
        let mut ed = Editor::new();
        ed.mode = Mode::Normal;
        ed.buffer = Buffer::from_str(text);
        ed.status.clear();
        ed
    }

    #[test]
    fn insert_mode_types_text() {
        let mut ed = fresh("");
        feed(&mut ed, &[Key::Char('i'), Key::Char('h'), Key::Char('i')]);
        assert_eq!(ed.mode, Mode::Insert);
        assert_eq!(ed.buffer.text.to_string(), "hi");
    }

    #[test]
    fn undo_reverses_one_insert_group() {
        let mut ed = fresh("abc");
        feed(&mut ed, &[Key::Char('x')]);
        assert_eq!(ed.buffer.text.to_string(), "bc");
        feed(&mut ed, &[Key::Char('u')]);
        assert_eq!(ed.buffer.text.to_string(), "abc");
    }

    #[test]
    fn redo_after_undo() {
        let mut ed = fresh("abc");
        feed(&mut ed, &[Key::Char('x'), Key::Char('u'), Key::Ctrl('r')]);
        assert_eq!(ed.buffer.text.to_string(), "bc");
    }

    #[test]
    fn theme_faces_are_settable() {
        let mut ed = fresh("");
        ed.apply(EditorCommand::SetSyntaxColor("keyword".into(), [1.0, 0.0, 0.0]));
        assert_eq!(ed.theme.color(HlKind::Keyword, [0.0; 3]), [1.0, 0.0, 0.0]);
    }

    #[test]
    fn normalize_keys_handles_both_forms() {
        assert_eq!(normalize_keys("gd"), "g d");
        assert_eq!(normalize_keys("SPC f f"), "SPC f f");
        assert_eq!(normalize_keys("C-x C-f"), "C-x C-f");
        // Named keys are one token, not five characters.
        for named in ["<tab>", "<ret>", "<esc>", "<bs>", "<left>"] {
            assert_eq!(normalize_keys(named), named);
        }
        // ...and each still matches what the key actually produces.
        assert_eq!(normalize_keys(&Key::Tab.token()), Key::Tab.token());
        assert_eq!(normalize_keys("C-M-j"), "C-M-j");
        assert_eq!(normalize_keys("M-+"), "M-+");
        assert_eq!(normalize_keys("SPC"), "SPC");
        assert_eq!(normalize_keys("-"), "-");
    }

    /// An editor in Insert mode with one marker, so an edit can be aimed
    /// anywhere in the text without the Normal-mode cursor clamp getting in the
    /// way.
    fn marked(text: &str, at: usize) -> (Editor, MarkerId) {
        let mut ed = fresh(text);
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        let m = ed.make_marker(at, Insertion::Stay);
        (ed, m)
    }

    #[test]
    fn a_marker_still_names_its_character_after_typing_in_front_of_it() {
        let (mut ed, m) = marked("alpha beta", 6);
        assert_eq!(ed.buffer.slice_string(6, 10), "beta");
        ed.apply(EditorCommand::MoveTo(0));
        ed.apply(EditorCommand::InsertText("xy".into()));
        assert_eq!(ed.marker_position(m), Some(8));
        assert_eq!(ed.buffer.slice_string(8, 12), "beta");
    }

    /// The reason every edit is a splice: a path that forgets to adjust is the
    /// bug markers exist to prevent, so each of them is checked by name.
    #[test]
    fn every_editing_command_moves_a_marker_that_follows_it() {
        let cases = [
            (vec![EditorCommand::InsertChar('z')], 6),
            (vec![EditorCommand::InsertText("zz".into())], 7),
            (vec![EditorCommand::InsertNewline], 6),
            (vec![EditorCommand::DeleteForward], 4),
            (vec![EditorCommand::DeleteRange(0, 2)], 3),
            (
                vec![EditorCommand::MoveTo(3), EditorCommand::DeleteBackward],
                4,
            ),
            (
                vec![
                    EditorCommand::SetRegister {
                        text: "pp".into(),
                        linewise: false,
                    },
                    EditorCommand::Paste { after: false },
                ],
                7,
            ),
        ];
        for (cmds, want) in cases {
            let (mut ed, m) = marked("abcdef", 5);
            for cmd in cmds.clone() {
                ed.apply(cmd);
            }
            assert_eq!(ed.marker_position(m), Some(want), "after {cmds:?}");
        }
    }

    #[test]
    fn a_marker_is_never_past_the_end_of_its_buffer() {
        let (mut ed, m) = marked("alpha beta", 10);
        ed.apply(EditorCommand::DeleteRange(0, 10));
        assert_eq!(ed.marker_position(m), Some(0));
        assert_eq!(ed.buffer.len_chars(), 0);
    }

    #[test]
    fn undo_keeps_markers_inside_the_restored_text() {
        let (mut ed, m) = marked("alpha", 0);
        ed.apply(EditorCommand::Checkpoint);
        ed.apply(EditorCommand::MoveTo(5));
        ed.apply(EditorCommand::InsertText(" and beta".into()));
        ed.set_marker(m, 12);
        ed.apply(EditorCommand::Undo);
        assert_eq!(ed.buffer.text.to_string(), "alpha");
        // Undo restores a snapshot rather than replaying the edit backwards, so
        // a marker beyond the restored end is clamped onto it.
        assert_eq!(ed.marker_position(m), Some(5));

        // ...and one that still fits keeps the position it had, which is what
        // makes it survive an undo at all.
        let (mut ed, m) = marked("alpha beta", 6);
        ed.apply(EditorCommand::Checkpoint);
        ed.apply(EditorCommand::MoveTo(0));
        ed.apply(EditorCommand::InsertText("xy".into()));
        assert_eq!(ed.marker_position(m), Some(8));
        ed.apply(EditorCommand::Undo);
        assert_eq!(ed.marker_position(m), Some(8));
    }

    #[test]
    fn markers_belong_to_the_buffer_they_were_made_in() {
        let (mut ed, m) = marked("alpha", 3);
        ed.load("elsewhere", Some(PathBuf::from("/tmp/elsewhere")), None);
        assert_eq!(ed.marker_position(m), None, "another buffer's marker");
        // Editing the other buffer must not move it either.
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::InsertText("xxxx".into()));
        ed.switch_buffer(1);
        assert_eq!(ed.buffer.text.to_string(), "alpha");
        assert_eq!(ed.marker_position(m), Some(3));
    }

    #[test]
    fn a_deleted_marker_reads_as_gone_rather_than_as_a_position() {
        let (mut ed, m) = marked("alpha", 3);
        ed.delete_marker(m);
        assert_eq!(ed.marker_position(m), None);
        // Deleting it twice, or a handle that was never a marker, is not an error.
        ed.delete_marker(m);
        ed.set_marker(m, 1);
        assert_eq!(ed.marker_position(m), None);
        assert_eq!(ed.marker_position(9999), None);
    }

    /// Highlights belong to the buffer, not to the editor. A split showing two
    /// files draws both, and moving focus between panes must not cost either of
    /// them its colours — which is what a single editor-wide span list did.
    #[test]
    fn highlights_survive_a_buffer_switch() {
        let mut ed = Editor::new();
        ed.load("fn main() {}", None, Some("rust".into()));
        let first = ed.buffer.id;
        ed.buffer.highlights = vec![Span {
            start: 0,
            end: 2,
            kind: HlKind::Keyword,
        }];

        // Park it behind another buffer, colour that one too...
        ed.apply(EditorCommand::SwitchBuffer(1));
        assert_ne!(ed.buffer.id, first, "a different buffer is live");
        ed.buffer.highlights = vec![Span {
            start: 0,
            end: 1,
            kind: HlKind::String,
        }];
        let second = ed.buffer.id;

        // ...and the parked one still has its own.
        let parked = ed
            .others
            .iter()
            .find(|b| b.id == first)
            .expect("the first buffer is parked, not gone");
        assert_eq!(parked.highlights.len(), 1);
        assert_eq!(parked.highlights[0].kind, HlKind::Keyword);

        // Coming back keeps them, and does not inherit the other buffer's.
        let back = ed.buffer_names().iter().position(|n| n.contains("untitled"));
        let _ = back;
        ed.apply(EditorCommand::SwitchBuffer(1));
        assert_eq!(ed.buffer.id, first);
        assert_eq!(ed.buffer.highlights[0].kind, HlKind::Keyword);
        assert_ne!(ed.buffer.id, second);
    }

    /// ...but a buffer whose *text* was replaced has no business keeping spans
    /// that described the old text.
    #[test]
    fn regenerated_text_drops_its_highlights() {
        let mut ed = Editor::new();
        ed.load("fn main() {}", None, Some("rust".into()));
        ed.buffer.highlights = vec![Span {
            start: 0,
            end: 2,
            kind: HlKind::Keyword,
        }];
        ed.load("something else entirely", None, None);
        assert!(ed.buffer.highlights.is_empty());

        ed.buffer.highlights = vec![Span {
            start: 0,
            end: 2,
            kind: HlKind::Keyword,
        }];
        ed.show_special(BufferKind::Dired, "a listing");
        assert!(ed.buffer.highlights.is_empty());
    }

    #[test]
    fn selection_is_inclusive_of_cursor_char() {
        let mut ed = fresh("abcdef");
        feed(&mut ed, &[Key::Char('v'), Key::Char('l')]);
        assert_eq!(ed.selection(), Some((0, 2)));
    }

    // --- lisp-api ------------------------------------------------------------

    /// `from_token` is only worth having if it is the *exact* inverse of
    /// `token`: the string a config reads out of `(key-bindings)` is the string
    /// it sends to the shell, and a vocabulary that agreed only mostly would be
    /// worse than none.
    #[test]
    fn every_key_survives_being_spelled_and_read_back() {
        let all = [
            Key::Char('a'),
            Key::Char('A'),
            Key::Char(' '),
            Key::Char('-'),
            Key::Ctrl('c'),
            Key::Meta('x'),
            Key::CtrlMeta('j'),
            Key::CtrlEnter,
            Key::CtrlMetaEnter,
            Key::MetaBackspace,
            Key::Enter,
            Key::Tab,
            Key::Backspace,
            Key::Esc,
            Key::Left,
            Key::Right,
            Key::Up,
            Key::Down,
        ];
        for key in all {
            assert_eq!(Key::from_token(&key.token()), Some(key), "{}", key.token());
        }
        // A chord is folded to lower case on the way out, so that is what comes
        // back — `token` is the canonical spelling and this agrees with it.
        assert_eq!(Key::from_token("C-A"), Some(Key::Ctrl('a')));
        // A *sequence* is not a key. `normalize_keys` owns those.
        assert_eq!(Key::from_token("g d"), None);
        assert_eq!(Key::from_token("C-xy"), None);
        assert_eq!(Key::from_token("M-"), None);
        assert_eq!(Key::from_token(""), None);
    }

    /// A buffer with no file behind it, which is what `*Messages*` and every
    /// other generated-by-Lisp listing needs.
    #[test]
    fn a_created_buffer_is_named_addressable_and_idempotent() {
        let mut ed = Editor::new();
        ed.apply(EditorCommand::CreateBuffer("*notes*".into()));
        assert_eq!(ed.buffer.name(), "*notes*");
        assert!(ed.buffer.path.is_none());
        // An ordinary text buffer, not a generated one: Lisp owns it, so it has
        // to be writable.
        assert!(!ed.buffer.kind.is_generated());
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::InsertText("hello".into()));
        assert_eq!(ed.buffer.text.to_string(), "hello");

        // Twice is once: a command that fills a buffer must not stack a second
        // copy nobody can tell from the first.
        ed.apply(EditorCommand::CreateBuffer("*notes*".into()));
        assert_eq!(ed.buffer.text.to_string(), "hello");
        assert_eq!(
            ed.buffer_names().iter().filter(|n| n.starts_with("*notes*")).count(),
            1
        );
        // ...and it is reachable by name from somewhere else, which is what
        // `switch-to-buffer` and `with-current-buffer` match on.
        ed.apply(EditorCommand::CreateBuffer("*other*".into()));
        assert_eq!(ed.buffer.name(), "*other*");
        ed.apply(EditorCommand::CreateBuffer("*notes*".into()));
        assert_eq!(ed.buffer.name(), "*notes*");

        // The language is settable, so a created buffer can be coloured — the
        // difference between this and a scratchpad written to disk first.
        ed.apply(EditorCommand::SetLanguage(Some("lisp".into())));
        assert_eq!(ed.buffer.language.as_deref(), Some("lisp"));
        ed.apply(EditorCommand::SetLanguage(None));
        assert!(ed.buffer.language.is_none());
    }

    #[test]
    fn killing_a_buffer_never_leaves_a_window_pointing_at_it() {
        let mut ed = Editor::new();
        ed.apply(EditorCommand::CreateBuffer("*a*".into()));
        ed.apply(EditorCommand::SplitWindow(frame::Split::Columns));
        ed.apply(EditorCommand::CreateBuffer("*b*".into()));
        let doomed = ed.buffer.id;
        // Both panes are showing *b*: one because it is focused, and the other
        // because the split copied the buffer that was there.
        ed.apply(EditorCommand::KillBuffer(0));
        assert_ne!(ed.buffer.id, doomed);
        for frame in &ed.frames {
            for w in &frame.windows {
                assert!(ed.buffer_by_id(w.buffer).is_some());
                assert_ne!(w.buffer, doomed);
            }
        }

        // By index, too — 0 is the live one, anything else is a position in
        // `buffer-names`.
        let named: Vec<String> = ed.buffer_names();
        let at = named.iter().position(|n| n == "*a*").expect("*a* is open");
        ed.apply(EditorCommand::KillBuffer(at));
        assert!(!ed.buffer_names().iter().any(|n| n == "*a*"));

        // The last one is refused: something has to be on screen.
        while !ed.others.is_empty() {
            ed.apply(EditorCommand::KillBuffer(0));
        }
        let last = ed.buffer.id;
        ed.apply(EditorCommand::KillBuffer(0));
        assert_eq!(ed.buffer.id, last);
        assert_eq!(ed.status, "cannot kill the last buffer");
    }

    /// A scene of one paragraph, named by its own text so one buffer's page can
    /// be told from another's.
    fn page(text: &str) -> zemacs_gui::Scene {
        let mut scene = zemacs_gui::Scene::default();
        let node = scene.push(zemacs_gui::Node::Text {
            runs: vec![zemacs_gui::Run::Text {
                text: text.into(),
                style: zemacs_gui::Style::default(),
                tag: None,
            }],
            align: zemacs_gui::Align::Start,
        });
        scene.set_root(node);
        scene
    }

    /// The text of the page a buffer is showing, or `None` for a buffer drawn
    /// as a grid of cells like every other one.
    fn page_text(buffer: &Buffer) -> Option<String> {
        let scene = buffer.scene.as_ref()?;
        let zemacs_gui::Node::Text { runs, .. } = scene.node(scene.root()?)? else {
            return None;
        };
        match runs.first()? {
            zemacs_gui::Run::Text { text, .. } => Some(text.clone()),
            zemacs_gui::Run::Image { .. } => None,
        }
    }

    /// A scene is not an editing surface, and this is where that stops being a
    /// sentence in `docs/gui.org` and becomes the keyboard refusing you.
    #[test]
    fn installing_a_scene_makes_the_buffer_read_only_and_clearing_it_puts_it_back() {
        let mut ed = fresh("hello");
        assert_eq!(ed.buffer.read_only(), ReadOnly::No);
        ed.apply(EditorCommand::SetScene(Some(page("a"))));
        assert_eq!(ed.buffer.read_only(), ReadOnly::Claimed);
        // The same claim `org-frozen` makes, refused by the same one guard: no
        // way into Insert, and nothing gets typed.
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        assert_eq!(ed.mode, Mode::Normal);
        ed.apply(EditorCommand::InsertText("x".into()));
        assert_eq!(ed.buffer.text.to_string(), "hello");

        // A page rebuilt over the top of the last one is a swap, not a second
        // claim — that is what would leave the buffer frozen forever.
        ed.apply(EditorCommand::SetScene(Some(page("b"))));
        assert_eq!(page_text(&ed.buffer).as_deref(), Some("b"));
        ed.apply(EditorCommand::SetScene(None));
        assert!(ed.buffer.scene.is_none());
        assert_eq!(ed.buffer.read_only(), ReadOnly::No);

        // ...and "what was there before" is a claim as readily as the absence
        // of one: a page shown over a document a mode had already frozen
        // leaves it frozen on the way out.
        ed.apply(EditorCommand::SetReadOnly(true));
        ed.apply(EditorCommand::SetScene(Some(page("c"))));
        ed.apply(EditorCommand::SetScene(None));
        assert_eq!(ed.buffer.read_only(), ReadOnly::Claimed);
    }

    /// A scene is swapped in whole, which is the right API and would otherwise
    /// mean the reader loses their place on every re-render: a curriculum
    /// rebuilds its page when one problem's state changes, and somebody
    /// answering question nine would be thrown back to question one.
    #[test]
    fn re_installing_a_scene_keeps_the_reader_where_they_were() {
        let mut ed = fresh("");
        ed.apply(EditorCommand::SetScene(Some(page("a"))));
        // The wheel, in the pixels a scene scrolls in.
        ed.buffer.scene.as_mut().expect("a page is up").scroll = 300;

        // The incoming page carries an offset of its own and it loses: the
        // builder in Lisp described a document, not a position in one.
        let mut next = page("b");
        next.scroll = 5;
        ed.apply(EditorCommand::SetScene(Some(next)));
        assert_eq!(page_text(&ed.buffer).as_deref(), Some("b"));
        assert_eq!(ed.buffer.scene.as_ref().unwrap().scroll, 300);
    }

    /// The other half of the same rule: a scene taken away and a new one put up
    /// is a *different document arriving*, not the same one re-rendered, and it
    /// starts where a document starts.
    #[test]
    fn clearing_the_scene_and_installing_a_new_one_starts_at_the_top() {
        let mut ed = fresh("");
        ed.apply(EditorCommand::SetScene(Some(page("a"))));
        ed.buffer.scene.as_mut().expect("a page is up").scroll = 300;
        ed.apply(EditorCommand::SetScene(None));
        ed.apply(EditorCommand::SetScene(Some(page("b"))));
        assert_eq!(ed.buffer.scene.as_ref().unwrap().scroll, 0);
    }

    /// `(image ID)` with no size is the form a document wants to write, and a
    /// node of no size lays out to a box of no size — an equation nobody can
    /// see. Core is where the bitmap's real size lives, so core is where the
    /// number comes from.
    #[test]
    fn an_image_with_no_size_takes_the_size_of_its_bitmap() {
        let mut ed = fresh("");
        ed.add_image(
            7,
            Image { width: 120, height: 40, depth: 9, rgba: vec![0; 120 * 40 * 4] },
        );
        let mut scene = zemacs_gui::Scene::default();
        let figure = scene.push(zemacs_gui::Node::Image {
            image: 7,
            width: 0,
            height: 0,
            depth: 0,
        });
        // ...and the same for an image *in a sentence*, which is the case the
        // whole thing is being built for.
        let inline = scene.push(zemacs_gui::Node::Text {
            runs: vec![zemacs_gui::Run::Image {
                image: 7,
                width: 0,
                height: 0,
                depth: 0,
                tag: None,
            }],
            align: zemacs_gui::Align::Start,
        });
        // A figure somebody scaled on purpose keeps the number they chose.
        let scaled = scene.push(zemacs_gui::Node::Image {
            image: 7,
            width: 60,
            height: 20,
            depth: 0,
        });
        let root = scene.push(zemacs_gui::Node::Block(zemacs_gui::Block {
            children: vec![figure, inline, scaled],
            ..Default::default()
        }));
        scene.set_root(root);
        ed.apply(EditorCommand::SetScene(Some(scene)));

        let scene = ed.buffer.scene.as_ref().expect("a page is up");
        assert_eq!(
            scene.node(figure),
            Some(&zemacs_gui::Node::Image { image: 7, width: 120, height: 40, depth: 9 })
        );
        let Some(zemacs_gui::Node::Text { runs, .. }) = scene.node(inline) else {
            panic!("the paragraph survived the pass");
        };
        assert_eq!(
            runs.first(),
            Some(&zemacs_gui::Run::Image {
                image: 7,
                width: 120,
                height: 40,
                depth: 9,
                tag: None
            })
        );
        assert_eq!(
            scene.node(scaled),
            Some(&zemacs_gui::Node::Image { image: 7, width: 60, height: 20, depth: 0 })
        );
        // An id naming no bitmap is left exactly as it came rather than given
        // an invented size: it arrived from arithmetic in Lisp.
        let mut orphan = zemacs_gui::Scene::default();
        let node = orphan.push(zemacs_gui::Node::Image { image: 99, width: 0, height: 0, depth: 0 });
        orphan.set_root(node);
        ed.apply(EditorCommand::SetScene(None));
        ed.apply(EditorCommand::SetScene(Some(orphan)));
        assert_eq!(
            ed.buffer.scene.as_ref().unwrap().node(node),
            Some(&zemacs_gui::Node::Image { image: 99, width: 0, height: 0, depth: 0 })
        );
    }

    /// A bitmap a scene names and no overlay does survives the prune that some
    /// unrelated overlay's deletion triggers.
    ///
    /// The bug this is here for is quiet: the page keeps its `ImageId`, the
    /// image table stops resolving it, and the equation simply stops being
    /// drawn — some time after the edit that dropped it, in a buffer that had
    /// nothing to do with the page.
    #[test]
    fn deleting_an_overlay_does_not_drop_an_image_only_a_scene_still_names() {
        let mut ed = fresh("hello");
        let bitmap = |n| Image { width: n, height: n, depth: 0, rgba: vec![0; (n * n * 4) as usize] };
        ed.add_image(7, bitmap(10)); // the scene's figure
        ed.add_image(8, bitmap(12)); // an inline fragment on the same page
        ed.add_image(9, bitmap(14)); // nobody's, once the overlay below goes

        // An overlay naming an image, in the buffer that will hold the page —
        // and a *second* buffer with the scene on it, so the prune has to walk
        // more than the live one.
        let home = ed.buffer.id;
        let doomed = ed.make_overlay(0, 3);
        ed.apply(EditorCommand::Overlay(OverlayEdit::Image(doomed, Some(9))));

        let mut scene = zemacs_gui::Scene::default();
        let figure = scene.push(zemacs_gui::Node::Image {
            image: 7,
            width: 10,
            height: 10,
            depth: 0,
        });
        let sentence = scene.push(zemacs_gui::Node::Text {
            runs: vec![zemacs_gui::Run::Image {
                image: 8,
                width: 12,
                height: 12,
                depth: 2,
                tag: None,
            }],
            align: zemacs_gui::Align::Start,
        });
        let root = scene.push(zemacs_gui::Node::Block(zemacs_gui::Block {
            children: vec![figure, sentence],
            ..Default::default()
        }));
        scene.set_root(root);
        ed.apply(EditorCommand::CreateBuffer("*page*".into()));
        ed.apply(EditorCommand::SetScene(Some(scene)));
        ed.switch_buffer_id(home);

        ed.apply(EditorCommand::Overlay(OverlayEdit::Delete(doomed)));
        assert!(ed.has_image(7), "the figure is named by the page");
        assert!(ed.has_image(8), "so is the fragment in the sentence");
        assert!(!ed.has_image(9), "and nothing at all names the overlay's");
    }

    /// The whole argument for the field living on the buffer: there is no scene
    /// table, so there is nothing to sweep and nothing to leak.
    #[test]
    fn a_killed_buffer_takes_its_scene_with_it() {
        let mut ed = Editor::new();
        ed.apply(EditorCommand::CreateBuffer("*page*".into()));
        ed.apply(EditorCommand::SetScene(Some(page("gone"))));
        let doomed = ed.buffer.id;

        // Parked rather than lost: the page follows its document out of the
        // pane, which is what a split showing it somewhere else needs.
        ed.apply(EditorCommand::CreateBuffer("*plain*".into()));
        assert!(ed.buffer.scene.is_none(), "a fresh buffer is a grid of cells");
        assert_eq!(
            ed.buffer_by_id(doomed).and_then(page_text).as_deref(),
            Some("gone")
        );

        let at = ed
            .buffer_names()
            .iter()
            .position(|n| n.starts_with("*page*"))
            .expect("*page* is open");
        ed.apply(EditorCommand::KillBuffer(at));
        assert!(ed.buffer_by_id(doomed).is_none());
        assert!(
            std::iter::once(&ed.buffer)
                .chain(ed.others.iter())
                .all(|b| page_text(b).as_deref() != Some("gone")),
            "the scene went with the buffer, because it was never anywhere else"
        );
    }

    /// A scene belongs to a document, so parking one and bringing it back has
    /// to leave both pages — and both read-only claims — exactly as they were.
    #[test]
    fn switching_buffers_leaves_each_scene_on_its_own_buffer() {
        let mut ed = Editor::new();
        ed.apply(EditorCommand::CreateBuffer("*one*".into()));
        ed.apply(EditorCommand::SetScene(Some(page("one"))));
        ed.apply(EditorCommand::CreateBuffer("*two*".into()));
        ed.apply(EditorCommand::SetScene(Some(page("two"))));

        let index = |ed: &Editor, name: &str| {
            ed.buffer_names()
                .iter()
                .position(|n| n.starts_with(name))
                .expect("buffer is open")
        };
        let back = index(&ed, "*one*");
        ed.switch_buffer(back);
        assert_eq!(page_text(&ed.buffer).as_deref(), Some("one"));
        assert_eq!(ed.buffer.read_only(), ReadOnly::Claimed);
        let forward = index(&ed, "*two*");
        ed.switch_buffer(forward);
        assert_eq!(page_text(&ed.buffer).as_deref(), Some("two"));
        assert_eq!(ed.buffer.read_only(), ReadOnly::Claimed);

        // ...and a buffer that never had one still has none, which is the half
        // of "undisturbed" that a swap of two identical things would hide.
        ed.apply(EditorCommand::CreateBuffer("*three*".into()));
        assert!(ed.buffer.scene.is_none());
        ed.switch_buffer(index(&ed, "*one*"));
        assert_eq!(page_text(&ed.buffer).as_deref(), Some("one"));
    }

    /// The prompt whose answer goes back to Lisp. Core's half of it: open one,
    /// feed it candidates, and check that both accepting and cancelling produce
    /// the call that reaches the continuation — a cancel that said nothing would
    /// leave the closure parked in the image forever.
    #[test]
    fn a_lisp_prompt_answers_its_continuation_either_way() {
        let mut ed = fresh("");
        ed.apply(EditorCommand::ReadFromMinibuffer {
            id: 7,
            label: "Name: ".into(),
            completing: false,
        });
        assert!(!ed.prompt.as_ref().unwrap().kind.completes());
        let out = {
            let mut out = Vec::new();
            for k in [Key::Char('a'), Key::Char('b'), Key::Enter] {
                out = ed.handle_key(k);
            }
            out
        };
        assert_eq!(
            out,
            vec![EditorCommand::CallLisp("(%prompt-reply 7 \"ab\")".into())]
        );
        assert!(ed.prompt.is_none());

        // Escape answers NIL rather than nothing at all.
        ed.apply(EditorCommand::ReadFromMinibuffer {
            id: 8,
            label: "Name: ".into(),
            completing: false,
        });
        assert_eq!(
            ed.handle_key(Key::Esc),
            vec![EditorCommand::CallLisp("(%prompt-reply 8 nil)".into())]
        );

        // ...and backspacing past the start is the same gesture.
        ed.apply(EditorCommand::ReadFromMinibuffer {
            id: 9,
            label: "Name: ".into(),
            completing: false,
        });
        assert_eq!(
            ed.handle_key(Key::Backspace),
            vec![EditorCommand::CallLisp("(%prompt-reply 9 nil)".into())]
        );

        // A completing prompt draws its box, keeps the order Lisp gave, and
        // answers the highlighted candidate rather than the typed text.
        ed.apply(EditorCommand::ReadFromMinibuffer {
            id: 10,
            label: "Pick: ".into(),
            completing: true,
        });
        for c in ["alpha", "beta", "gamma"] {
            ed.apply(EditorCommand::PromptItem(c.into()));
        }
        {
            let p = ed.prompt.as_ref().unwrap();
            assert!(p.kind.completes());
            assert_eq!(p.current(), Some("alpha"));
            assert_eq!(p.matches.len(), 3);
        }
        let out = {
            let mut out = Vec::new();
            for k in [Key::Char('g'), Key::Char('a'), Key::Enter] {
                out = ed.handle_key(k);
            }
            out
        };
        assert_eq!(
            out,
            vec![EditorCommand::CallLisp("(%prompt-reply 10 \"gamma\")".into())]
        );

        // The answer crosses as Lisp *source*, so it has to survive READ.
        ed.apply(EditorCommand::ReadFromMinibuffer {
            id: 11,
            label: "s: ".into(),
            completing: false,
        });
        let out = {
            let mut out = Vec::new();
            for k in [Key::Char('"'), Key::Char('\\'), Key::Enter] {
                out = ed.handle_key(k);
            }
            out
        };
        assert_eq!(
            out,
            vec![EditorCommand::CallLisp(
                r#"(%prompt-reply 11 "\"\\")"#.into()
            )]
        );

        // A candidate arriving after the prompt has gone is dropped rather than
        // landing in whatever picker is up next.
        ed.open_prompt(PromptKind::Buffer);
        let before = ed.prompt.as_ref().unwrap().items.len();
        ed.apply(EditorCommand::PromptItem("stray".into()));
        assert_eq!(ed.prompt.as_ref().unwrap().items.len(), before);
    }

    // --- end of the lisp-api block -------------------------------------------
}
