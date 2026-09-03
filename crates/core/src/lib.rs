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
pub mod fuzzy;
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

/// How a thread that is not the main one says "there is something to draw".
///
/// The main loop parks in `SDL_WaitEventTimeout` and every other thread reaches
/// the editor through the mutex above, raising no window event. Without this
/// the loop's only way to notice a change made off the event queue was to wake
/// on a timer and look — sixty times a second, forever, for the once an hour
/// something is actually there. This is that timer, replaced by the signal it
/// was standing in for.
///
/// A global rather than a handle threaded through six constructors, for the
/// same reason [`Editor::generation`] is a counter rather than a callback: the
/// producers are in four crates that have no business knowing what a window is,
/// and there is exactly one editor per process by construction. `None` until
/// the app installs one, which is what keeps every test and every headless
/// caller working with no waker at all.
static WAKE: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();

/// Install the process's waker. The second call is ignored.
pub fn set_waker(f: impl Fn() + Send + Sync + 'static) {
    let _ = WAKE.set(Box::new(f));
}

/// Ask the main loop to come round. Cheap enough to call per primitive: the
/// installed waker coalesces, so a Lisp loop calling `(point)` ten thousand
/// times raises one event, not ten thousand.
pub fn wake() {
    if let Some(f) = WAKE.get() {
        f();
    }
}

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
    /// On macOS that is everything involving Command, plus the modified Enters.
    /// Ctrl is deliberately excluded: `C-c`, `C-a`, `C-d`, `C-r` and `C-w` all
    /// belong to the shell, and taking any of them would break it.
    ///
    /// `M-S-<left>`/`M-S-<right>` are here while `M-<left>`/`M-<right>` are not,
    /// and the split is the shell's own doing: a terminal reads `M-<left>` as
    /// word-wise motion (`Input::AltLeft`) and has no encoding at all for the
    /// shifted pair, so keeping them is taking nothing away from it.
    ///
    /// Home, End, the page keys, forward-Delete and the F-keys are all absent
    /// for that same test, and it is not close: they are line-start and
    /// line-end in readline, a screenful in every pager, and the whole menu bar
    /// of a TUI. Taking any of them would be taking the terminal's own keys.
    pub fn is_editor_key(self) -> bool {
        matches!(
            self,
            Key::Meta(_)
                | Key::CtrlMeta(_)
                | Key::CtrlEnter
                | Key::CtrlMetaEnter
                | Key::MetaEnter
                | Key::MetaShiftEnter
                | Key::MetaShiftLeft
                | Key::MetaShiftRight
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
    // ponytail: fifteen named keys carry a modifier now — the two window splits,
    // `M-<bs>`, `<backtab>`, the Meta'd arrows, and the Shift'd ones org's
    // structure keys are spelled with — where this note once set its threshold
    // at four. A modifier bitset over a `Named` enum is still the real answer
    // and still owed. It was not taken here because the debt is a match arm in
    // every crate that reads a `Key`, and eight more arms is a duller change
    // than rewriting ~400 `Key::` sites across fifteen files in the most
    // safety-critical code in the editor.
    CtrlEnter,
    CtrlMetaEnter,
    /// `⌘⏎`. org's `M-RET`: a new list item, a new heading — "another one of
    /// what this line is". A named key for [`Key::MetaBackspace`]'s reason, and
    /// it used to be dropped on the floor: `combo_char` cannot spell `<ret>`, so
    /// the `Meta(char)` arm answered `None` and the keystroke reached nothing.
    MetaEnter,
    /// `⌘⌫`. Deletes the word before point, and in a terminal is handed to the
    /// shell, which has its own idea of where a word starts.
    MetaBackspace,
    /// `⌘←` and `⌘→` — Meta plus an arrow, which is word-at-a-time motion
    /// everywhere it means anything. Named keys rather than `Meta(char)` for
    /// [`Key::MetaBackspace`]'s reason: an arrow has no character to carry.
    MetaLeft,
    MetaRight,
    /// `⇧⏎` and `⌘⇧⏎`. Shift is a modifier in this enum only on the keys below,
    /// and this is the one that asked for it: org spells "another one of these,
    /// but a *task*" `M-S-<ret>`, and until these existed there was no way to
    /// write that binding down at all.
    ShiftEnter,
    MetaShiftEnter,
    /// `⇧←` and its three relatives. Named keys for [`Key::MetaLeft`]'s reason —
    /// an arrow carries no character for `combo_char` to spell.
    ///
    /// Unbound they still mean what the bare arrow means (see `insert_key` and
    /// `Editor::motion`), so a shift left over from typing a capital moves the
    /// cursor as it always did rather than turning it into a dead key.
    ShiftLeft,
    ShiftRight,
    ShiftUp,
    ShiftDown,
    /// `⌘⇧←`/`⌘⇧→` — org's promote/demote *subtree*, which is precisely what
    /// plain `M-<left>` deliberately does not do. No `M-S-<up>`/`M-S-<down>`
    /// twins: nothing binds them, and a variant nothing produces is a match arm
    /// in four crates for a key nobody presses.
    MetaShiftLeft,
    MetaShiftRight,
    Enter,
    Tab,
    /// `⇧⇥`. Its own key and not a shifted `Tab`, because that is what every
    /// terminal has meant by it since the VT100 — `ESC [ Z`, which is how an
    /// agent's input box reads "the other way through the modes".
    BackTab,
    Backspace,
    Esc,
    Left,
    Right,
    Up,
    Down,
    /// The block above the arrows, which this enum simply had none of: every
    /// one of them was dropped on the floor in every mode, because `combo_char`
    /// spells a key by its name and all of these names are words.
    ///
    /// `Home` is *beginning of line* and not first-non-blank — `0`, not `^`.
    /// That is what Home does in every other text field on the machine, and
    /// `ESC [ H` is what readline reads the same way.
    Home,
    End,
    PageUp,
    PageDown,
    /// Forward delete — `⌦`, the far side of the cursor. Not
    /// [`Key::Backspace`], which macOS also prints "delete" on.
    Delete,
    /// `F1`–`F12`, numbered rather than twelve variants: they do nothing by
    /// default, so a variant each would be eleven more copies of `vec![]` in
    /// every crate that reads a `Key` — which is exactly the debt the note
    /// above is already carrying. Only 1..=12 is constructible, from
    /// [`Key::from_token`] or from a keystroke.
    F(u8),
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
            Key::MetaEnter => "M-<ret>".into(),
            Key::MetaBackspace => "M-<bs>".into(),
            Key::MetaLeft => "M-<left>".into(),
            Key::MetaRight => "M-<right>".into(),
            Key::ShiftEnter => "S-<ret>".into(),
            Key::MetaShiftEnter => "M-S-<ret>".into(),
            Key::ShiftLeft => "S-<left>".into(),
            Key::ShiftRight => "S-<right>".into(),
            Key::ShiftUp => "S-<up>".into(),
            Key::ShiftDown => "S-<down>".into(),
            Key::MetaShiftLeft => "M-S-<left>".into(),
            Key::MetaShiftRight => "M-S-<right>".into(),
            Key::Enter => "<ret>".into(),
            Key::Tab => "<tab>".into(),
            Key::BackTab => "<backtab>".into(),
            Key::Backspace => "<bs>".into(),
            Key::Esc => "<esc>".into(),
            Key::Left => "<left>".into(),
            Key::Right => "<right>".into(),
            Key::Up => "<up>".into(),
            Key::Down => "<down>".into(),
            Key::Home => "<home>".into(),
            Key::End => "<end>".into(),
            // `<pageup>`/`<pagedown>`, not Emacs' `<prior>`/`<next>`: this
            // codebase names a thing for what it does whenever the traditional
            // name is a polarity nobody remembers (see `LineOverflow`, and the
            // window splits named for the side they land on). `from_token`
            // still reads the Emacs spellings, so a binding copied out of an
            // `.emacs` is understood — it is only not what `token` writes.
            Key::PageUp => "<pageup>".into(),
            Key::PageDown => "<pagedown>".into(),
            Key::Delete => "<delete>".into(),
            Key::F(n) => format!("<f{n}>"),
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
            "<backtab>" | "S-<tab>" => Key::BackTab,
            "<bs>" | "<backspace>" => Key::Backspace,
            "<esc>" | "ESC" => Key::Esc,
            "<left>" => Key::Left,
            "<right>" => Key::Right,
            "<up>" => Key::Up,
            "<down>" => Key::Down,
            "C-<ret>" => Key::CtrlEnter,
            "C-M-<ret>" => Key::CtrlMetaEnter,
            "M-<ret>" => Key::MetaEnter,
            "M-<bs>" => Key::MetaBackspace,
            "M-<left>" => Key::MetaLeft,
            "M-<right>" => Key::MetaRight,
            // Spelled `M-S-`, never `S-M-`: one order, so a keymap cannot hold
            // the same chord twice under two names. Emacs' order too.
            "S-<ret>" => Key::ShiftEnter,
            "M-S-<ret>" => Key::MetaShiftEnter,
            "S-<left>" => Key::ShiftLeft,
            "S-<right>" => Key::ShiftRight,
            "S-<up>" => Key::ShiftUp,
            "S-<down>" => Key::ShiftDown,
            "<home>" => Key::Home,
            "<end>" => Key::End,
            "<pageup>" | "<prior>" => Key::PageUp,
            "<pagedown>" | "<next>" => Key::PageDown,
            "<delete>" | "<del>" => Key::Delete,
            "M-S-<left>" => Key::MetaShiftLeft,
            "M-S-<right>" => Key::MetaShiftRight,
            // `<f1>`…`<f12>`, and nothing outside that range: `zemacs-term` has
            // sequences for exactly these twelve, so an `<f13>` would be a
            // binding that parses and can then never be pressed.
            _ if s.starts_with("<f") && s.ends_with('>') => Key::F(
                s[2..s.len() - 1]
                    .parse()
                    .ok()
                    .filter(|n| (1..=12).contains(n))?,
            ),
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
    Truncate,
    /// Continue the line on the next row. The default: text you cannot read is
    /// worse than text that took two rows to show you, and the `→` marking the
    /// tail tells you a line is long without telling you what is on it.
    #[default]
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
    /// Markup faces, used by org mode. What a span carries is a *name*, and
    /// nothing more: a span is a fact about the text the parser found, and it is
    /// recomputed from scratch on every keystroke. Everything the name resolves
    /// to — a colour, and since [`FaceStyle`] a weight and a slant — is the
    /// theme's answer rather than the parser's, so a theme change repaints
    /// without re-parsing.
    ///
    /// Type *size* is the one attribute that stays off this list, and it stays
    /// off for a metric reason rather than a stylistic one: weight and slant
    /// pick a font handle and change no advance, while a size changes how many
    /// rows a line claims. So size lives on [`Overlay`](crate::Overlay), where
    /// it can be *put* by a mode and cleared without re-parsing the buffer.
    /// `org-modern.lisp` sets both: the face for the hue, `weight`/`slant` and
    /// `scale` for the shape.
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

    // ---------------------------------------------------------------------
    // The UI faces.
    //
    // Everything above is a fact about *text* — a parser found a keyword, a
    // theme says keywords are magenta. Everything below is a fact about the
    // *editor*: where point is, what is selected, what a panel is drawn on.
    // They live in the same table anyway, because the table is what Lisp
    // already knows how to talk to and a second one would be a second thing
    // to explain.
    //
    // They differ from the 22 in one way that matters: **they are optional.**
    // A face above this line that a theme forgets falls back to the body
    // colour, which is visibly wrong, which is why `themes.rs` insists every
    // theme names all 22. A face below it falls back to the ratio the
    // renderer used before these existed — `mix(bg, fg, 0.28)` for a region,
    // and so on — so a theme that names none of them looks exactly as it did.
    // That is the whole point: eleven shipped themes did not have to be
    // rewritten for the cursor to become themeable, and a theme that *does*
    // want its own cursor says so in one line.
    //
    // The cost of optional is that a face set by one theme survives into the
    // next one, which is the bug `themes.rs` exists to prevent for the 22.
    // `load-theme` answers it by clearing the table first — see `ResetFaces`.
    /// The selection band. A *background*: the text keeps its own faces.
    Region,
    /// The caret. The glyph underneath is knocked out to the background.
    Cursor,
    /// The stripe behind the line point is on. A background, and a subtle one —
    /// a few percent off the ground. Read as "you are here", not as a highlight.
    CurrentLine,
    /// Gutter digits.
    LineNumber,
    /// The digit on point's own line, which is the only one anybody reads.
    LineNumberCurrent,
    /// The rule between panes.
    Divider,
    /// Errors: an LSP diagnostic, dired's `D` flag, anything that has gone
    /// wrong. The face `lsp.lisp` says outright it was missing.
    Error,
    /// Warnings — the severity below [`HlKind::Error`], and the reason both had
    /// to arrive together: one of the two alone cannot say "this is worse".
    Warning,
    /// A search hit. A background, like [`HlKind::Region`], and drawn under it:
    /// the two mean different things and only one of them is where you are.
    ///
    /// Every hit on screen, in the document, plus the matched characters of a
    /// candidate in a completing prompt. While `/` is open it follows what is
    /// being *typed*, so a pattern lights the file up before you commit to it;
    /// after Enter it follows `last_search`, which is vim's `hlsearch`, and
    /// `:noh` is the way out.
    ///
    /// This entry used to say nothing drew it, and named the ceiling that
    /// stopped it: `overlays_for_line` is a linear scan per drawn line, so one
    /// overlay per hit was the wrong shape, and an *index* was the upgrade path
    /// it asked for. What it got is neither. [`Editor::search_hits`] scans the
    /// lines a pane is about to draw and answers char ranges — the same shape
    /// as the highlight spans beside it, recomputed rather than invalidated, and
    /// bounded by the window instead of by the file.
    Match,
    /// The fill of a floating panel: completion, which-key, corfu, the context
    /// menu, a tooltip. One face for all of them, because they are one surface
    /// appearing in several places, and a reader who has learnt what a panel
    /// looks like should not have to learn it twice.
    Popup,
    /// The 1px stroke around that panel.
    PopupBorder,
    /// The UI accent — selection bars, kind chips, the dashboard's rule. Every
    /// one of those hardcoded [`HlKind::Function`] before this existed, which
    /// worked because a function name is an accent colour in most themes and
    /// was luck in the rest. This is the theme saying which colour it meant.
    Accent,
}

impl HlKind {
    /// Every face, in `face-list` order — which is [`HlKind::face_id`] order,
    /// which is the order a scene numbers them in.
    pub const ALL: [HlKind; 34] = [
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
        HlKind::Region,
        HlKind::Cursor,
        HlKind::CurrentLine,
        HlKind::LineNumber,
        HlKind::LineNumberCurrent,
        HlKind::Divider,
        HlKind::Error,
        HlKind::Warning,
        HlKind::Match,
        HlKind::Popup,
        HlKind::PopupBorder,
        HlKind::Accent,
    ];

    /// The faces a theme is *required* to name — the first 22, the ones about
    /// text. `ALL` minus the UI faces, and the list `crates/lisp/tests/themes.rs`
    /// holds every shipped theme to.
    ///
    /// A prefix of [`HlKind::ALL`] rather than its own array, so a face added to
    /// one cannot go missing from the other. The split is the point: a theme
    /// that forgets `keyword' is broken and a theme that leaves `cursor' to the
    /// renderer's own ratio is not, and one test cannot say both at once.
    pub const CORE: &'static [HlKind] = {
        let (core, _ui) = HlKind::ALL.split_at(22);
        core
    };

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
            HlKind::Region => "region",
            HlKind::Cursor => "cursor",
            HlKind::CurrentLine => "current-line",
            HlKind::LineNumber => "line-number",
            HlKind::LineNumberCurrent => "line-number-current",
            HlKind::Divider => "divider",
            HlKind::Error => "error",
            HlKind::Warning => "warning",
            HlKind::Match => "match",
            HlKind::Popup => "popup",
            HlKind::PopupBorder => "popup-border",
            HlKind::Accent => "accent",
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

/// The half of a face that picks a *font handle* rather than a colour.
///
/// `FaceStyle` and not `Style`, which is the name it wants: `zemacs_gui::Style`
/// already exists and the renderer already imports it, and a third same-named
/// type in one namespace is how a blit ends up in the wrong coordinate space —
/// the comment on `Rect` over in `crates/render` tells that story at length.
///
/// Both bits false is the default and is the whole of backward compatibility: a
/// theme that never mentions weight gets exactly the faces it got before this
/// type existed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FaceStyle {
    pub bold: bool,
    pub italic: bool,
}

/// The faces: a colour and a [`FaceStyle`] per [`HlKind`], both settable from
/// Lisp — `set-syntax-color` for the one, `set-face-style` for the other, and
/// `set-face` in `init.lisp` for both at once.
#[derive(Clone, Debug)]
pub struct Theme {
    map: HashMap<HlKind, [f32; 3]>,
    /// Weight and slant, for the faces that have asked for one. A second map
    /// rather than a wider value in the first, because the two halves are set by
    /// two primitives and consumed at two different moments — a colour picks the
    /// `set_color_mod`, a style picks the font handle — and because absence has
    /// to keep meaning "the default": a face with a colour and no style is the
    /// overwhelmingly common case and must not have to name one.
    styles: HashMap<HlKind, FaceStyle>,
}

impl Default for Theme {
    fn default() -> Self {
        // A calm, slightly cool palette to match the default background. No
        // entry in `styles`: the default theme is upright body weight
        // throughout, and a theme has to ask before anything else happens.
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
        Self {
            map,
            styles: HashMap::new(),
        }
    }
}

impl Theme {
    pub fn color(&self, kind: HlKind, fallback: [f32; 3]) -> [f32; 3] {
        self.map.get(&kind).copied().unwrap_or(fallback)
    }

    pub fn set(&mut self, kind: HlKind, rgb: [f32; 3]) {
        self.map.insert(kind, clamp3(rgb));
    }

    /// No fallback argument, unlike [`Theme::color`]: there is a right answer
    /// for a face nobody styled — upright body weight — where there is no right
    /// answer for a colour, which is why that one has to be told the pane's
    /// foreground and this one does not.
    pub fn style(&self, kind: HlKind) -> FaceStyle {
        self.styles.get(&kind).copied().unwrap_or_default()
    }

    pub fn set_style(&mut self, kind: HlKind, style: FaceStyle) {
        self.styles.insert(kind, style);
    }

    /// Forget every face, colour and style alike.
    ///
    /// Empty rather than back to [`Theme::default`], and the difference is the
    /// whole reason this exists: a theme is a pile of assignments into a table,
    /// so a face the incoming theme does not mention keeps the *outgoing*
    /// theme's answer. `crates/lisp/tests/themes.rs` holds the 22 core faces to
    /// naming all of themselves, which closes that hole by making the pile
    /// total — but the UI faces are deliberately optional, and an optional face
    /// cannot be made total by a test. So `load-theme` empties the table first
    /// and every unset face falls through to the renderer's own ratio, which is
    /// the one answer that is right regardless of what was loaded before.
    ///
    /// `Default` would put eleven arbitrary colours back instead, which is a
    /// twelfth theme nobody asked for showing through the gaps in the eleventh.
    pub fn reset(&mut self) {
        self.map.clear();
        self.styles.clear();
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
    /// Insert `text` at a char offset, leaving point after it. The insert twin
    /// of [`EditorCommand::DeleteRange`] — for an edit whose position is
    /// computed rather than "wherever the cursor is". See `Buffer::insert_at`.
    InsertAt(usize, String),
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
    /// Set the `%N` modeline note. Empty takes it down.
    SetModelineNote(String),
    Quit,

    // --- settings, all reachable from Lisp ---
    SetFontSize(f32),
    /// Set the body font by file path, or `None` to go back to the renderer's
    /// own search. Takes effect on the next frame — see `Renderer::sync`.
    SetFontPath(Option<String>),
    /// Ask the app what fonts are installed. Answered asynchronously, by calling
    /// `(%fonts-listed ...)` in the image: core does no IO and has no font
    /// library, so this is a *request* rather than a query, in the shape
    /// [`EditorCommand::Term`] and [`EditorCommand::Project`] already have.
    ListFonts,
    /// Write whatever picture is on the clipboard to a file, and tell the image
    /// where it went by calling `(%clipboard-image "PATH")` — or with NIL when
    /// there is none.
    ///
    /// A *request*, in `ListFonts`' shape and for its reason: core does no IO
    /// and has no clipboard, and the layer that owns a window is the only one
    /// that can reach the pasteboard. Answered asynchronously, so the command
    /// that asked has already returned by the time the path arrives — which is
    /// why the answer is a call into Lisp rather than a value.
    ClipboardImage,
    SetBackground([f32; 3]),
    SetForeground([f32; 3]),
    SetSyntaxColor(String, [f32; 3]),
    /// The face's weight and slant, by name: bold then italic. Separate from
    /// [`EditorCommand::SetSyntaxColor`] rather than folded into it because a
    /// theme overwhelmingly wants to set a colour and say nothing about weight,
    /// and one command carrying both would make every such line say `nil nil`.
    SetFaceStyle(String, bool, bool),
    /// Empty the face table — see [`Theme::reset`]. `load-theme` sends this
    /// before it loads the file, so nothing survives from the theme before.
    ResetFaces,
    SetLineNumbers(bool),
    SetTabWidth(usize),
    /// Space-separated line endings that open a block, for auto-indent — see
    /// [`Settings::indent_openers`]. Empty is "only copy the previous indent".
    SetIndentOpeners(String),
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
    /// See [`Settings::scroll_past_end`].
    SetScrollPastEnd(bool),

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
    /// Magnify the focused window by `n` steps of [`frame::ZOOM_STEPS`]; `0`
    /// puts it back to 100%.
    ///
    /// The window and not the frame, and not `SetFontSize`: `font_size` is the
    /// size of text everywhere and stays exactly that, while this is one pane
    /// asking to be larger than its neighbour. Reading a paper beside the code
    /// you are writing about it is the case, and it is the case a per-frame
    /// zoom cannot serve at all.
    ZoomWindow(i32),
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
    /// `"mkdir"`, `"create-file"`, `"toggle-hidden"`, `"refresh"`. Core has no
    /// filesystem in it.
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
        /// The global key sequence for the same command, drawn flush right.
        /// Empty for none — see [`dashboard::Item::hint`].
        hint: String,
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

    /// Re-read the live buffer from the file it is visiting, discarding what is
    /// in it.
    ///
    /// The manual half of auto-revert, and it exists because the automatic half
    /// deliberately refuses one case: a buffer with unsaved changes is never
    /// silently clobbered, so a file rewritten underneath it — by a formatter,
    /// by `git checkout`, by an agent editing the file you are sitting in — is
    /// announced and *not* taken. This is how you then say "take it anyway",
    /// and it is guarded by a [`EditorCommand::Confirmed`] in exactly that case
    /// because the text it discards has never been written down anywhere.
    ///
    /// The app's, not core's: the file is the app's to read, and the splice that
    /// keeps markers and overlays alive is [`Editor::revert_buffer`].
    RevertBuffer,

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
        /// Send the highlighted candidate back as `%prompt-preview` every time
        /// the selection moves — consult's live preview, with the *meaning* of
        /// it left in the image. See [`PromptKind::Lisp`].
        previewing: bool,
    },
    /// Append one candidate to the open Lisp prompt. Ignored for any other
    /// prompt — the file and grep pickers get their candidates from the app,
    /// and a stray item from a continuation that has already been cancelled
    /// must not land in somebody else's list.
    PromptItem(String),
    /// The same, for a whole list at once: one candidate per line.
    ///
    /// [`EditorCommand::PromptItem`] is one lock and one crossing of the shim
    /// *per candidate*, which `shim.c` says outright is fine for the few hundred
    /// a command offers and wants a different route for a list long enough to
    /// feel. A project's file list is that list — tens of thousands of paths —
    /// and this is that route: `completing-read` sends its candidates in one
    /// string, whatever the length.
    ///
    /// Newline-separated because the write envelope carries one string and a
    /// candidate is a row in a one-line-per-row popup, so it has nowhere to put
    /// a newline of its own.
    // ponytail: a candidate containing a newline splits into two. Nothing in the
    // tree produces one; the upgrade is a separator the payload cannot hold,
    // which means not a C string.
    PromptItems(String),
    /// Replace what has been typed into the open prompt, and re-rank against it.
    ///
    /// The seeding half of the gap `docs/boundary.org` recorded as *"Seed a
    /// prompt with text or candidates core owns"*. `project-open` wants the file
    /// picker to start at an expanded `~/` so the app's absolute completions
    /// match; `dired-rename` wants the name being renamed. Neither could be said
    /// from Lisp, so both were Rust that only existed to set one field.
    SetPromptText(String),
    /// Retitle the open prompt. The other half: a picker that Lisp filled owes
    /// the reader a label naming what it filled it with.
    SetPromptLabel(String),
    /// Fill the open prompt from a list **the app owns** — `"SOURCE ARGUMENT"`,
    /// split at the first space.
    ///
    /// The candidates never enter the image, which is the whole point. A
    /// project's files come off a cached tree walk that has to answer between
    /// two keystrokes; handing them to Lisp so Lisp could hand them back would
    /// be two crossings of a list that exists precisely because walking it is
    /// too slow to do twice. So Lisp owns the verb, the label and what accepting
    /// one means, and says *which* list it wants by name.
    ///
    /// See [`EditorCommand::needs_app`]: the sources are the app's, as
    /// [`EditorCommand::Dired`] and [`EditorCommand::Git`] are.
    PromptSource(String),

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

    /// Append one segment to the modeline's format, or — with `None` — empty
    /// both sides of it.
    ///
    /// [`EditorCommand::WhichKey`]'s shape, for its reason: the write envelope
    /// carries one string, a format is many, and clearing has to be sayable so a
    /// config can build a strip rather than append to the shipped one.
    ///
    /// The difference from which-key is what happens next. A which-key row is
    /// text that gets drawn; this is a *template* that gets expanded per pane per
    /// frame, so the image speaks once and pays nothing per frame. See
    /// [`modeline`](crate::modeline) for the codes and for why it is not a
    /// callback.
    Modeline(Option<(bool, modeline::Spec)>),

    /// corfu: the in-buffer completion popup, which is which-key's sibling and
    /// is filled the same way — see [`CompletionEdit`] and [`Completion`].
    Completion(CompletionEdit),

    /// Hand the **next keystroke** to the image, as `(FUNCTION "a")`, instead of
    /// to the editor. `None` stops wanting it. The key is spelled the way
    /// [`Key::token`] spells one, so `<esc>` arrives as `"<esc>"` and what counts
    /// as a label is entirely the image's opinion.
    ///
    /// avy, and the fourth answer to "a label drawn over the frame". The other
    /// three all draw *in core* — [`Editor::ace`]'s window letters,
    /// [`EditorCommand::WhichKey`]'s panel, [`Completion`]'s popup — and this one
    /// draws nothing at all, because avy's labels are `display` overlays and the
    /// image has owned those since overlays landed. What is left over is the one
    /// fact the image cannot have for itself: *the next keystroke, whatever it
    /// is.* So that is the whole of what crosses.
    ///
    /// **Not a prompt**, which is which-key's and corfu's objection and is
    /// sharpest here: a prompt opens a minibuffer and owns the keyboard, and
    /// avy's entire gesture is one key pressed against the *document*.
    ///
    /// **Not `ace`**, though it is the closest relative and was tried first:
    /// `ace` resolves the label itself, into a [`WindowId`] core owns. A jump
    /// target is a buffer offset hung off an overlay, which lives in the image —
    /// so core resolving it means core keeping a second copy of a label table
    /// Lisp already has, and the two disagreeing is exactly the half-cleared
    /// screenful of labels this feature must never leave behind.
    ///
    /// **Not a Lisp-only transient keymap**, which was the laziest candidate and
    /// is wrong on the case that matters: a keymap catches the keys that are *in*
    /// it, so a key that is not a label falls through and edits the buffer with
    /// the labels still up. "Anything else cancels" is not expressible as a
    /// keymap; it is expressible as three lines here.
    ///
    /// A function *name* rather than a bool, which costs the same one field and
    /// keeps the word "avy" out of core — core is not the layer that knows what a
    /// label means, and this is the second caller's door as well as the first's.
    GrabKey(Option<String>),

    // --- end of the lisp-api block -------------------------------------------
}

/// One change to the in-buffer completion popup.
///
/// Two verbs rather than four, and the split is between *where the popup is*
/// and *what is in it*, because those two facts change at different rates: the
/// candidate list is fetched once per word and the selection moves once per
/// `C-n`. Folding them together would mean resending the whole list — one
/// command per candidate, since the write envelope carries a single string — on
/// every press of the key whose entire job is to be cheap.
#[derive(Clone, Debug, PartialEq)]
pub enum CompletionEdit {
    /// Put the popup under the text starting at the first field with the
    /// second's row highlighted, or — with `None` — take it down.
    ///
    /// The rows survive a re-`Show` *at the same anchor* and are dropped when
    /// the anchor moves, because a row list is a claim about the word starting
    /// there. So moving the selection is one command, and starting a new
    /// completion cannot inherit the last one's candidates even if the caller
    /// forgets to clear them.
    Show(Option<(usize, usize)>),
    /// Append one candidate, or — with `None` — empty the list without taking
    /// the popup down. Exactly [`EditorCommand::WhichKey`]'s idiom, and for
    /// exactly its reason: one string per call is what the envelope carries.
    ///
    /// A row is tab-separated into *kind glyph*, *label* and *detail*, and a row
    /// with no tabs in it is all label — so a caller that knows nothing about
    /// the columns still draws correctly. The split is here rather than in the
    /// image because the columns have to be *aligned*, which means measuring
    /// cells, which is the renderer's side of the boundary.
    Row(Option<String>),
    /// Append one line of documentation for the *selected* candidate, or — with
    /// `None` — empty it. Resent whenever the selection moves, which is the one
    /// place this differs from [`CompletionEdit::Row`]: a doc is a fact about
    /// one candidate rather than about the list, and there is no anchor to hang
    /// its lifetime on.
    Doc(Option<String>),
}

/// The in-buffer completion popup: candidates for the word being typed, drawn
/// at point rather than in the minibuffer.
///
/// **Not overlays**, and this is the third time that answer has been the wrong
/// one (see [`EditorCommand::WhichKey`] and `which-key.lisp`). An overlay is
/// anchored to a *range of buffer text* and slides when you type in front of
/// it — which sounds right here, since the popup is about the word under point,
/// and is not: an overlay's payload is drawn *in the cell grid, in the flow of
/// the line*, so a ten-candidate list would have to be ten lines of the
/// document that are not in the file. The popup floats over lines it is not
/// about. That is a frame-level surface, which is what `ace` and `which_key`
/// already are, so this is filed beside them.
///
/// **Not a prompt** either, for which-key's reason turned inside out: a prompt
/// owns the keyboard, and here the keyboard has to keep reaching the *buffer* —
/// the whole feature is that you go on typing and the list narrows.
///
/// The one thing it has that which-key does not is a position, since it is
/// anchored to the document rather than to the frame. `at` is a buffer offset
/// and the renderer turns it into pixels, because where a character is on
/// screen depends on wrapping, folds, a window's zoom and an overlay's type
/// scale — none of which core knows and all of which the renderer has just
/// finished computing in order to draw the caret.
#[derive(Clone, Debug, Default)]
pub struct Completion {
    /// Where the text being completed starts. Both the popup's anchor and the
    /// start of the range a candidate replaces, which is deliberately one
    /// number: a popup that pointed at one word while accepting over another
    /// is the failure mode worth designing out.
    pub at: usize,
    pub rows: Vec<String>,
    /// Index into `rows`. Kept clamped by [`Editor::apply`] rather than trusted,
    /// since it arrives from the image and the renderer indexes with it.
    pub selected: usize,
    /// The selected candidate's documentation, one line per entry, drawn in a
    /// second box beside the list — vscode's shape, and the one thing a bare
    /// list of identifiers cannot tell you.
    pub doc: Vec<String>,
    /// Highlight spans over `doc` joined by newlines, filled in by the *app*
    /// layer: core cannot parse (`zemacs-syntax` depends on core, not the other
    /// way round) and the renderer has no parser either.
    ///
    /// `None` means "nobody has parsed this yet" and draws as plain text;
    /// `Some(vec![])` means "parsed, and there was nothing to colour". Two
    /// states rather than one empty vector, because a doc that is pure prose
    /// would otherwise look unparsed forever and be re-parsed every frame.
    pub doc_spans: Option<Vec<Span>>,
    /// True from the moment a key asked the image to accept a candidate until
    /// the image takes the popup down.
    ///
    /// Set in core and cleared in core, because the round trip through Lisp is
    /// a queue turn wide (`docs/threading.org`) and `RET RET` is faster than
    /// that: without it the second `RET` sees a popup that is still up, accepts
    /// the same candidate a second time, and eats the newline that was meant.
    pub accepting: bool,
}

impl Completion {
    /// The rows to draw, and where in `rows` they start — scrolled to keep the
    /// selection visible, as [`minibuffer::Prompt::visible`] does for the other
    /// popup. Separate from that one because a `Prompt` row carries an index
    /// into `items` for the highlighter and a `selected` flag per row; here
    /// there is one list and one integer, so the caller subtracts.
    pub fn visible(&self, max: usize) -> (usize, &[String]) {
        if max == 0 || self.rows.is_empty() {
            return (0, &[]);
        }
        let first = self
            .selected
            .saturating_sub(max - 1)
            .min(self.rows.len().saturating_sub(max));
        (first, &self.rows[first..(first + max).min(self.rows.len())])
    }
}

/// The menu a right-click puts under the pointer.
///
/// **Pixels, in core**, which is unusual here and is the same exception
/// [`frame::Frame::divider_at`] already is: this is anchored to *the pointer*
/// and not to anything in the document, so there is no offset for the renderer
/// to convert. A [`Completion`] carries a buffer offset for the opposite
/// reason.
///
/// **A short list.** The verbs are the ones a mouse is plausibly reaching for —
/// a second window, a split, closing one — and they are built-ins, so a menu
/// works in a build with no config at all. [`Editor::open_context_menu`] adds
/// dired's **New File** on top of them in a listing, which is the only place
/// the list is not the same everywhere. ponytail: not extensible from Lisp. The
/// upgrade path is the `which-key`/`completion-row` idiom, one string per call,
/// and it is worth building the first time somebody wants their own entry in
/// here rather than a keybinding.
#[derive(Clone, Debug)]
pub struct ContextMenu {
    pub x: i32,
    pub y: i32,
    /// `(label, verb)`. The verb goes through [`Editor::run_action`], which is
    /// the same door a keybinding uses — so a menu entry cannot do anything a
    /// key could not.
    pub items: Vec<(&'static str, &'static str)>,
    /// The row under the pointer, for hover. `None` between the rows and in the
    /// padding, which is the honest answer and stops a click there picking the
    /// nearest one.
    pub hover: Option<usize>,
}

impl Default for ContextMenu {
    fn default() -> Self {
        ContextMenu {
            x: 0,
            y: 0,
            items: vec![
                ("New Window", "new-frame"),
                ("Split Right", "split-window-right"),
                ("Split Below", "split-window-below"),
                ("Close Window", "delete-window"),
            ],
            hover: None,
        }
    }
}

/// The box the pointer resting on something puts under itself — a diagnostic's
/// message when you hover its mark in the gutter.
///
/// The fourth surface drawn *over* the frame rather than in it, after
/// [`Editor::ace`], [`EditorCommand::WhichKey`] and [`Completion`], and the
/// second one anchored in **pixels** — [`ContextMenu`] is the first, for the
/// identical reason: this hangs off *the pointer* and not off anything in the
/// document, so there is no offset for the renderer to convert. A `Completion`
/// carries a buffer offset because it is anchored to a word.
///
/// **Not an `after-string` overlay**, which is what `lsp.lisp` predicted the
/// answer would be, and it is worth saying why the prediction does not survive
/// contact with the word *hover*. That note was written for an **inline**
/// message — text sitting permanently at the end of the offending line, the way
/// some editors show diagnostics — and both halves of it are wrong here. An
/// `after-string` is pinned to the end of a line, and a hover box belongs at the
/// pointer, which may be forty columns away in the margin; and an `after-string`
/// is *on* until something removes it, where the entire contract of a hover is
/// that it is gone the moment you look away. A property that is permanent and
/// line-anchored cannot express a thing that is transient and pointer-anchored.
/// The inline-message feature is still an `after-string` and is still unbuilt.
///
/// **Not the echo area**, which already does this: `SPC l e` puts the same text
/// in [`Editor::status`]. That is the keyboard's answer to the question and it
/// stays; a message in the echo area is not "over the thing you are pointing
/// at", it is at the bottom of the frame, and it survives until the next message
/// overwrites it rather than until the pointer moves.
///
/// **Text, not rows.** Deliberately one string and not a `Vec<String>`: where a
/// paragraph of prose breaks depends on how many cells the box has, which is the
/// renderer's arithmetic and nobody else's — the same division of labour that
/// keeps [`Completion::at`] a buffer offset. It carries newlines, because a
/// server that sends a two-paragraph diagnostic meant them.
///
/// Filling it is [`Editor::help_echo_at`] plus whoever is holding the pointer;
/// emptying it is *four* places, and that is the design rather than an
/// oversight — see the callers, and see [`Completion`] for why a hook was not
/// good enough at exactly this.
#[derive(Clone, Debug, PartialEq)]
pub struct Tooltip {
    /// Which OS window's pixels `x` and `y` are in. [`ContextMenu`] gets away
    /// without one because a right-click focuses the frame it landed in first,
    /// and *pointing* at a window deliberately does not focus it — so without
    /// this, hovering a mark in a second frame draws the box in the first one,
    /// at coordinates that mean nothing there.
    pub frame: usize,
    pub x: i32,
    pub y: i32,
    pub text: String,
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
                | EditorCommand::InsertAt(..)
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
                | EditorCommand::RevertBuffer
                // Whatever it wraps was guarded by a layer that only the app
                // has — the filesystem, or git.
                | EditorCommand::Confirmed(_)
                | EditorCommand::Git(_)
                | EditorCommand::Dired(_)
                | EditorCommand::OpenAt(_)
                | EditorCommand::Project(_)
                // The list is the app's — a cached tree walk core has no crate
                // for. See [`EditorCommand::PromptSource`].
                | EditorCommand::PromptSource(_)
                | EditorCommand::ListFonts
                | EditorCommand::ClipboardImage
                | EditorCommand::Term(_)
                | EditorCommand::TermKey(_)
                | EditorCommand::CallLisp(_)
                | EditorCommand::CloseFrame
        )
    }
}

// `Clone` but no longer `Copy`: `font_path` is a `String`, and every reader in
// the tree already takes `&Settings` — so the bound was costing nothing and
// keeping it would have meant putting the font somewhere other than the settings
// it plainly is one of.
#[derive(Clone, Debug)]
pub struct Settings {
    pub font_size: f32,
    /// The file the body text is set in, or `None` for the renderer's own
    /// search. A *path* and not a family name, because that is what opening a
    /// font takes and core has no font library to resolve a name with — see
    /// `set-font` in `runtime/library.lisp`, which is where a name becomes one.
    pub font_path: Option<String>,
    pub background: [f32; 3],
    pub foreground: [f32; 3],
    pub line_numbers: bool,
    pub tab_width: usize,
    /// What, at the end of a line, means the next line is *inside* something —
    /// so a newline after it indents one step further.
    ///
    /// `":"` for Python, `"{"` for the C family, `"("`, `"["`, `"do"`, `"then"`
    /// for the shells. A **table rather than a parser**, and deliberately: this
    /// runs on the Enter key, where a tree-sitter query would be a parse per
    /// keystroke to answer a question a suffix test gets right almost always. It
    /// is also the half a config has an opinion about, which is why it arrives
    /// from Lisp — `runtime/modes/modes.lisp` claims it per major mode, exactly
    /// as it claims the tab width beside it.
    ///
    /// Empty by default, which is *only* copy-the-previous-indent. A plain text
    /// buffer where `Notes:` opened a block would be worse than one that does
    /// nothing clever at all.
    ///
    /// Global rather than per buffer, unlike [`Buffer::text_width`], and the
    /// difference is which question is being asked: a measure is *drawn* for
    /// every pane at once, and this is only ever read for the buffer being typed
    /// into, which is by construction the live one.
    pub indent_openers: Vec<String>,
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
    /// Let the view keep going after the last line, into rows the document does
    /// not reach — Emacs, vim's `~` filler, VSCode's `scrollBeyondLastLine`.
    ///
    /// **Nothing is inserted.** The buffer is untouched and the empty rows are
    /// not a place: point cannot go there, the gutter does not number them, and
    /// a click in them lands at `point-max`. The whole feature is one number,
    /// [`Editor::max_scroll`], and this is the switch on it.
    pub scroll_past_end: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size: 18.0,
            font_path: None,
            background: [0.06, 0.06, 0.09],
            foreground: [0.86, 0.90, 1.00],
            line_numbers: true,
            tab_width: 4,
            indent_openers: Vec::new(),
            completion_style: CompletionStyle::default(),
            modeline_relief: 2,
            modeline_pad: 8,
            line_overflow: LineOverflow::default(),
            relative_line_numbers: false,
            text_width: 0,
            // On, because both editors this one is measured against do it and
            // neither offers a way not to: vim draws `~` past the end and Emacs
            // lets the last line reach the top of the window. It is the switch
            // to turn it *off* that is the feature here.
            scroll_past_end: true,
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

/// One edit to a document, in **character** offsets: `start..old_end` of the
/// text before it became `start..new_end` of the text after it.
///
/// Three numbers rather than two, and the shape is not invented here — it is
/// the intersection of the two consumers that were already asking for it.
/// tree-sitter's `InputEdit` is this triple restated in bytes and row/columns,
/// both of which are recoverable from the text and neither of which core has
/// any business holding an opinion about; LSP's incremental `didChange` is the
/// same triple wearing line/character clothes, plus the replacement text.
/// Neither wanted a number the other did not, which is why there is one record
/// and not two.
///
/// Offsets are characters because *every* offset in this editor is — `point`,
/// `region`, markers, overlays, spans. A record in bytes would be the one place
/// arithmetic against the rest of the API silently lied on a buffer containing
/// an accent.
///
/// **The inserted text is deliberately absent.** It is exactly `start..new_end`
/// of the buffer, for as long as no later edit has moved those offsets, so
/// whoever needs it takes it at the moment it reads the record — see
/// `crates/app`, which slices it under the same lock that read the record and
/// hands the image a self-contained pair. Keeping it here would mean the log of
/// a `replace-region` over a 10 MB buffer weighed 10 MB, for a copy most
/// readers throw away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub start: usize,
    pub old_end: usize,
    pub new_end: usize,
}

impl Change {
    /// The single record that covers a run of consecutive edits — the text
    /// before the first became the text after the last.
    ///
    /// Conservative rather than minimal: the answer is the smallest record
    /// derivable *from the records alone*, which can be wider than a diff of
    /// the two texts would give (typing `a` then deleting it reports a
    /// one-character change rather than none). Narrowing it would mean holding
    /// both texts and comparing them, which is the diffing this whole mechanism
    /// exists to avoid, and every consumer treats a too-wide record as correct
    /// and merely slower.
    ///
    /// `None` for an empty run, which is the honest answer to "what changed?"
    /// when nothing did — a revision can move without the text moving.
    pub fn coalesce(changes: &[Change]) -> Option<Change> {
        let (first, rest) = changes.split_first()?;
        let mut acc = *first;
        for c in rest {
            // How far the accumulated record has already shifted everything
            // after it: later records are in *current* coordinates, `old_end`
            // is in the coordinates of the text before the run began, and this
            // is the only quantity that translates between them.
            let shift = acc.new_end as i64 - acc.old_end as i64;
            acc = Change {
                start: acc.start.min(c.start),
                old_end: acc.old_end.max((c.old_end as i64 - shift).max(0) as usize),
                new_end: (acc.new_end as i64 + c.new_end as i64 - c.old_end as i64)
                    .max(c.new_end as i64) as usize,
            };
        }
        Some(acc)
    }
}

/// Every edit made to one buffer, oldest first, and a count of how many there
/// have ever been.
///
/// **A list, and not one running coalesced record.** A coalesced record has to
/// be coalesced relative to somebody's last look, and there is more than one
/// somebody: the syntax thread, the after-change hook, and — next — an LSP
/// client, each reading at its own rate and each entitled to a different
/// answer. Core would have to know all of them to keep one record; it needs to
/// know none of them to keep a list. So core appends, a reader names the count
/// it last acted on, and folds what it gets back with [`Change::coalesce`] if
/// one record is what it wanted.
///
/// The log is capped and the count is not. A reader further behind than
/// [`CHANGE_LIMIT`] gets `None` — "reread the whole text" — which is the same
/// answer it gets for a buffer it has never seen, so there is one recovery path
/// to write and one to test rather than two.
#[derive(Default)]
struct Changes {
    count: u64,
    log: Vec<Change>,
}

impl Changes {
    fn record(&mut self, change: Change) {
        self.count += 1;
        self.log.push(change);
        if self.log.len() > CHANGE_LIMIT {
            self.log.remove(0);
        }
    }

    fn since(&self, seen: u64) -> Option<&[Change]> {
        // `seen > count` is a reader holding a number from another buffer's
        // life. Not an error — buffers are switched constantly — but it is
        // never a slice of this log, so it takes the same road as an overflow.
        let behind = self.count.checked_sub(seen)? as usize;
        (behind <= self.log.len()).then(|| &self.log[self.log.len() - behind..])
    }
}

/// Does this buffer wrap its long lines?
///
/// The buffer's own answer when it has one, the editor's otherwise. A free
/// function for `zemacs_render::gutter_on`'s reason, one setting along: three callers ask it — `Editor::visual_lines` so `j` counts screen
/// rows, and the renderer's row count and draw loop — and the last time a rule
/// like this lived in more than one place the two disagreed about every org
/// buffer. It is in core rather than beside `gutter_on` because core is one of
/// the three.
pub fn wraps(buf: &Buffer, set: &Settings) -> bool {
    buf.line_overflow.unwrap_or(set.line_overflow) == LineOverflow::Wrap
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
    /// The column measure *this buffer* is held to and centred in, or `None` to
    /// follow the editor-wide [`Settings::text_width`].
    ///
    /// [`Buffer::line_numbers`]' argument, one setting along, and it arrived the
    /// same way: `org-mode` claims 80 columns, a terminal claims none, and with
    /// one measure in the editor the pane that won was whichever mode had been
    /// entered last. Focusing a shell in a split therefore un-centred the org
    /// document beside it — the text did not move, the *measure* did, in a pane
    /// nobody had touched.
    ///
    /// Stamped by [`EditorCommand::SetTextWidth`] rather than recomputed from a
    /// table, which is the one difference from the gutter and is what keeps a
    /// second policy list out of core: `runtime/modes/modes.lisp` re-pushes
    /// every claimed setting whenever the modes change, and it does that *for
    /// the buffer that just entered one*. So the value arriving here is always
    /// about this buffer, and the editor-wide setting stays what it always was —
    /// the baseline a buffer nobody has claimed anything about is drawn at.
    pub text_width: Option<usize>,
    /// Whether *this buffer* wraps its long lines, or `None` to follow the
    /// editor-wide [`Settings::line_overflow`].
    ///
    /// [`Buffer::text_width`]' argument, one setting along, and reported the
    /// same way: two frames, one on prose with `visual-line` on and one on a
    /// `.rs`, and clicking into the second un-wrapped the *first* — a window
    /// nobody had touched reflowed because the setting behind it was the
    /// editor's and there is one of it. The same thing happens in a split, and
    /// it is the same bug: `wrap` is a fact about the document, and a pane draws
    /// a document.
    ///
    /// Stamped by [`EditorCommand::SetLineOverflow`], exactly as `text_width`
    /// is: `runtime/modes/modes.lisp` re-resolves every claimed setting on each
    /// buffer switch and pushes it *for the buffer that is now on screen*, so
    /// the value arriving here is always about this buffer. The editor-wide
    /// setting stays what it always was — the baseline for a buffer no mode has
    /// claimed anything about.
    pub line_overflow: Option<LineOverflow>,
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
    /// What has happened to this text, for anything that has to keep a copy of
    /// it in step — the syntax thread's tree, the image, a language server.
    ///
    /// Per buffer and travelling with it, for the same reason the undo history
    /// and the markers do: a change log is a fact about a *document*, and a
    /// reader that had one buffer's edits applied to another's copy would be
    /// worse off than a reader with no log at all.
    changes: Changes,
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
            line_overflow: None,
            text_width: None,
            major_mode: FUNDAMENTAL.into(),
            minor_modes: Vec::new(),
            saved_scroll: 0,
            file_mode: None,
            visited: None,
            highlights: Vec::new(),
            changes: Changes::default(),
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
        // **Applied as the edit it is, not as a whole-document swap.** A
        // snapshot is two ropes and no diff, but the diff between them is one
        // scan from each end: everything an undo did is between the first
        // character that differs and the last, because that is what a snapshot
        // of an *edit* differs by.
        //
        // This used to be a `replace_text` — `0..old_len -> 0..new_len`, with
        // the markers and overlays afterwards *clamped* rather than moved. It
        // was honest and it was wrong in a way that only showed up once
        // something hung off an offset. An overlay does not survive being told
        // the whole document changed: it keeps its absolute position, so
        // undoing an edit *above* a LaTeX preview left the preview where it
        // was while the text under it slid — an image over the wrong equation,
        // and the same for every diagnostic mark, fold and org-modern bullet in
        // the file. Splicing runs them through `adjust`, which is the machinery
        // that has always kept them in place across an ordinary edit.
        //
        // Two more things fall out. The change record is now the *range* that
        // changed, so a `u` on a one-character typo is an incremental reparse
        // and one small `didChange` rather than a full pass over the file —
        // which is the ceiling the note that used to be here named. And the
        // clamping is gone, because a splice cannot leave anything outside the
        // document in the first place.
        let (at, removed, inserted) = Self::diff(&self.text, &snap.text);
        let text: String = snap.text.slice(at..at + inserted).chars().collect();
        self.splice(at, removed, &text);
        // Belt and braces: the scan above is arithmetic and the assertion it
        // rests on — that splicing the difference produces the snapshot — is
        // one every reader below depends on.
        debug_assert_eq!(self.text, snap.text, "undo did not restore the snapshot");
        self.cursor = snap.cursor.min(self.text.len_chars());
        self.modified = true;
        true
    }

    pub fn len_chars(&self) -> usize {
        self.text.len_chars()
    }

    pub fn len_lines(&self) -> usize {
        self.text.len_lines()
    }

    /// The last line the cursor may sit on — vim's count, not the rope's.
    ///
    /// A rope counts the empty string after a trailing newline as a line, and
    /// very nearly every file ends in one. So `len_lines() - 1` names a line
    /// that is not there, and everything reaching for "the last line" landed on
    /// it: `G` put the cursor on a row with no text, `j` walked onto it, `}`
    /// stopped short of the real last paragraph, and `dd` on the actual last
    /// line computed its range against a phantom.
    ///
    /// `"one\ntwo\n"` is two lines here, as it is in vim and in `wc -l`.
    /// `"one\ntwo"` is also two, and the second one is real — which is the
    /// distinction this has to make and the reason it is not simply a subtract.
    pub fn last_line(&self) -> usize {
        let last = self.len_lines().saturating_sub(1);
        let trailing_newline = self.char_at(self.len_chars().saturating_sub(1)) == Some('\n');
        if last > 0 && trailing_newline && self.line_len(last) == 0 {
            last - 1
        } else {
            last
        }
    }

    /// `line`'s leading whitespace, as the string to repeat.
    ///
    /// What `o` and `O` open the new line with: vim's `autoindent`, which evil
    /// leaves on, and without which every `o` in indented code is followed by
    /// retyping the indent you were already at.
    pub fn line_indent(&self, line: usize) -> String {
        let start = self.line_start(line);
        (0..self.line_len(line))
            .map(|i| self.text.char(start + i))
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect()
    }

    /// True when `line` ends with something that opens a block — see
    /// [`Settings::indent_openers`].
    ///
    /// Tested against the line with its trailing whitespace removed, so a `{`
    /// you left a space after still counts, and against a *comment-free* line it
    /// deliberately does not: `if x: # note` is a line that opens a block and a
    /// suffix test says it does not. That is the known cost of not parsing, and
    /// it fails in the direction that leaves you where you were rather than
    /// indenting text you did not mean to indent.
    fn opens_block(&self, line: usize, openers: &[String]) -> bool {
        if openers.is_empty() {
            return false;
        }
        let start = self.line_start(line);
        let text: String = (0..self.line_len(line))
            .map(|i| self.text.char(start + i))
            .collect();
        let text = text.trim_end();
        openers.iter().any(|o| !o.is_empty() && text.ends_with(o.as_str()))
    }

    /// The indentation a line opened *after* `line` should start with: what
    /// `line` had, plus one step when it opened a block.
    ///
    /// One function because the three keys that open a line — `Enter`, `o` and
    /// `O` — have to agree, and they did not: `o` and `O` carried the previous
    /// indent and `Enter` carried nothing, so the same gesture written two ways
    /// produced two different lines.
    pub fn indent_after(&self, line: usize, openers: &[String], tab: usize) -> String {
        let mut indent = self.line_indent(line);
        if self.opens_block(line, openers) {
            // Spaces, because that is what `Tab` inserts here — see
            // `insert_key`. A buffer indented with tabs keeps its tabs (they came
            // from `line_indent`) and gains a step of spaces, which is what any
            // editor without a tabs-versus-spaces setting does.
            indent.push_str(&" ".repeat(tab.max(1)));
        }
        indent
    }

    /// The bracket matching the one at `pos`, or the one just before it.
    ///
    /// Both directions from one function because both callers want both: `%`
    /// jumps from either end of a pair, and the paren highlight has to light up
    /// the partner whether point is on the opener or just past the closer —
    /// which is where point sits the instant you finish typing one.
    ///
    /// ponytail: brackets in strings and comments count. Skipping them needs the
    /// syntax spans, which arrive on a different thread and a frame late, so a
    /// matcher that consulted them would answer differently depending on when it
    /// was asked. Nesting is honoured, which is what makes it useful at all;
    /// `"("` in a string is the known wrong answer.
    pub fn matching_bracket(&self, pos: usize) -> Option<usize> {
        const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];
        // On a bracket, or — for the closer only — just past one. Point after a
        // freshly typed `)` is the case the highlight exists for.
        let at = |i: usize| self.char_at(i);
        let (from, c) = match at(pos).filter(|c| PAIRS.iter().any(|(o, k)| c == o || c == k)) {
            Some(c) => (pos, c),
            None => {
                let back = pos.checked_sub(1)?;
                let c = at(back).filter(|c| PAIRS.iter().any(|(_, k)| c == k))?;
                (back, c)
            }
        };
        let (open, close, forward) = match PAIRS.iter().find(|(o, _)| *o == c) {
            Some(&(o, k)) => (o, k, true),
            None => {
                let &(o, k) = PAIRS.iter().find(|(_, k)| *k == c)?;
                (o, k, false)
            }
        };
        let mut depth = 0i32;
        let n = self.len_chars();
        let mut i = from;
        loop {
            match at(i) {
                Some(ch) if ch == open => depth += 1,
                Some(ch) if ch == close => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                return Some(i);
            }
            i = match forward {
                true if i + 1 < n => i + 1,
                false if i > 0 => i - 1,
                _ => return None,
            };
        }
    }

    /// The zero-based line `pos` falls on, clamped to the document.
    ///
    /// The clamp is the whole reason this is a method: `Rope::char_to_line`
    /// *panics* one past the end, and the offsets asking come from arithmetic in
    /// Lisp, from a motion target computed against a buffer an edit has since
    /// shortened, and from a marker restored across an undo. A dozen callers
    /// spelled the guard out for themselves, which is a dozen chances to be the
    /// one that did not.
    pub(crate) fn line_of(&self, pos: usize) -> usize {
        self.text.char_to_line(pos.min(self.len_chars()))
    }

    /// (line, column) of the cursor, both zero-based.
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let line = self.line_of(self.cursor);
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

    /// The smallest splice that turns `old` into `new`: `(start, removed, the
    /// text to put there)`, in **characters**, since that is what `splice`
    /// counts in.
    ///
    /// Not a diff — a diff would find the several edits an agent actually made
    /// and this finds the one span containing all of them. That is the right
    /// trade here: it is two linear walks with no allocation and no table, it is
    /// exact when the change is contiguous (which a formatter's or an agent's
    /// usually is), and when it is not, the cost is overlays inside the span
    /// between the first and last change — never overlays outside it, which is
    /// the property that matters.
    ///
    /// ponytail: no Myers diff. Ceiling: two edits at opposite ends of a file
    /// widen the splice to the whole file and drop the folds in between. The
    /// upgrade path is a real diff behind this same signature, so nothing above
    /// changes.
    fn narrow<'a>(old: &str, new: &'a str) -> (usize, usize, &'a str) {
        let mut pre = 0;
        let mut a = old.chars();
        let mut b = new.chars();
        // Byte offset into `new` of the first differing character, tracked
        // alongside the char count because slicing a `str` needs bytes.
        let mut pre_bytes = 0;
        loop {
            match (a.next(), b.next()) {
                (Some(x), Some(y)) if x == y => {
                    pre += 1;
                    pre_bytes += x.len_utf8();
                }
                _ => break,
            }
        }
        let old_len = old.chars().count();
        let new_len = new.chars().count();
        // Never past the prefix: with `old = "aa"` and `new = "aaa"` the naive
        // suffix walk would claim both `a`s twice over and describe a negative
        // range.
        let room = (old_len - pre).min(new_len - pre);
        let mut suf = 0;
        let mut suf_bytes = 0;
        let mut a = old.chars().rev();
        let mut b = new.chars().rev();
        while suf < room {
            match (a.next(), b.next()) {
                (Some(x), Some(y)) if x == y => {
                    suf += 1;
                    suf_bytes += x.len_utf8();
                }
                _ => break,
            }
        }
        (pre, old_len - pre - suf, &new[pre_bytes..new.len() - suf_bytes])
    }

    /// Replace `removed` characters at `start` with `text` — the only place the
    /// rope is edited, so it is also the only place markers have to be moved.
    ///
    /// Every editing operation is a splice, and funnelling them here rather than
    /// adjusting at each call site is the point: a mutation path that forgets to
    /// adjust is exactly the bug markers exist to prevent, and one that forgets
    /// to *splice* does not compile.
    /// What one has to be spliced with to become the other: `(at, removed,
    /// inserted)`, all in characters.
    ///
    /// The common prefix and the common suffix, and everything between them.
    /// That is not a general diff and does not try to be — it is exactly what
    /// an *undo* needs, because the two ropes it compares differ by the one
    /// edit that was undone, and a single edit is by construction one
    /// contiguous range. A cleverer diff would find the same range and cost
    /// more to do it.
    ///
    /// Iterators from both ends rather than `Rope::char(i)`: random access into
    /// a rope is logarithmic per character, and a prefix scan over an edit near
    /// the end of a large file would walk most of it that way.
    ///
    /// The two runs are not allowed to overlap, which is the whole of the
    /// correctness argument for `aaa` -> `aa`: the prefix would happily claim
    /// all of `aa` and the suffix would claim it again, and the result would be
    /// a removal of minus one.
    fn diff(old: &Rope, new: &Rope) -> (usize, usize, usize) {
        let (n, m) = (old.len_chars(), new.len_chars());
        let shortest = n.min(m);
        let mut at = 0;
        let (mut a, mut b) = (old.chars(), new.chars());
        while at < shortest && a.next() == b.next() {
            at += 1;
        }
        let mut tail = 0;
        let (mut a, mut b) = (old.chars_at(n), new.chars_at(m));
        while tail < shortest - at && a.prev() == b.prev() {
            tail += 1;
        }
        (at, n - at - tail, m - at - tail)
    }

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
        // Recorded here for exactly the reason the markers are adjusted here:
        // this is the only place the rope moves under an edit, so it is the
        // only place a record can be produced that cannot be forgotten. A
        // no-op splice is still recorded — `(insert "")` moves the revision,
        // and a log that disagreed with the revision counter would be a second
        // thing to reason about.
        self.changes.record(Change {
            start,
            old_end: start + removed,
            new_end: start + inserted,
        });
        self.modified = true;
    }

    /// Swap the whole text out, recording the whole-document replacement it is.
    ///
    /// The callers — undo/redo, and [`Buffer::adopt`] for everything else — are
    /// not *edits*: each throws the undo history, the markers and the overlays
    /// away and starts a document over, so there is nothing for `splice` to
    /// adjust and no reason to pay for its rope surgery. The change log has to
    /// hear about it all the same. A reader that missed it would go on
    /// adjusting offsets into a document that is gone, which is worse than
    /// being told to start again.
    fn replace_text(&mut self, text: Rope) {
        self.changes.record(Change {
            start: 0,
            old_end: self.text.len_chars(),
            new_end: text.len_chars(),
        });
        self.text = text;
    }

    /// Take `text` as a **different document**, dropping everything that
    /// described the old one.
    ///
    /// The shared half of the three routes to a buffer holding something else —
    /// `load` reading a file, `create_buffer` making one from Lisp, and
    /// `show_named` re-rendering a generated listing — which had three copies
    /// of this list and were one forgotten `markers.clear()` away from a marker
    /// naming an offset in text that no longer exists. The undo history goes
    /// for the same reason: one `u` must never restore a document you are not
    /// looking at.
    ///
    /// What stays with the caller is what genuinely differs between the three:
    /// where point lands afterwards, and whether a *scene* went with the
    /// document — a page is only stale where a buffer was reused rather than
    /// stacked, so the two callers that reuse one say so themselves.
    fn adopt(&mut self, text: &str) {
        self.replace_text(Rope::from_str(text));
        self.cursor = 0;
        self.modified = false;
        self.undo.clear();
        self.redo.clear();
        self.markers.clear();
        self.overlays.clear();
        self.highlights.clear();
    }

    /// How many edits this buffer has ever seen.
    ///
    /// A reader keeps the number it last acted on and hands it back to
    /// [`Buffer::changes_since`]. A counter rather than a cursor the reader
    /// holds *in* the buffer, because there is more than one reader and core
    /// has no business knowing how many.
    pub fn change_count(&self) -> u64 {
        self.changes.count
    }

    /// Every edit since `seen`, oldest first — or `None`, meaning the log no
    /// longer reaches back that far and the reader should reread the text.
    ///
    /// An empty slice and `None` are different answers and the difference
    /// matters: empty is "the revision moved but the text did not", which
    /// `set-language` and a mode change both produce, and a reader that
    /// resynced on those would reparse the file every time a minor mode was
    /// toggled.
    pub fn changes_since(&self, seen: u64) -> Option<&[Change]> {
        self.changes.since(seen)
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

    /// Put `text` at `at`, leaving point after it — the insert twin of
    /// [`Buffer::delete_range`], and there for the same reason: an edit whose
    /// position is *computed* rather than "wherever the cursor is".
    ///
    /// `o` on a closed fold is the case that needed it. The line it opens after
    /// is the last line the fold hides, and a cursor may not sit on a hidden
    /// line — `clamp_cursor` escapes it back to the fold's head at the end of
    /// every `apply`, so moving there and then inserting put the newline at the
    /// top of the fold instead of below it.
    fn insert_at(&mut self, at: usize, text: &str) {
        let at = at.min(self.len_chars());
        self.splice(at, 0, text);
        self.cursor = (at + text.chars().count()).min(self.len_chars());
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
    /// document. This said "nothing evicts" for a long time and no longer does:
    /// [`Editor::prune_images`] drops every entry no buffer's overlays, no
    /// buffer's scene and not the dashboard still names, and it runs whenever an
    /// edit or an overlay command actually let go of one.
    images: HashMap<ImageId, Image>,
    /// The em, in the device pixels the renderer draws with — `font_size` times
    /// the display's scale factor, which core cannot know and the renderer parks
    /// here once a frame, exactly as it does `viewport_lines`. A LaTeX preview
    /// has to be rasterised at this size or it comes out half-height on a
    /// Retina display.
    pub font_px: f32,
    /// Command names published by the Lisp image, for `M-x` completion.
    pub commands: Vec<String>,
    /// What has been answered to prompts before, oldest first, so `M-p` in a
    /// prompt can walk back through it.
    ///
    /// One flat log tagged by kind rather than a list per kind: walking filters
    /// it, which is a scan of at most [`HISTORY_LIMIT`] entries on a keypress a
    /// human made, and it costs no map, no `Hash` on [`PromptKind`] and no
    /// decision about what to do with a kind nobody has typed into yet.
    pub prompt_history: Vec<(PromptKind, String)>,
    /// Mode hooks waiting to be run. Core records that a hook is due; the app
    /// drains this and asks the Lisp image to run each one, since core cannot
    /// call Lisp itself.
    pub pending_hooks: Vec<String>,
    /// Buffers whose text was replaced under them and which want a fresh parse.
    /// The same division of labour as [`Editor::pending_hooks`] one line up, and
    /// for the same reason: highlighting happens on a thread core does not own,
    /// so core can only record that it is due.
    ///
    /// A *parked* buffer is what this exists for. The live one bumps the
    /// revision and gets re-parsed for that alone; the buffer nobody is looking
    /// at has no such signal and used to sit uncoloured until it was visited.
    pub pending_highlight: Vec<BufferId>,
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
    /// A line a *mode* keeps on the modeline, drawn by `%N`.
    ///
    /// The difference from [`Editor::status`] is how long it is true for.
    /// `status` is the last thing that *happened* and is replaced by the next
    /// thing; this is a standing fact about a mode that is running — how many
    /// files a watcher has seen, that a transcription is in flight — and it stays
    /// until the mode itself takes it down. A mode reporting that through
    /// `message` would either be talking over every other message in the editor
    /// or be silent by the time you looked.
    ///
    /// Empty is the ordinary state, and a `%N` segment vanishes when it is empty
    /// — see [`Fields::expand`], which drops a segment whose every code came back
    /// blank. So a mode that has nothing to say costs no space on the strip.
    pub modeline_note: String,
    /// Every message, oldest first, capped at [`MESSAGE_LIMIT`]. The status line
    /// only ever shows the last one, so this is the only record that a message
    /// which was immediately replaced was ever produced at all.
    pub messages: Vec<String>,
    pub should_quit: bool,

    /// Bumped on every document mutation; the app re-highlights when it moves.
    pub revision: u64,
    /// Bumped whenever *anything the screen is made of* changes — the document,
    /// the cursor, the mode, a prompt, a message, an overlay, the frame layout.
    ///
    /// [`Editor::revision`]'s coarser sibling, and it exists for the app's draw
    /// loop rather than for the highlighter: an editor nobody is touching used
    /// to redraw at the display's refresh rate and throw every frame away when a
    /// digest of the draw calls said it was identical. That is correct and it is
    /// a couple of milliseconds of CPU per display frame spent proving nothing
    /// happened.
    ///
    /// **Conservative on purpose.** Over-counting costs a frame that was going
    /// to be drawn anyway; under-counting leaves a stale screen. So every
    /// [`Editor::apply`] bumps it whether or not the command changed anything,
    /// every keystroke bumps it, and every Lisp primitive that takes the editor
    /// mutably bumps it — which is the one signal that catches the image editing
    /// the buffer through the shared mutex, since that raises no window event.
    ///
    /// Writers outside core call [`Editor::touch`]. The app's draw loop pairs
    /// this with a periodic forced draw, so a writer that forgets costs a
    /// fraction of a second of staleness rather than a screen that never
    /// updates — see `App::draw`.
    pub generation: u64,

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
    /// What the modeline says, as a list of templates the image set.
    ///
    /// Filed beside `which_key` and `dashboard`, which are the other two
    /// surfaces whose *content* is Lisp's and whose drawing is not. It differs
    /// from both in being read every frame for every pane, which is why it is a
    /// format expanded here rather than rows pushed across — see
    /// [`modeline`](crate::modeline).
    pub modeline: modeline::Format,
    /// corfu: candidates for the word being typed, filed here for
    /// [`Completion`]'s reasons and read through [`Editor::completion`] rather
    /// than directly.
    ///
    /// Private, unlike `which_key`, because "is there a popup" and "is there a
    /// popup that still describes where you are typing" are different questions
    /// and only the second one is ever the right one to ask. See
    /// [`Editor::completion`].
    completion: Option<Completion>,
    /// A Lisp function the image parked here to receive the *next* keystroke —
    /// avy. See [`EditorCommand::GrabKey`], which is where the design is.
    ///
    /// Filed beside `ace` and `which_key` for their reason and with one
    /// difference: those two are a *surface* core draws, and this is only the
    /// keyboard half. Nothing reads it but [`Editor::dispatch_key`], which takes
    /// it — so the flag is spent by the keystroke that satisfies it and no path
    /// through the image can leave the keyboard captured.
    pub grab_key: Option<String>,
    /// The right-click menu, if one is up. See [`ContextMenu`].
    pub context_menu: Option<ContextMenu>,
    /// The box under the pointer, if it is resting on something that has
    /// anything to say. See [`Tooltip`].
    pub tooltip: Option<Tooltip>,
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
    /// Which way `n` goes. Set by `/` and `?` and by `*` and `#`: a search
    /// started backwards has to *repeat* backwards, or `?` is not a backward
    /// search at all, only a one-shot jump.
    search_backward: bool,
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
/// How many edits [`Changes`] keeps per buffer before the oldest fall off and a
/// reader that far behind is told to reread the text instead.
///
/// Sized for the gap between a reader and the keyboard, not for a session: the
/// app looks once a frame and the image is a queue turn behind that, so a
/// hundred is already two seconds of very fast typing. Every reader has a
/// resync path — it is the same path a buffer switch takes — so overflowing is
/// slow rather than wrong, and the alternative, an unbounded log, is a leak in
/// a long editing session with nothing draining it.
const CHANGE_LIMIT: usize = 256;
/// How many messages [`Editor::messages`] keeps before dropping the oldest.
pub const MESSAGE_LIMIT: usize = 500;

/// How many prompt answers are kept. Emacs' own default for a history ring is
/// 100 and nobody scrolls past the last few; the cap exists so a long session
/// does not grow a list forever, not because the tail is precious.
pub const HISTORY_LIMIT: usize = 100;

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
            prompt_history: Vec::new(),
            pending_hooks: Vec::new(),
            pending_highlight: Vec::new(),
            mode: Mode::Dashboard,
            settings: Settings::default(),
            theme: Theme::default(),
            dashboard: Dashboard::default(),
            keymap: HashMap::new(),
            mode_keymap: HashMap::new(),
            no_gutter_modes: Vec::new(),
            prompt: None,
            status: String::from("zemacs — Common Lisp inside."),
            modeline_note: String::new(),
            messages: Vec::new(),
            should_quit: false,
            revision: 0,
            generation: 0,
            scroll: 0,
            viewport_lines: 24,
            wrap_cols: 0,
            pending: evil::Pending::default(),
            visual_anchor: None,
            ace: None,
            which_key: Vec::new(), // which-key panel
            // Almost nothing until `runtime/init.lisp` says otherwise — see
            // `modeline::Format::default`.
            modeline: modeline::Format::default(),
            completion: None,      // corfu
            grab_key: None,        // avy
            context_menu: None,
            tooltip: None,
            desired_col: None,
            register: String::new(),
            register_linewise: false,
            register_revision: 0,
            last_search: String::new(),
            search_backward: false,
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
                    self.stack_buffer();
                    // A new, empty buffer starts at the top. `switch_buffer` in
                    // the arm above restores the incoming buffer's own scroll;
                    // this arm had no incoming buffer and so kept the *outgoing*
                    // one's, which is how a terminal opened from line 400 of a
                    // file got a window scrolled 400 lines into a blank grid.
                    // `create_buffer` has always said this; this is the same
                    // line.
                    self.scroll = 0;
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
        // Regenerated text, so every marker into the old listing names a line
        // that may not even be there any more. Overlays go the same way: dired
        // and magit put their faces back on every refresh anyway — see
        // [`Buffer::adopt`], which is where the rest of that list lives.
        self.adopt_text(text);
        self.buffer.move_to_line_col(line, 0);
        self.mode = kind.mode();
        self.revision += 1;
        // The scroll goes back onto the window too, and not only the buffer and
        // the cursor: the sync at the top of this function parked the *outgoing*
        // buffer's scroll there, and the window is what the renderer draws every
        // pane from.
        self.sync_window();
        self.buffer.id
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
        // Only what actually *changed*, and this is the whole of why folds
        // survive an agent now.
        //
        // This used to be `splice(0, end, text)` — replace the document with the
        // document. `Overlays::adjust` collapses every overlay an edit swallowed
        // and drops the empty ones, quite rightly, so a whole-buffer splice
        // dropped **all** of them: every fold, every org-modern glyph, every
        // LaTeX preview, on every write by a coding agent. The buffer came back
        // correct and completely unfolded, which is not what "the file changed
        // underneath you" should mean.
        //
        // Trimming the common prefix and suffix costs two walks and turns the
        // usual case — an agent rewriting four lines of a five-hundred-line file
        // — into a four-line splice that the existing adjustment machinery
        // handles correctly and has always handled correctly. Overlays outside
        // the edit are untouched because they are outside the edit. Nothing here
        // is special-cased for folds; they were never the exception, the maximal
        // splice was.
        let old = buffer.slice_string(0, end);
        let (start, removed, replacement) = Buffer::narrow(&old, text);
        if removed == 0 && replacement.is_empty() {
            // `touch` on an unchanged file, or a rewrite that put back exactly
            // what was there. Doing nothing is not an optimisation, it is the
            // correct answer: a revision bump would re-parse and re-render the
            // buffer to arrive at the pixels already on screen.
            return true;
        }
        buffer.splice(start, removed, replacement);
        buffer.move_to_line_col(line, col);
        // `splice` sets it and is wrong to: the buffer now holds exactly what is
        // on disk, which is the definition of unmodified.
        buffer.modified = false;
        // Spans describe text that is gone, so they go — but clearing them was
        // only ever half an answer. The live buffer gets a fresh parse from the
        // revision bump below; a parked one got *nothing*, which is how a file
        // rewritten under the editor by an agent came back correct and
        // completely colourless and stayed that way until you switched to it
        // and typed. So the buffer is named as wanting a parse rather than left
        // uncoloured, and the app hands the name to the syntax thread on its
        // next pass.
        buffer.highlights.clear();
        let dropped = buffer.overlays.take_dropped();
        self.pending_highlight.push(id);
        // Asked here as well as in [`Editor::apply`], because an agent rewriting
        // a file can land on the lines a preview covers, and this buffer is very
        // often *not* the live one — the sweep at the end of `apply` would never
        // come to it.
        if dropped {
            self.prune_images();
        }
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

    /// Every open buffer, **live one first**.
    ///
    /// That order is not an implementation detail: it is what `(buffer-list)`
    /// and `(buffer-info)` promise, what the switcher's indices are into — index
    /// 0 is the current buffer — and why `switch-to-buffer` can find one by
    /// name. Half a dozen readers used to spell the chain out for themselves;
    /// an invariant this many things depend on is worth exactly one statement.
    ///
    /// `pub` rather than `pub(crate)` because the app wanted the same walk and,
    /// being unable to reach this one, wrote it again — which is how its
    /// `autosave_all` came to visit the live buffer by hand while the revert
    /// sweep used an iterator. The live-first order is a promise made to Lisp;
    /// it should not be re-derived by anyone.
    pub fn buffers(&self) -> impl Iterator<Item = &Buffer> {
        std::iter::once(&self.buffer).chain(self.others.iter())
    }

    fn dashboard_buffer_id(&self) -> BufferId {
        self.buffers()
            .find(|b| b.kind == BufferKind::Dashboard)
            .map(|b| b.id)
            .unwrap_or(0)
    }

    /// Any buffer by handle — the renderer needs this to draw the *inactive*
    /// windows, whose buffers are not `self.buffer`.
    pub fn buffer_by_id(&self, id: BufferId) -> Option<&Buffer> {
        self.buffers().find(|b| b.id == id)
    }

    /// The same buffer, to write to — what auto-revert and auto-save reach for,
    /// since both act on a buffer that is not the one being typed into.
    ///
    /// Spelled out rather than built on a `buffers_mut`, because that iterator
    /// has exactly this one caller and an iterator with one caller is a worse
    /// abstraction than the chain it hides.
    pub fn buffer_by_id_mut(&mut self, id: BufferId) -> Option<&mut Buffer> {
        std::iter::once(&mut self.buffer)
            .chain(self.others.iter_mut())
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

    /// Park the live buffer, cursor and scroll on the focused window. Must run
    /// before anything changes which window is focused — and again after
    /// anything changes which *buffer* is live, since the window is what the
    /// renderer draws every pane from and one still naming the buffer that left
    /// would swap it straight back in on the next focus change.
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

    /// Park the live buffer on the stack and make a fresh, empty one live.
    ///
    /// The first half of every route to "a different document is on screen" —
    /// `load`, `create_buffer`, `show_named` — which had three copies of it and
    /// were the three places a forgotten `saved_scroll` or a reused
    /// `next_buffer_id` would show up as one buffer wearing another's identity.
    ///
    /// *Whether* to stack stays with the caller, because that is where the three
    /// genuinely differ: `load` and `create_buffer` reuse a pristine buffer
    /// rather than leaving an untitled husk in the switcher, and a generated
    /// buffer always gets its own.
    fn stack_buffer(&mut self) {
        self.buffer.saved_scroll = self.scroll;
        let mut fresh = Buffer::from_str("");
        fresh.id = self.next_buffer_id;
        self.next_buffer_id += 1;
        let previous = std::mem::replace(&mut self.buffer, fresh);
        self.others.insert(0, previous);
    }

    /// Tell Lisp that a *different buffer* is now on screen.
    ///
    /// The one report the boundary was missing. Core has always said "this
    /// buffer entered mode X" and never "you are looking at a different
    /// buffer", and the settings a mode claims — wrapping, the text measure,
    /// the tab stop — are **global in the editor**, one of each, so Lisp
    /// resolves them from the last mode it saw *entered*. Without this line,
    /// opening one `.rs` file costs every org and prose buffer its wrapping and
    /// its 80-column measure for the rest of the session, because nothing ever
    /// tells the image that the org buffer came back.
    ///
    /// Queued rather than called, like every other hook: core does not call
    /// Lisp. The name is dispatched through `pending_hooks` behind the same
    /// `fboundp` guard, so an image that never loaded `runtime/modes/modes.lisp`
    /// pays nothing.
    ///
    /// **`buffer-switch-hook` owns re-resolving a mode's claims for a switch;
    /// `%enter-major-mode` owns them for an entry.** The two never fire for the
    /// same event — every route that swaps the live buffer comes through here
    /// and queues no mode hook (`load` and `create_buffer` return the moment
    /// they find the buffer already open), and every route that changes a
    /// buffer's mode queues `X-hook` and swaps nothing. Where a caller does both
    /// — a terminal buffer being shown and given `terminal-mode` in one frame —
    /// the two agree, because each reads the mode from the buffer rather than
    /// remembering one.
    fn announce_buffer_switch(&mut self) {
        self.pending_hooks.push("buffer-switch-hook".into());
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
                // Focus moving between two panes changes the buffer on screen
                // exactly as the switcher does, and one pane on an org file
                // beside another on a `.rs` is the shape that made this bug
                // impossible to miss.
                self.announce_buffer_switch();
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
        self.buffers().map(|b| b.id).collect()
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
        self.buffers()
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
        self.buffers()
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
        self.sync_window();
        // The chokepoint: `switch_buffer_id`, `create_buffer`, `show_named` and
        // `load` on an already-open file all reach a different document through
        // here, so this is the one place the report has to be made.
        self.announce_buffer_switch();
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
            self.stack_buffer();
        }
        self.buffer.given_name = Some(name);
        self.buffer.kind = BufferKind::Text;
        self.buffer.path = None;
        self.adopt_text("");
        // A scene is about a document too, and this path *reuses* the live
        // buffer when it was pristine — so a page left over from the buffer
        // that was here would be drawn over a scratchpad that has nothing to do
        // with it, and would keep the read-only claim that came with it.
        self.buffer.set_scene(None);
        // No hook: `fundamental-mode-hook` firing on every scratchpad would be a
        // surprise, and Lisp that wants one calls `set-major-mode` itself — it
        // is one line, and it is the line that says which mode it meant.
        self.buffer.major_mode = FUNDAMENTAL.into();
        self.buffer.minor_modes.clear();
        self.scroll = 0;
        self.revision += 1;
        self.mode = Mode::Normal;
        self.sync_window();
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

    /// One edit to the live document, and the revision it owes.
    ///
    /// The six mutating arms of [`Editor::apply`] each used to bump the counter
    /// for themselves, which is six chances to add a seventh and forget — and a
    /// mutation the revision does not see is a buffer the syntax thread never
    /// re-parses and an `after-change-hook` that never fires. The bump belongs
    /// to *being* an edit, so it is stated where that is decided.
    fn edit(&mut self, f: impl FnOnce(&mut Buffer)) {
        f(&mut self.buffer);
        self.revision += 1;
    }

    /// Say that something the screen is made of has changed, for a writer
    /// outside core — the app adopting highlight spans, a terminal grid moving,
    /// a picker being refilled from the filesystem. See [`Editor::generation`].
    ///
    /// Free to call when nothing changed. Costing a frame that was going to be
    /// drawn anyway is the *cheap* mistake here; the expensive one is a screen
    /// that stops updating.
    pub fn touch(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// The one and only document mutator.
    pub fn apply(&mut self, cmd: EditorCommand) {
        // Before the read-only refusal below and before every early return in
        // the match: a command that was *refused* still put a message on the
        // status line, and a command that did nothing at all costs one frame
        // rather than a wrong one. See [`Editor::generation`].
        self.touch();
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
            EditorCommand::InsertChar(c) => self.edit(|b| b.insert_char(c)),
            EditorCommand::InsertText(s) => self.edit(|b| b.insert_text(&s)),
            EditorCommand::InsertNewline => self.edit(|b| b.insert_char('\n')),
            EditorCommand::DeleteBackward => self.edit(Buffer::delete_backward),
            EditorCommand::DeleteForward => self.edit(Buffer::delete_forward),
            EditorCommand::DeleteRange(a, b) => self.edit(|buf| buf.delete_range(a, b)),
            EditorCommand::InsertAt(at, text) => self.edit(|buf| buf.insert_at(at, &text)),
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
            EditorCommand::SetModelineNote(note) => self.modeline_note = note,
            EditorCommand::Message(m) => {
                // The status line shows one message; the log keeps the rest.
                // Without it a burst — a config load, a command that reports
                // twice — is indistinguishable from its last line, and there is
                // nothing to look at afterwards. This is what `*Messages*` is.
                //
                // The empty one is how Esc and a cancelled ace *clear* the echo
                // area, which is not something that happened — so it takes the
                // status line down without leaving a blank row in the log.
                if !m.is_empty() {
                    if self.messages.len() == MESSAGE_LIMIT {
                        self.messages.remove(0);
                    }
                    self.messages.push(m.clone());
                }
                self.status = m;
            }
            EditorCommand::Quit => self.should_quit = true,

            EditorCommand::SetFontSize(s) => self.settings.font_size = s.clamp(4.0, 400.0),
            // Not checked for existence here: core does no IO, so a path that
            // is not a font is the renderer's to refuse — and it refuses by
            // keeping the face it already has, which is the only failure mode
            // that leaves an editor you can still read.
            EditorCommand::SetFontPath(p) => self.settings.font_path = p,
            EditorCommand::SetBackground(c) => self.settings.background = clamp3(c),
            EditorCommand::SetForeground(c) => {
                self.settings.foreground = clamp3(c);
                self.theme.set(HlKind::Default, c);
            }
            EditorCommand::SetSyntaxColor(name, rgb) => match HlKind::from_name(&name) {
                Some(k) => self.theme.set(k, rgb),
                None => self.status = format!("unknown syntax face: {name}"),
            },
            // The same complaint, word for word, as the colour setter's: a
            // misspelled face is one mistake, and which of the two primitives
            // happened to catch it is not the reader's problem.
            EditorCommand::SetFaceStyle(name, bold, italic) => match HlKind::from_name(&name) {
                Some(k) => self.theme.set_style(k, FaceStyle { bold, italic }),
                None => self.status = format!("unknown syntax face: {name}"),
            },
            // The pane's own background and foreground are *settings*, not
            // faces, and are deliberately left alone: a theme sets both on its
            // first two lines, and clearing them here would flash the previous
            // ground for the length of one `load'.
            EditorCommand::ResetFaces => self.theme.reset(),
            EditorCommand::SetLineNumbers(on) => self.settings.line_numbers = on,
            EditorCommand::SetTabWidth(n) => self.settings.tab_width = n.clamp(1, 16),
            // Space-separated, because an opener is a token like `:` or `{` or
            // `do` and none of them contains a space. One string is what the
            // write envelope carries; splitting it here is what keeps this off
            // the list of things that needed a C primitive.
            EditorCommand::SetIndentOpeners(s) => {
                self.settings.indent_openers =
                    s.split_whitespace().map(str::to_string).collect();
            }
            // Not clamped at the low end past zero, which is the "off" value:
            // a measure of one or two columns is silly rather than dangerous,
            // and the renderer never lets the inset exceed the pane anyway.
            // Both, and the pair is the whole fix for a split losing its
            // centring — see [`Buffer::text_width`]. The editor-wide value is
            // the baseline for a buffer no mode has spoken for; the stamp is
            // what makes the pane beside a terminal keep its own measure.
            EditorCommand::SetTextWidth(n) => {
                let n = n.min(1000);
                self.settings.text_width = n;
                self.buffer.text_width = Some(n);
            }
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
            EditorCommand::SetScrollPastEnd(on) => self.settings.scroll_past_end = on,
            EditorCommand::SetLineOverflow(name) => match LineOverflow::from_name(&name) {
                Some(o) => {
                    self.settings.line_overflow = o;
                    self.buffer.line_overflow = Some(o);
                }
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
            EditorCommand::ZoomWindow(by) => {
                let w = self.frame_mut().current_window_mut();
                w.zoom = match by {
                    0 => 100,
                    by => frame::zoom_step(w.zoom, by),
                };
                let at = w.zoom;
                self.status = format!("window zoom {at}%");
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
            EditorCommand::AddDashboardItem {
                key,
                label,
                action,
                hint,
            } => {
                self.dashboard.items.push(dashboard::Item {
                    key,
                    label,
                    action,
                    hint,
                });
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
            // No sweep here. Whether this edit let go of an `ImageId` is a
            // question only the overlay itself can answer — see
            // [`overlay::Overlays::edit`], which asks it on the way past and
            // arms the same `dropped` flag an edit to the *text* arms. The one
            // drain at the end of `apply` turns either into a prune.
            //
            // It used to sweep inline, unconditionally, on every delete: an
            // image nobody points at is a few hundred KB and a texture the
            // renderer will never draw again, so the sweep has to happen — but
            // a delete of an overlay carrying no image cannot orphan one, and
            // paying a walk over every buffer to discover that is what made
            // clearing a mode's overlays quadratic in the whole editor.
            EditorCommand::Overlay(edit) => self.buffer.overlays.edit(edit),

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
                previewing,
            } => {
                let kind = PromptKind::Lisp {
                    id,
                    completing,
                    previewing,
                };
                let mut prompt = Prompt::new(kind, &label, Vec::new());
                // Still nothing for *core* to restore on Escape, even when this
                // one previews: what a Lisp preview disturbed is not a cursor
                // and not a buffer, and the callback puts it back on the NIL.
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
            // Gated the same way and for the same reason. A trailing newline is
            // how `~{~a~^~%~}` prints an empty list and how a text file ends, so
            // the empty tail it leaves is dropped rather than offered as a
            // candidate with no name.
            EditorCommand::PromptItems(text) => {
                if let Some(p) = self.prompt.as_mut() {
                    if matches!(p.kind, PromptKind::Lisp { .. }) {
                        p.extend_items(text.lines().map(str::to_string));
                    }
                }
            }
            // Not gated: seeding is asked for *by* the code that opened the
            // prompt, one command earlier in the same batch, and the prompt it
            // means may be any kind — `project-open` seeds the app's file
            // picker, which is the case the verb exists for.
            EditorCommand::SetPromptText(text) => {
                if let Some(p) = self.prompt.as_mut() {
                    p.text = text;
                    p.refilter();
                }
            }
            EditorCommand::SetPromptLabel(label) => {
                if let Some(p) = self.prompt.as_mut() {
                    p.label = label;
                }
            }
            EditorCommand::WhichKey(row) => match row {
                Some(r) => self.which_key.push(r),
                None => self.which_key.clear(),
            },
            EditorCommand::Modeline(seg) => match seg {
                Some((right, spec)) => self.modeline.push(right, spec),
                None => self.modeline.clear(),
            },
            // avy. A plain assignment and not a push: there is one next
            // keystroke, so a second `GrabKey` before the first is spent is a
            // change of mind rather than a queue.
            EditorCommand::GrabKey(f) => self.grab_key = f,
            EditorCommand::Completion(edit) => match edit {
                CompletionEdit::Show(None) => self.completion = None,
                CompletionEdit::Show(Some((at, selected))) => {
                    // Kept only if the anchor is the same one: see
                    // [`CompletionEdit::Show`]. Clamped to the document because
                    // the number came out of the image, and an anchor past the
                    // end would be a range `accept` could not replace.
                    let at = at.min(self.buffer.len_chars());
                    let mut c = self
                        .completion
                        .take()
                        .filter(|c| c.at == at)
                        .unwrap_or_default();
                    c.at = at;
                    c.selected = selected.min(c.rows.len().saturating_sub(1));
                    self.completion = Some(c);
                }
                CompletionEdit::Row(row) => {
                    if let Some(c) = self.completion.as_mut() {
                        match row {
                            Some(r) => c.rows.push(r),
                            None => {
                                c.rows.clear();
                                c.selected = 0;
                            }
                        }
                    }
                }
                CompletionEdit::Doc(line) => {
                    if let Some(c) = self.completion.as_mut() {
                        match line {
                            Some(l) => c.doc.push(l),
                            None => c.doc.clear(),
                        }
                        // Stale the moment the text changes, and re-derived by
                        // the app from `doc` — see [`Completion::doc_spans`].
                        c.doc_spans = None;
                    }
                }
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
            // Silent, unlike its neighbours: a prompt with no candidates is a
            // prompt you can still type an answer into, which is what a
            // headless core should do with one. Saying "no backend" would put
            // an error over a picker that is working.
            EditorCommand::PromptSource(_) => {}
            EditorCommand::ListFonts => self.status = "no font backend".into(),
            // Headless, there is no pasteboard to read. Same shape as the line
            // above: core answers the request by saying it cannot, rather than
            // leaving the caller waiting for a reply that never comes.
            EditorCommand::ClipboardImage => self.status = "no clipboard here".into(),
            // Only reachable with no app under core — a keystroke aimed at a
            // shell that is not there is nothing, not an error worth reporting
            // on every key.
            EditorCommand::TermKey(_) => {}
            EditorCommand::OpenFile(p) => self.status = format!("cannot open {}", p.display()),
            EditorCommand::OpenAt(hit) => self.status = format!("cannot open {hit}"),
            EditorCommand::SaveFile(_) => self.status = "cannot save: no file backend".into(),
            EditorCommand::RevertBuffer => {
                self.status = "cannot revert: no file backend".into()
            }
            EditorCommand::Confirmed(_) => {
                self.status = "cannot confirm: no file backend".into()
            }
        }
        if self.revision != before && !history {
            self.buffer.redo.clear();
        }
        // An ordinary edit drops overlays too: select the text under a LaTeX
        // preview, delete it, and the overlay that named the bitmap collapses
        // inside `splice` with nobody above it any the wiser. Asked once here,
        // for every command rather than the editing ones, because taking a
        // `bool` is free and remembering which arms edit is exactly the mistake
        // that left this hole. See [`Editor::prune_images`].
        if self.buffer.overlays.take_dropped() {
            self.prune_images();
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
            self.stack_buffer();
        }
        // A new document gets a new history — and note the outgoing buffer took
        // its own undo stack with it, so one `u` here can never restore the
        // file you were looking at a moment ago. Its markers went the same way,
        // and any left are stale by the same argument. See [`Buffer::adopt`].
        self.adopt_text(text);
        self.buffer.path = path;
        self.buffer.language = language;
        // ...and the scene, for the reason `create_buffer` clears it: a
        // pristine buffer is reused rather than stacked, and the page that was
        // on it is about the document that just left.
        self.buffer.set_scene(None);
        self.buffer.kind = BufferKind::Text;
        // The major mode follows the file, and its hook fires so `init.lisp`
        // can react — `(defun org-mode-hook () ...)` is the whole extension
        // point, exactly as in Emacs.
        let major = major_mode_for(self.buffer.language.as_deref());
        // ...and the gutter follows the mode, here as it does in
        // `EditorCommand::SetMajorMode`. It did not, and that was a real bug
        // with a confusing shape: `set-no-gutter-modes` decides for the buffers
        // that exist *when it runs*, and `SetMajorMode` decides for a mode set by
        // hand — but a file opened from the prompt reaches its mode through this
        // function and through neither of those, so it kept whatever flag the
        // reused buffer happened to carry. The symptom was org files showing
        // line numbers the shipped config switches off, and `M-x org-mode` on the
        // same buffer then fixing it, which reads as anything but a missing line.
        //
        // Computed before the field writes because it borrows `self`.
        let gutter = self.gutter_for_mode(&major);
        self.buffer.major_mode = major.clone();
        self.buffer.line_numbers = Some(gutter);
        self.buffer.minor_modes.clear();
        self.pending_hooks.push(format!("{major}-hook"));
        self.revision += 1;
        self.scroll = 0;
        self.mode = Mode::Normal;
        self.sync_window();
    }

    fn set_mode(&mut self, m: Mode) {
        match m {
            // Switching *between* visual modes keeps the anchor, so `v` then
            // `C-v` reshapes the same selection rather than restarting it.
            m if m.is_visual() && !self.mode.is_visual() => {
                self.visual_anchor = Some(self.buffer.cursor);
            }
            // Leaving visual: keep what was selected, so `gv` can put it back.
            // Remembered on the way out rather than as it is dragged, because
            // "the last selection" means the one you finished with.
            Mode::Normal | Mode::Insert | Mode::Dashboard => {
                if self.mode.is_visual() {
                    self.vim.last_selection = self.selection();
                }
                self.visual_anchor = None;
            }
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
            let first = self.buffer.line_of(a);
            let last = self.buffer.line_of(b);
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

    // --- corfu ------------------------------------------------------------

    /// The in-buffer completion popup, but only while it still describes where
    /// you are typing: Insert mode, and point at or after the anchor on the
    /// anchor's own line.
    ///
    /// **A predicate rather than a `clear` somewhere, and the difference is
    /// timing.** which-key is retired by a question asked in `handle_key` —
    /// "is anything still pending?" — because what makes it stale is core's own
    /// [`evil::Pending`], which `dispatch_key` has already updated by then. What
    /// makes *this* stale is the mode and the cursor, and those live in
    /// [`EditorCommand`]s that `handle_key` has produced and nobody has applied
    /// yet. A check there would be a frame behind on every key that matters —
    /// the `Esc` leaving Insert, the backspace off the front of the word — and
    /// a popup that outlives the word it is about by one keystroke is a popup
    /// you accept the wrong candidate from.
    ///
    /// Asked where the editor is *read*, it is exact: everything has landed by
    /// then. It is also one site, which is the property `handle_key` was
    /// protecting — there is no list of "commands that retire the popup" to keep
    /// in step with the grammar.
    ///
    /// [`Editor::retire_completion`] is the other half, and only the other half:
    /// it stops a hidden popup coming *back*.
    pub fn completion(&self) -> Option<&Completion> {
        let c = self.completion.as_ref()?;
        let cursor = self.buffer.cursor;
        let live = self.mode == Mode::Insert
            && cursor >= c.at
            // Same line. A prefix cannot span one, so this is both "point is
            // still in the word" and "the popup is beside the word" at once —
            // and it is what makes `j` take the popup down without core having
            // to know that `j` moves the cursor.
            && line_col_of(&self.buffer, c.at).0 == line_col_of(&self.buffer, cursor).0;
        live.then_some(c)
    }

    // --- the right-click menu ----------------------------------------------
    //
    // Called by the app rather than driven by an `EditorCommand`, the way
    // `Frame::divider_at` and `adopt_window` are: the whole gesture is a
    // pointer position and a hit test, and neither is a thing a keybinding or
    // the image can produce. What a picked entry *does* goes back through
    // `run_action`, which is the door everything else uses.

    /// Put the menu under the pointer.
    ///
    /// The window verbs are the whole of it everywhere except a listing, which
    /// gets **New File** on top: `C-c n` is the one dired gesture with no key a
    /// hand on the mouse would guess at, and a menu is where you look for it.
    /// Same verb the binding runs, so it prompts for a name and creates it in
    /// the directory on screen.
    pub fn open_context_menu(&mut self, x: i32, y: i32) {
        // The two surfaces anchored to the pointer cannot both be under it. Here
        // rather than at the call site so every door into the menu closes the
        // box, which is the same argument `pick_context_menu` makes for taking
        // the menu itself down in exactly one place.
        self.tooltip = None;
        let mut menu = ContextMenu { x, y, ..Default::default() };
        if self.mode == Mode::Dired {
            menu.items.insert(0, ("New File", "dired-create-file"));
        }
        self.context_menu = Some(menu);
    }

    pub fn close_context_menu(&mut self) {
        self.context_menu = None;
    }

    /// The verb on row `i`, and the menu taken down. `None` for a click that
    /// landed on no row, which closes the menu and does nothing else — the way
    /// a click outside a menu does everywhere.
    pub fn pick_context_menu(&mut self, i: Option<usize>) -> Option<&'static str> {
        let menu = self.context_menu.take()?;
        Some(menu.items.get(i?)?.1)
    }

    /// What the pointer resting on the line containing `at` has to say, out of
    /// the overlays there — `None` when nothing there carries a message.
    ///
    /// **The whole line**, not the character, and that is what makes the answer
    /// agree with what is on screen: a gutter mark is drawn from an overlay
    /// *overlapping the line* (`overlays_for_line` in the renderer, on the line's
    /// first row only), so a message that answered per character would be
    /// available in some columns of a marked line and not others.
    ///
    /// Creation order, last one wins, which is the rule the renderer already
    /// resolves every other overlay attribute by. ponytail: so two diagnostics on
    /// one line show the newer one's message, exactly as they already show only
    /// the newer one's glyph. The upgrade path is joining them the way
    /// `lsp-diagnostics-at-point` does — but it belongs on the Lisp side, since
    /// deciding that two overlays are *both* about the same complaint is not a
    /// fact core has.
    pub fn help_echo_at(&self, at: usize) -> Option<&str> {
        let buf = &self.buffer;
        let line = buf.text.char_to_line(at.min(buf.len_chars()));
        let start = buf.line_start(line);
        // Inclusive of the newline: `%lsp-diagnostic-overlay` falls back to it on
        // an empty line, which is exactly where an unclosed bracket is reported.
        let end = start + buf.line_len(line) + 1;
        buf.overlays()
            .iter()
            .filter(|o| o.end > start && o.start < end)
            .filter_map(|o| o.help_echo.as_deref())
            .next_back()
    }

    /// The documentation the popup is showing, and whether anybody has coloured
    /// it yet. `None` when there is no popup or it has no doc.
    ///
    /// The app's half of [`Completion::doc_spans`]: this is the read, and
    /// [`Editor::set_completion_doc_spans`] is the write. A pair of narrow
    /// methods rather than a public field, so the invariant — spans are offsets
    /// into `doc` joined by newlines — has exactly one place it can be broken.
    pub fn completion_doc_to_colour(&self) -> Option<&[String]> {
        let c = self.completion.as_ref()?;
        (!c.doc.is_empty() && c.doc_spans.is_none()).then_some(&c.doc[..])
    }

    pub fn set_completion_doc_spans(&mut self, spans: Vec<Span>) {
        if let Some(c) = self.completion.as_mut() {
            c.doc_spans = Some(spans);
        }
    }

    /// Latch [`Completion::accepting`]: a key has asked the image to take the
    /// highlighted candidate, and the popup is no longer the keyboard's.
    fn completion_accepting(&mut self) {
        if let Some(c) = self.completion.as_mut() {
            c.accepting = true;
        }
    }

    /// Forget a popup that [`Editor::completion`] is already hiding.
    ///
    /// Without this, hidden is not gone: `Esc` then `i` puts you back in Insert
    /// with the cursor where it was, and the popup — never emptied, only
    /// invisible — would come back holding the candidates for a word you have
    /// stopped typing. Run *before* the key is interpreted, so it sees the
    /// previous key's outcome and fires ahead of the key that would resurrect it.
    fn retire_completion(&mut self) {
        if self.completion.is_some() && self.completion().is_none() {
            self.completion = None;
        }
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

    /// [`Editor::make_overlay`], with the payload set in the same call — see
    /// [`overlay::Overlays::add_with`] for why that is worth a second method.
    pub fn make_overlay_with(
        &mut self,
        start: usize,
        end: usize,
        f: impl FnOnce(&mut Overlay),
    ) -> OverlayId {
        let n = self.buffer.len_chars();
        let (start, end) = (start.min(n), end.min(n));
        if start >= end {
            return 0;
        }
        let id = self.next_overlay_id;
        self.next_overlay_id += 1;
        self.buffer.overlays.add_with(id, start, end, f);
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

    /// Whether *any* bitmap exists. One `is_empty` rather than a scan, and the
    /// renderer's guard against paying for images on a session that has none:
    /// counting the rows an image claims means walking a line's overlays a
    /// second time, and with nothing rasterised the answer is one row every
    /// time. True the moment anything is previewed, editor-wide rather than per
    /// buffer, which is the cheap and safe direction — a buffer with no
    /// fragments pays a scan it did not need rather than skipping one it did.
    pub fn has_images(&self) -> bool {
        !self.images.is_empty()
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
    /// [`Buffer::adopt`], and then the sweep it arms.
    ///
    /// The three callers — `show_named` regenerating a listing, `create_buffer`
    /// emptying a scratchpad, `load` taking a file's text — are all *replacing a
    /// document*, which drops every overlay on it at once. Only `create_buffer`
    /// goes through [`Editor::apply`], so two of the three would never reach the
    /// drain at the end of it and a reverted page's bitmaps would sit there
    /// until something unrelated dropped an overlay somewhere else.
    ///
    /// One method rather than the same two lines three times, for the reason
    /// `apply`'s own drain gives: remembering which callers let go of an overlay
    /// is exactly the mistake that leaves this kind of hole, and a fourth caller
    /// arriving later cannot forget what it never has to remember.
    fn adopt_text(&mut self, text: &str) {
        self.buffer.adopt(text);
        if self.buffer.overlays.take_dropped() {
            self.prune_images();
        }
    }

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
    // Everything here is arithmetic over `display::line_cells`, the *same*
    // function the renderer lays a line out with, because a `j` that computed
    // cells differently would put the cursor where the block is not drawn. The
    // only thing core cannot work out is how many cells fit — `wrap_cols`, which
    // the renderer parks alongside `viewport_lines`.

    /// Whether motion counts screen rows. Wrapping off means one row per line
    /// and this whole path is the identity; `wrap_cols == 0` means nothing has
    /// drawn yet, which is every headless test.
    pub(crate) fn visual_lines(&self) -> bool {
        wraps(&self.buffer, &self.settings) && self.wrap_cols > 0
    }

    /// The cells of buffer line `line`, exactly as the renderer lays it out —
    /// overlays and all, which is what makes `j` land in the column the glyph is
    /// really drawn in on a line whose stars became a bullet or whose `[ ]`
    /// became one box.
    ///
    /// A pure function of (text, overlays, tab width), and that matters because
    /// org-appear changes a line's overlays as the cursor moves onto it: the
    /// answer here is always for the overlays as they stand *now*, which is what
    /// the frame just drew, so nothing is self-referential and a `j` measured on
    /// a revealed line lands on the substituted one below it by its own cells.
    ///
    /// ponytail: an overlay carrying an *image* and no `display` string still
    /// counts as its raw text, because the cells a bitmap reserves are pixels
    /// over a cell width core does not have — see [`display::display_subs`].
    /// Ceiling: a line with a LaTeX preview on it, off by the difference between
    /// the fragment's source and the bitmap's width. Upgrade path: the renderer
    /// parks a cell width on the editor the way it already parks `wrap_cols`.
    fn line_cells(&self, line: usize) -> Vec<display::Cell> {
        let start = self.buffer.line_start(line);
        let end = start + self.buffer.line_len(line);
        let text = self.buffer.slice_string(start, end);
        // ponytail: a linear scan of the buffer's whole overlay list per call,
        // which is exactly what the renderer already does per line per frame.
        // Ceiling: `j` costs O(overlays) per row crossed — microseconds on the
        // few thousand org-modern makes for a file. Upgrade path is an overlay
        // list kept sorted by start, which is a change to `Overlays` and not to
        // anything here.
        display::line_cells(
            &text,
            self.settings.tab_width,
            self.buffer.overlays(),
            start,
            end,
        )
    }

    /// Cell column the cursor is drawn in, *within its display row* — the thing
    /// a vertical run holds on to. Not a character column: on a line of CJK the
    /// two differ by a factor of two, and holding the wrong one walks the cursor
    /// sideways down the screen.
    pub(crate) fn cursor_vcol(&self) -> usize {
        let (line, col) = self.buffer.cursor_line_col();
        let cells = self.line_cells(line);
        let vc = display::visual_col(&cells, col);
        // The offset into the cursor's *own* row, which used to be `vc % cols`.
        // It is a subtraction now for the reason `wrap_breaks` exists: rows no
        // longer start at multiples of the width, so the modulus was the wrong
        // number the moment a line broke between words instead of at the edge.
        // `max(1)` for the same reason [`Editor::visual_target`] does it: the
        // two have to agree about where the rows are, and `g j` is reachable
        // with nothing drawn yet (`wrap_cols == 0`).
        let breaks = display::wrap_breaks(&cells, self.wrap_cols.max(1));
        vc - breaks[display::wrap_row_of(&breaks, vc)]
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
        let mut breaks = display::wrap_breaks(&cells, cols);
        let mut row = display::wrap_row_of(&breaks, display::visual_col(&cells, col));
        for _ in 0..n {
            // Crossing into another buffer line goes through `step_line`, which
            // steps over anything a fold is hiding — the rows of a folded line
            // are not drawn, so they are not rows to move through either.
            match (down, row) {
                (true, r) if r + 1 < breaks.len() => row += 1,
                (false, r) if r > 0 => row -= 1,
                _ => match self.step_line(line, down) {
                    // The first or last visible row: stop, exactly as `j` on the
                    // last line already does.
                    l if l == line => break,
                    l => {
                        line = l;
                        cells = self.line_cells(line);
                        breaks = display::wrap_breaks(&cells, cols);
                        row = match down {
                            true => 0,
                            false => breaks.len() - 1,
                        };
                    }
                },
            }
        }
        // Past the end of a short row is the end of *that row* — not of the
        // line, which is what a raw `cells.len()` clamp gave: with word wrap a
        // row can end well short of the pane's edge, and landing past its end
        // would jump the cursor into the row below.
        let (start, end) = display::wrap_row_range(&breaks, row, cells.len());
        let want = (start + vcol).min(end);
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
        let max = self.max_scroll() as i64;
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
        // `C-d` and `z t` add to `scroll` unconditionally, so without this a
        // file shorter than the viewport walks straight off the top of the
        // screen. The number is [`Editor::max_scroll`] and is the *only* thing
        // `scroll-past-end` changes.
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// The furthest `scroll` may go, in buffer lines — asked by every writer of
    /// a scroll offset, which is `scroll_lines` and the backstop above. `C-d`
    /// and `z t` write one without clamping and are caught by the backstop.
    ///
    /// Off, this is the rule it always was: the last line sits on the *bottom*
    /// row and the pane is full of document. On, the last line may sit on the
    /// *top* row and the rows below it are empty — which is vim's `~` filler
    /// and Emacs' end of buffer, and is why the number is `last_line()` rather
    /// than a count of rows.
    ///
    /// Three things about that spelling are load-bearing:
    ///
    /// - `last_line()` and not `len_lines() - 1`, because a rope counts the
    ///   empty string after a trailing newline and `scroll_lines` *drags point
    ///   to `scroll`*. Naming the phantom line here would put point on a line
    ///   `G` refuses to visit — point leaving the document is the one thing
    ///   this feature must never do, and it is one function call away.
    /// - a *line* and not a row count, so no second row-counting rule enters
    ///   the codebase. The renderer already spends rows on lines exactly once —
    ///   `visible_lines`, which is what parked `viewport_lines` here — and a
    ///   limit expressed in rows would have to agree with it. This one cannot
    ///   disagree with anything: it is the last line, and the draw loop stops
    ///   at `len_lines()` on its own, so the empty rows and the unnumbered
    ///   gutter come out of the renderer unchanged.
    /// - `.max()` and not a plain swap, so the setting can only ever let the
    ///   view go *further*. A one-row pane already reached `len_lines() - 1`
    ///   under the old rule and still does.
    ///
    /// Generated buffers keep the old clamp whatever the setting says. Their
    /// text is a rendering of state rather than a document — there is no "past
    /// the end" of a directory listing or of a terminal's visible grid, and the
    /// wheel over a terminal is intercepted by the app before it ever gets
    /// here. Same instinct as `set-no-gutter-modes`: the chrome a document
    /// wants is noise on a listing.
    ///
    /// ponytail: a *line* and not a drawn line, so a fold covering the end of
    /// the file can be scrolled into and shows an empty pane rather than the
    /// last visible heading. Inherited rather than introduced — the old clamp
    /// walked into the same fold `viewport_lines` earlier — and it closes with
    /// the same upgrade both the fold and the image work named: a scroll
    /// position of (line, row) on `Window`.
    fn max_scroll(&self) -> usize {
        let h = self.viewport_lines.max(1);
        let full = self.buffer.len_lines().saturating_sub(h);
        if !self.settings.scroll_past_end || self.buffer.kind.is_generated() {
            return full;
        }
        full.max(self.buffer.last_line())
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

    /// The menu is the window verbs everywhere, plus the one gesture a listing
    /// has that no mouse would guess at. Through `run_action`, so the entry
    /// cannot do anything the key could not.
    #[test]
    fn a_listing_gets_new_file_on_top_of_the_window_verbs() {
        let mut ed = fresh("a\n");
        ed.open_context_menu(10, 10);
        let plain = ed.context_menu.as_ref().unwrap().items.clone();
        assert!(!plain.iter().any(|(l, _)| *l == "New File"));

        ed.apply(EditorCommand::SetMode(Mode::Dired));
        ed.open_context_menu(10, 10);
        assert_eq!(ed.context_menu.as_ref().unwrap().items[0].0, "New File");
        assert_eq!(ed.context_menu.as_ref().unwrap().items[1..], plain[..]);
        assert_eq!(ed.pick_context_menu(Some(0)), Some("dired-create-file"));
        assert_eq!(
            ed.run_action("dired-create-file"),
            vec![EditorCommand::Dired("create-file".into())]
        );
    }

    // --- corfu -----------------------------------------------------------

    /// A buffer mid-word in Insert mode with a popup up, which is the only
    /// state any of this is about.
    fn completing(text: &str, at: usize, cursor: usize, rows: &[&str]) -> Editor {
        let mut ed = fresh(text);
        ed.mode = Mode::Insert;
        ed.buffer.cursor = cursor;
        ed.apply(EditorCommand::Completion(CompletionEdit::Show(Some((at, 0)))));
        for r in rows {
            ed.apply(EditorCommand::Completion(CompletionEdit::Row(Some(
                r.to_string(),
            ))));
        }
        ed
    }

    /// The three keys core answers *instead of* the image, and the reason it
    /// has to: a Lisp command bound to `RET` could not insert the newline in
    /// the no-popup case without landing a queue turn late. Asked here, the
    /// question is exact and the fallback is the ordinary arm below it.
    #[test]
    fn tab_and_ret_drive_the_popup_while_one_is_up_and_mean_themselves_otherwise() {
        let lisp_of = |cmds: &[EditorCommand]| {
            cmds.iter().find_map(|c| match c {
                EditorCommand::CallLisp(s) => Some(s.clone()),
                _ => None,
            })
        };

        let mut ed = completing("let foo", 4, 7, &["format!", "foo_bar"]);
        assert_eq!(
            lisp_of(&ed.handle_key(Key::Tab)).as_deref(),
            Some("(lsp-complete-next)"),
            "TAB cycles rather than typing whitespace"
        );
        assert_eq!(
            lisp_of(&ed.handle_key(Key::BackTab)).as_deref(),
            Some("(lsp-complete-previous)")
        );
        assert_eq!(
            lisp_of(&ed.handle_key(Key::Enter)).as_deref(),
            Some("(lsp-complete-accept)")
        );

        // ...and the second `RET`, pressed before the image has had its queue
        // turn, is a newline and not a second accept. Without the latch it
        // would insert the same candidate twice and eat the line break.
        assert!(
            ed.handle_key(Key::Enter)
                .contains(&EditorCommand::InsertNewline),
            "an accept already in flight hands RET back to the document"
        );

        // With no popup at all both keys are themselves, on the same keystroke
        // rather than a frame later.
        let mut ed = fresh("let foo");
        ed.mode = Mode::Insert;
        assert!(ed.handle_key(Key::Enter).contains(&EditorCommand::InsertNewline));
        assert!(matches!(
            ed.handle_key(Key::Tab).as_slice(),
            [EditorCommand::InsertText(_)]
        ));
    }

    #[test]
    fn a_completion_popup_is_only_shown_while_point_is_still_in_the_word_it_describes() {
        let mut ed = completing("let foo\nbar", 4, 7, &["format!", "foo_bar"]);
        assert_eq!(ed.completion().map(|c| c.rows.len()), Some(2));

        // Typing further into the word keeps it: that is the feature.
        ed.buffer.cursor = 7;
        assert!(ed.completion().is_some());

        // Backspacing off the front of the word does not.
        ed.buffer.cursor = 3;
        assert!(ed.completion().is_none(), "point is before the anchor");

        // Nor does landing on another line, which is how `j` retires the popup
        // without core knowing anything about `j`.
        ed.buffer.cursor = 9;
        assert!(ed.completion().is_none(), "point is on another line");

        // Nor does leaving Insert.
        ed.buffer.cursor = 7;
        ed.mode = Mode::Normal;
        assert!(ed.completion().is_none(), "a popup is an Insert-mode thing");
    }

    #[test]
    fn a_hidden_completion_popup_does_not_come_back_when_insert_does() {
        let mut ed = completing("let foo", 4, 7, &["format!"]);
        // Esc, then `i`: the mode is what hid it and the mode is coming back, so
        // without `retire_completion` the candidates for a word you stopped
        // typing would reappear.
        feed(&mut ed, &[Key::Esc]);
        assert!(ed.completion().is_none());
        feed(&mut ed, &[Key::Char('i')]);
        assert!(
            ed.completion().is_none(),
            "hidden has to mean gone, not merely invisible"
        );
    }

    #[test]
    fn showing_at_the_same_anchor_keeps_the_rows_and_moving_it_drops_them() {
        let mut ed = completing("let foo", 4, 7, &["format!", "foo_bar"]);
        // A selection change is one command and must not cost the list.
        ed.apply(EditorCommand::Completion(CompletionEdit::Show(Some((4, 1)))));
        assert_eq!(ed.completion().map(|c| c.selected), Some(1));
        assert_eq!(ed.completion().map(|c| c.rows.len()), Some(2));

        // A new word cannot inherit the last one's candidates, even if whoever
        // filled it in forgot to clear them.
        ed.apply(EditorCommand::Completion(CompletionEdit::Show(Some((5, 0)))));
        assert_eq!(ed.completion().map(|c| c.rows.len()), Some(0));
    }

    #[test]
    fn a_selection_out_of_the_image_is_clamped_rather_than_indexed_with() {
        let mut ed = completing("let foo", 4, 7, &["format!"]);
        ed.apply(EditorCommand::Completion(CompletionEdit::Show(Some((4, 99)))));
        assert_eq!(ed.completion().map(|c| c.selected), Some(0));
    }

    #[test]
    fn the_visible_rows_scroll_to_hold_the_selection() {
        let mut c = Completion {
            at: 0,
            rows: (0..8).map(|i| i.to_string()).collect(),
            ..Default::default()
        };
        assert_eq!(c.visible(3), (0, &c.rows[0..3]));
        // Still in the first window.
        c.selected = 2;
        assert_eq!(c.visible(3).0, 0);
        // Past it, so the window follows rather than the selection leaving.
        c.selected = 5;
        let (first, rows) = c.visible(3);
        assert_eq!(first, 3);
        assert_eq!(rows, ["3", "4", "5"]);
        // The last row never scrolls past the end of the list.
        c.selected = 7;
        assert_eq!(c.visible(3), (5, &c.rows[5..8]));
        // A box with no room draws nothing rather than panicking on a slice.
        assert_eq!(c.visible(0), (0, &[] as &[String]));
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
    fn a_face_remembers_the_weight_and_slant_it_was_given() {
        let mut ed = fresh("");
        ed.apply(EditorCommand::SetFaceStyle("keyword".into(), true, false));
        ed.apply(EditorCommand::SetFaceStyle("comment".into(), false, true));
        assert_eq!(
            ed.theme.style(HlKind::Keyword),
            FaceStyle { bold: true, italic: false }
        );
        assert_eq!(
            ed.theme.style(HlKind::Comment),
            FaceStyle { bold: false, italic: true }
        );
        // Setting one face says nothing about any other, and a style set back to
        // plain really goes back to plain — a theme reloaded over another one
        // has to be able to undo what the first said.
        assert_eq!(ed.theme.style(HlKind::String), FaceStyle::default());
        ed.apply(EditorCommand::SetFaceStyle("keyword".into(), false, false));
        assert_eq!(ed.theme.style(HlKind::Keyword), FaceStyle::default());
    }

    #[test]
    fn styling_a_face_that_does_not_exist_complains_and_changes_nothing() {
        let mut ed = fresh("");
        ed.apply(EditorCommand::SetFaceStyle("keywrod".into(), true, true));
        assert_eq!(ed.status, "unknown syntax face: keywrod");
        assert!(HlKind::ALL.iter().all(|&k| ed.theme.style(k) == FaceStyle::default()));
    }

    #[test]
    fn the_default_theme_is_upright_body_weight_throughout() {
        // The guarantee that this feature is invisible until a theme asks for
        // it: nothing renders bold or italic on a face until something sets one.
        let ed = fresh("");
        for k in HlKind::ALL {
            assert_eq!(ed.theme.style(k), FaceStyle::default(), "{}", k.name());
        }
    }

    #[test]
    fn normalize_keys_handles_both_forms() {
        assert_eq!(normalize_keys("gd"), "g d");
        assert_eq!(normalize_keys("SPC f f"), "SPC f f");
        assert_eq!(normalize_keys("C-x C-f"), "C-x C-f");
        // Named keys are one token, not five characters.
        for named in [
            "<tab>", "<backtab>", "<ret>", "<esc>", "<bs>", "<left>", "<home>", "<end>",
            "<pageup>", "<pagedown>", "<delete>", "<f1>", "<f12>",
        ] {
            assert_eq!(normalize_keys(named), named);
        }
        // ...and each still matches what the key actually produces.
        assert_eq!(normalize_keys(&Key::Tab.token()), Key::Tab.token());
        // `<backtab>` is a key of its own, so a config can bind it and it can
        // never be mistaken for the `<tab>` it used to arrive as.
        assert_eq!(Key::from_token("<backtab>"), Some(Key::BackTab));
        assert_eq!(Key::from_token("S-<tab>"), Some(Key::BackTab));
        assert_ne!(Key::BackTab.token(), Key::Tab.token());
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

    /// `narrow` on its own, because the off-by-ones live here and a whole-buffer
    /// case would hide them.
    #[test]
    fn the_smallest_splice_that_turns_one_text_into_another() {
        // A change in the middle touches only the middle.
        assert_eq!(Buffer::narrow("abcXdef", "abcYdef"), (3, 1, "Y"));
        // Pure insertion and pure deletion.
        assert_eq!(Buffer::narrow("abcdef", "abcZZdef"), (3, 0, "ZZ"));
        assert_eq!(Buffer::narrow("abcZZdef", "abcdef"), (3, 2, ""));
        // Identical text is no edit at all, which is what lets `revert_buffer`
        // return without bumping the revision.
        assert_eq!(Buffer::narrow("same", "same"), (4, 0, ""));
        // Nothing in common, so it really is the whole document.
        assert_eq!(Buffer::narrow("abc", "xyz"), (0, 3, "xyz"));
        // The prefix and the suffix must not both claim the same characters:
        // "aa" -> "aaa" is one insertion, not two overlapping ones.
        assert_eq!(Buffer::narrow("aa", "aaa"), (2, 0, "a"));
        assert_eq!(Buffer::narrow("aaa", "aa"), (2, 1, ""));
        // Counted in characters and sliced in bytes, which are not the same
        // number the moment a document is not ASCII.
        assert_eq!(Buffer::narrow("αβγ", "αΔγ"), (1, 1, "Δ"));
        // Empty on either side.
        assert_eq!(Buffer::narrow("", "new"), (0, 0, "new"));
        assert_eq!(Buffer::narrow("old", ""), (0, 3, ""));
    }

    /// What an agent rewriting a file must not cost you.
    ///
    /// A fold is an overlay, and so is every org-modern glyph and every LaTeX
    /// preview. `revert_buffer` used to splice the whole document, and
    /// `Overlays::adjust` drops an overlay the edit swallowed — so a file
    /// rewritten underneath the editor came back correct and completely
    /// unfolded. The splice is now the changed span only, and everything outside
    /// it is outside the edit.
    #[test]
    fn reverting_a_file_keeps_the_overlays_the_change_did_not_touch() {
        let mut ed = Editor::new();
        ed.load("one\ntwo\nthree\nfour\n", None, None);
        // After the load, not before: `load` switches to a buffer of its own,
        // and the id taken first is the dashboard's.
        let id = ed.buffer.id;
        // Over "four", the last line — well clear of the edit below, and the
        // one that used to vanish on every write by an agent.
        let far = ed.make_overlay(14, 18);
        // ...and one over "two", which the edit lands on.
        let near = ed.make_overlay(4, 7);

        assert!(ed.revert_buffer(id, "one\nTWO\nthree\nfour\n"));
        assert_eq!(ed.buffer.slice_string(0, 19), "one\nTWO\nthree\nfour\n");
        assert_eq!(
            ed.overlay_span(far),
            Some((14, 18)),
            "an overlay outside the change must not move or vanish"
        );
        // The one *on* the change survives too, because a three-character
        // replacement is a three-character splice and `adjust` moves its ends
        // rather than collapsing them. Worth pinning: it is the difference
        // between a fold around an edited line staying folded and springing
        // open every time an agent touches the line.
        assert_eq!(ed.overlay_span(near), Some((4, 7)));

        // The drop rule still holds where it should. Deleting the line an
        // overlay covers really does swallow it, and an overlay describing text
        // that is gone has to go — that is the same rule as any other edit, and
        // the one the whole-buffer splice was accidentally applying to
        // everything.
        let doomed = ed.make_overlay(8, 14);
        assert!(ed.revert_buffer(id, "one\nTWO\nfour\n"));
        assert_eq!(ed.overlay_span(doomed), None, "its text was deleted");

        // A rewrite that changed nothing changes nothing — no dropped overlays,
        // and no revision bump to re-parse and re-render an identical buffer.
        let before = ed.revision;
        let kept = ed.make_overlay(4, 7);
        assert!(ed.revert_buffer(id, "one\nTWO\nfour\n"));
        assert_eq!(ed.overlay_span(kept), Some((4, 7)));
        assert_eq!(ed.revision, before, "an unchanged file is not an edit");
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

    /// The command sequence a mouse drag emits, without a mouse.
    ///
    /// The pointer arithmetic lives in the renderer and the event plumbing in
    /// the app, but what those two agree to send is this — press, then Visual,
    /// then a `MoveTo` per motion — and it is the part that has to keep meaning
    /// "a selection anchored where the press landed".
    #[test]
    fn a_drag_is_a_visual_selection_anchored_at_the_press() {
        let (mut ed, _) = marked("alpha beta gamma", 0);
        ed.apply(EditorCommand::MoveTo(6)); // the press, on `b`
        ed.apply(EditorCommand::SetMode(Mode::Visual));
        ed.apply(EditorCommand::MoveTo(9)); // dragged three characters right
        // Inclusive of the character under the cursor, as vim's visual is —
        // so `beta` is four characters and the range ends at 10.
        assert_eq!(ed.selection(), Some((6, 10)));
        // Every motion after the first moves only the far end: the anchor is
        // set once, on entering Visual, and `v` then `C-v` keeping it is the
        // same rule.
        ed.apply(EditorCommand::MoveTo(2)); // dragged back past the start
        assert_eq!(ed.selection(), Some((2, 7)));

        // A fresh press collapses it, which is what stops the next click from
        // dragging the far end of the selection you just made.
        ed.apply(EditorCommand::SetMode(Mode::Normal));
        ed.apply(EditorCommand::MoveTo(11));
        assert_eq!(ed.selection(), None);
    }

    /// Undo runs the markers *backwards through the edit* rather than clamping
    /// them into the restored text, which is the difference between a marker
    /// that survives an undo and one that merely stays inside the document.
    #[test]
    fn undo_puts_a_marker_back_where_it_was() {
        let (mut ed, m) = marked("alpha", 0);
        ed.apply(EditorCommand::Checkpoint);
        ed.apply(EditorCommand::MoveTo(5));
        ed.apply(EditorCommand::InsertText(" and beta".into()));
        ed.set_marker(m, 12);
        ed.apply(EditorCommand::Undo);
        assert_eq!(ed.buffer.text.to_string(), "alpha");
        // Inside the text the undo removed, so it collapses onto the start of
        // the removal — the same answer a marker inside any deleted range gets.
        assert_eq!(ed.marker_position(m), Some(5));

        // The one that changed when undo stopped being a whole-document swap.
        // An insertion *above* the marker pushed it from 6 to 8; undoing that
        // insertion has to bring it back to 6, because 6 is where it was
        // pointing — at the `beta`. Clamping left it at 8, still inside the
        // document and two characters wrong, which is what put a LaTeX preview
        // over the wrong equation after an undo higher up the file.
        let (mut ed, m) = marked("alpha beta", 6);
        ed.apply(EditorCommand::Checkpoint);
        ed.apply(EditorCommand::MoveTo(0));
        ed.apply(EditorCommand::InsertText("xy".into()));
        assert_eq!(ed.marker_position(m), Some(8));
        ed.apply(EditorCommand::Undo);
        assert_eq!(ed.marker_position(m), Some(6));
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

    /// Opening a file applies the no-gutter list, like every other route to a
    /// major mode.
    ///
    /// The bug this pins had a shape worth remembering: the list was applied
    /// when it was *set* (to the buffers that existed then) and when a mode was
    /// set *by hand*, and a file opened from the prompt goes through neither —
    /// so it inherited whatever flag the reused buffer carried. Every org file
    /// opened normally showed the numbers the shipped config switches off, and
    /// `M-x org-mode` on that same buffer then took them away, which reads as a
    /// mystery rather than as a missing line.
    ///
    /// Asserted in both directions on one editor, because "org has no gutter" is
    /// only interesting beside "rust still does" — a fix that switched the
    /// gutter off everywhere would pass half of this.
    #[test]
    fn opening_a_file_takes_the_gutter_from_its_mode() {
        let mut ed = Editor::new();
        ed.apply(EditorCommand::SetNoGutterModes("org-mode text-mode".into()));

        ed.load("fn main() {}", None, Some("rust".into()));
        assert_eq!(ed.buffer.line_numbers, Some(true), "rust keeps its gutter");

        ed.load("* Heading", None, Some("org".into()));
        assert_eq!(ed.buffer.line_numbers, Some(false), "org is on the list");

        // ...and back, so the flag tracks the mode rather than latching on the
        // first file that set it.
        ed.load("fn main() {}", None, Some("rust".into()));
        assert_eq!(ed.buffer.line_numbers, Some(true), "the flag follows the mode");
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

    /// The blank-line half of the claim [`Editor::line_cells`] exists for: what
    /// `j`, a click and a wrap point count on an empty line carrying ghost text
    /// is the cells the renderer draws there, not the zero its text has. Core
    /// used to say zero on both counts — `display_subs` dropped the overlay and
    /// `substitute` had no cell to walk — so the two sides agreed only by both
    /// drawing nothing.
    #[test]
    fn an_empty_line_is_laid_out_with_the_display_string_on_it() {
        // "a\n\nb\n": line 1 is blank, chars [2, 2), and the only range an
        // overlay on it can have is the newline that ends it.
        let mut ed = fresh("a\n\nb\n");
        let ghost = ed.make_overlay(2, 3);
        ed.apply(EditorCommand::Overlay(OverlayEdit::Display(
            ghost,
            Some("type here".into()),
        )));
        let text_of = |c: Vec<display::Cell>| c.iter().map(|&(c, _)| c).collect::<String>();
        assert_eq!(text_of(ed.line_cells(1)), "type here");
        // ...and a blank line nothing was put on is still blank, so this is the
        // overlay's doing and not a newline that grew a column.
        assert!(text_of(ed.line_cells(3)).is_empty());
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
            Key::MetaEnter,
            Key::MetaBackspace,
            Key::MetaLeft,
            Key::MetaRight,
            Key::ShiftEnter,
            Key::MetaShiftEnter,
            Key::ShiftLeft,
            Key::ShiftRight,
            Key::ShiftUp,
            Key::ShiftDown,
            Key::MetaShiftLeft,
            Key::MetaShiftRight,
            Key::Enter,
            Key::Tab,
            Key::BackTab,
            Key::Backspace,
            Key::Esc,
            Key::Left,
            Key::Right,
            Key::Up,
            Key::Down,
            Key::Home,
            Key::End,
            Key::PageUp,
            Key::PageDown,
            Key::Delete,
            Key::F(1),
            Key::F(9),
            Key::F(12),
        ];
        for key in all {
            assert_eq!(Key::from_token(&key.token()), Some(key), "{}", key.token());
        }
        // Emacs' names for the page keys are read but not written — a binding
        // copied out of an `.emacs` works, and `token` still answers with the
        // spelling that says which way it goes.
        assert_eq!(Key::from_token("<prior>"), Some(Key::PageUp));
        assert_eq!(Key::from_token("<next>"), Some(Key::PageDown));
        assert_eq!(Key::PageUp.token(), "<pageup>");
        // Only the twelve the terminal has sequences for, so a binding that
        // parses is a binding that can be pressed.
        assert_eq!(Key::from_token("<f13>"), None);
        assert_eq!(Key::from_token("<f0>"), None);
        assert_eq!(Key::from_token("<f>"), None);
        assert_eq!(Key::from_token("<fx>"), None);
        // A chord is folded to lower case on the way out, so that is what comes
        // back — `token` is the canonical spelling and this agrees with it.
        assert_eq!(Key::from_token("C-A"), Some(Key::Ctrl('a')));
        // The shifted spellings have exactly one order, so `S-M-<ret>` is a typo
        // rather than a second name for a chord already in the map.
        assert_eq!(Key::from_token("S-M-<ret>"), None);
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

        // The renderer's O(1) guard, asserted on the same events, because it is
        // what decides whether a line's overlays get walked for the rows an
        // image claimed at all: `false` while a bitmap is live is a display
        // equation the row count silently stops seeing, and `true` forever is a
        // scan every line of every buffer pays for nothing.
        assert!(ed.has_images(), "three bitmaps went in, two survived");
        assert!(!Editor::new().has_images(), "and a fresh editor has none");
    }

    /// Deleting the *text* under a preview frees its bitmap, not just deleting
    /// the overlay.
    ///
    /// The sweep used to hang off `EditorCommand::Overlay` alone, so the way you
    /// actually get rid of a preview — select the equation, `d`, the overlay
    /// collapses inside `splice` — left a few hundred KB and the renderer's
    /// texture for it sitting there until some unrelated overlay elsewhere
    /// happened to be deleted. Bounded, and still a leak with no upper bound
    /// anybody could name.
    #[test]
    fn deleting_the_text_under_a_preview_frees_its_bitmap() {
        let mut ed = fresh("hello world");
        ed.add_image(9, Image { width: 4, height: 4, depth: 0, rgba: vec![0; 64] });
        let preview = ed.make_overlay(0, 5);
        ed.apply(EditorCommand::Overlay(OverlayEdit::Image(preview, Some(9))));
        assert!(ed.has_image(9));

        // A real splice over exactly the overlay's range, which is what a visual
        // selection and `d` comes down to.
        ed.apply(EditorCommand::DeleteRange(0, 5));
        assert_eq!(ed.buffer.overlays.span(preview), None, "the overlay went");
        assert!(!ed.has_image(9), "and its bitmap went with it");

        // Undo brings the text back but not the overlay — an overlay is not in
        // the snapshot — so the bitmap stays gone rather than coming back
        // unreferenced.
        ed.apply(EditorCommand::Undo);
        assert!(!ed.has_image(9));
    }

    /// Re-typesetting frees the bitmap it replaced.
    ///
    /// The sibling of the entry above, and it hid for the same reason in
    /// reverse: `Image(_, None)` was in the `drops` list and `Image(_, Some)`
    /// was not, so *clearing* a preview swept and *replacing* one did not. Every
    /// route that re-renders the same fragment — a theme change, a zoom, an edit
    /// inside `$…$` — goes through the second arm, so the leak was one per
    /// re-render of every equation on screen rather than a rarity.
    #[test]
    fn re_typesetting_a_preview_frees_the_bitmap_it_replaced() {
        let mut ed = fresh("hello world");
        ed.add_image(9, Image { width: 4, height: 4, depth: 0, rgba: vec![0; 64] });
        let preview = ed.make_overlay(0, 5);
        ed.apply(EditorCommand::Overlay(OverlayEdit::Image(preview, Some(9))));

        // The same overlay, a second bitmap: nothing points at 9 any more, and
        // no overlay was dropped, so the text path's flag never fires for this.
        ed.add_image(10, Image { width: 8, height: 8, depth: 0, rgba: vec![0; 256] });
        ed.apply(EditorCommand::Overlay(OverlayEdit::Image(preview, Some(10))));
        assert!(!ed.has_image(9), "the bitmap it replaced went");
        assert!(ed.has_image(10), "and the one that replaced it stayed");
    }

    /// Regenerating a listing frees the bitmaps that were on the old one.
    ///
    /// `Buffer::adopt` drops every overlay at once, and `show_named` — which
    /// rebuilds a dired or magit listing in place — does not go through
    /// [`Editor::apply`], so the drain at the end of it never came here.
    ///
    /// `load` was the other suspect and turned out not to be one, which is worth
    /// recording so nobody "fixes" it: opening a file *stacks* the outgoing
    /// buffer unless it was pristine, so its overlays are still live in
    /// `others` and its bitmap is still named. Freeing there would be the bug.
    #[test]
    fn regenerating_a_listing_frees_the_bitmaps_that_were_on_it() {
        let mut ed = fresh("listing");
        ed.add_image(11, Image { width: 4, height: 4, depth: 0, rgba: vec![0; 64] });
        let mark = ed.make_overlay(0, 4);
        ed.apply(EditorCommand::Overlay(OverlayEdit::Image(mark, Some(11))));
        assert!(ed.has_image(11));

        ed.show_named(BufferKind::Text, None, "regenerated");
        assert!(!ed.has_image(11), "a regenerated listing keeps none of its marks");
    }

    /// The buffer a file-open pushed aside keeps its previews, because it keeps
    /// its overlays — see the note above. The other half of that pair.
    #[test]
    fn a_stacked_buffer_keeps_the_preview_that_was_on_it() {
        let mut ed = fresh("$x^2$ and more");
        ed.add_image(9, Image { width: 4, height: 4, depth: 0, rgba: vec![0; 64] });
        let preview = ed.make_overlay(0, 5);
        ed.apply(EditorCommand::Overlay(OverlayEdit::Image(preview, Some(9))));

        ed.load("something else entirely", None, None);
        assert!(ed.has_image(9), "the buffer it belongs to is stacked, not gone");
    }

    /// The other half of the policy: an edit that dropped *nothing* must not pay
    /// for the sweep, and must not lose a bitmap to it either.
    #[test]
    fn an_ordinary_edit_leaves_every_bitmap_alone() {
        let mut ed = fresh("hello world");
        ed.add_image(9, Image { width: 4, height: 4, depth: 0, rgba: vec![0; 64] });
        let preview = ed.make_overlay(6, 11);
        ed.apply(EditorCommand::Overlay(OverlayEdit::Image(preview, Some(9))));

        ed.apply(EditorCommand::InsertAt(0, "say ".into()));
        assert!(!ed.buffer.overlays.take_dropped(), "nothing collapsed");
        assert!(ed.has_image(9), "so nothing was swept");
        assert_eq!(ed.buffer.overlays.span(preview), Some((10, 15)));
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

    /// Clearing a mode's overlays one at a time leaves the image table exactly
    /// right — nothing leaked, nothing swept that something still names.
    ///
    /// The shape of `(mapc #'delete-overlay ovs)` in `org-modern.lisp` and
    /// `org-latex.lisp`, and the reason the sweep is armed per *overlay* rather
    /// than per delete: almost none of these carries a bitmap, so almost none of
    /// them may cost a walk over every buffer — but the two that do must still
    /// free theirs, and the walk they arm must not take the page's figure or the
    /// dashboard's logo with it.
    #[test]
    fn clearing_a_thousand_overlays_leaves_the_image_table_exact() {
        let mut ed = Editor::new();
        let bitmap = |n| Image { width: n, height: n, depth: 0, rgba: vec![0; (n * n * 4) as usize] };
        for id in 1..=5 {
            ed.add_image(id, bitmap(4));
        }
        ed.dashboard.logo = Some(1); // the editor's own, owned by no document

        // A second document holding a page, so the sweep has more than the live
        // buffer to walk and something to wrongly free.
        ed.apply(EditorCommand::CreateBuffer("*page*".into()));
        let mut scene = zemacs_gui::Scene::default();
        let figure = scene.push(zemacs_gui::Node::Image { image: 2, width: 4, height: 4, depth: 0 });
        scene.set_root(figure);
        ed.apply(EditorCommand::SetScene(Some(scene)));

        ed.apply(EditorCommand::CreateBuffer("*doc*".into()));
        ed.apply(EditorCommand::InsertText("x".repeat(4100)));
        // A preview that survives the clear, because nothing deletes it.
        let keeper = ed.make_overlay(0, 4);
        ed.apply(EditorCommand::Overlay(OverlayEdit::Image(keeper, Some(3))));

        // A thousand plain overlays, two of them previews, cleared oldest-first.
        let mut doomed = Vec::new();
        for i in 0..1000 {
            let ov = ed.make_overlay(i * 4 + 8, i * 4 + 12);
            if i == 250 {
                ed.apply(EditorCommand::Overlay(OverlayEdit::Image(ov, Some(4))));
            }
            if i == 750 {
                ed.apply(EditorCommand::Overlay(OverlayEdit::Image(ov, Some(5))));
            }
            doomed.push(ov);
        }
        assert!((1..=5).all(|id| ed.has_image(id)), "all five are named");

        for ov in doomed {
            ed.apply(EditorCommand::Overlay(OverlayEdit::Delete(ov)));
        }

        assert_eq!(ed.buffer.overlays().len(), 1, "only the keeper is left");
        assert!(ed.has_image(1), "the dashboard's logo belongs to no document");
        assert!(ed.has_image(2), "the other buffer's page still names its figure");
        assert!(ed.has_image(3), "the preview nobody deleted keeps its bitmap");
        assert!(!ed.has_image(4), "the deleted preview's bitmap went");
        assert!(!ed.has_image(5), "and so did the second one's");
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
            previewing: false,
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
            previewing: false,
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
            previewing: false,
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
            previewing: false,
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
            previewing: false,
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

    // --- the change log ------------------------------------------------------

    /// The only thing a record actually promises: the text before `start` and
    /// the text after the two ends are the same in both documents.
    ///
    /// Asserted rather than a literal offset triple, because that is the
    /// property every reader depends on and a literal would only say that the
    /// arithmetic had not changed. A record that is *wider* than it had to be
    /// passes — see [`Change::coalesce`], which is deliberately conservative.
    fn record_describes(before: &str, after: &str, c: Change) {
        let b: Vec<char> = before.chars().collect();
        let a: Vec<char> = after.chars().collect();
        assert!(c.start <= c.old_end && c.start <= c.new_end, "inverted {c:?}");
        assert!(c.old_end <= b.len() && c.new_end <= a.len(), "past the end {c:?}");
        assert_eq!(b[..c.start], a[..c.start], "prefix moved under {c:?}");
        assert_eq!(b[c.old_end..], a[c.new_end..], "suffix moved under {c:?}");
    }

    /// Run `keys` and check that what the log says happened is what happened.
    fn edits_are_described_by_their_record(text: &str, keys: &[Key]) {
        let mut ed = fresh(text);
        let seen = ed.buffer.change_count();
        let before = ed.buffer.text.to_string();
        feed(&mut ed, keys);
        let after = ed.buffer.text.to_string();
        let log = ed.buffer.changes_since(seen).expect("nothing has overflowed");
        match Change::coalesce(log) {
            Some(c) => record_describes(&before, &after, c),
            None => assert_eq!(before, after, "no record, but the text moved: {keys:?}"),
        }
    }

    /// Every keystroke that edits, put through the invariant. The scripts are
    /// chosen for the shapes that break the coalescing arithmetic: an insert
    /// followed by a delete *behind* it, a delete that swallows a previous
    /// insert whole, and a paste that lands before everything already recorded.
    #[test]
    fn a_record_names_exactly_the_text_that_moved() {
        let text = "one two three\nfour five six\nseven eight\n";
        for keys in [
            &[Key::Char('x')][..],
            &[Key::Char('d'), Key::Char('d')],
            &[Key::Char('i'), Key::Char('a'), Key::Char('b'), Key::Esc],
            &[Key::Char('o'), Key::Char('z'), Key::Esc],
            &[Key::Char('A'), Key::Char('!'), Key::Esc],
            // insert, then delete from *before* where the insert landed
            &[
                Key::Char('j'),
                Key::Char('i'),
                Key::Char('q'),
                Key::Esc,
                Key::Char('k'),
                Key::Char('d'),
                Key::Char('d'),
            ],
            // yank a line and paste it above everything typed so far
            &[
                Key::Char('y'),
                Key::Char('y'),
                Key::Char('G'),
                Key::Char('o'),
                Key::Char('w'),
                Key::Esc,
                Key::Char('g'),
                Key::Char('g'),
                Key::Char('P'),
            ],
            &[Key::Char('x'), Key::Char('x'), Key::Char('u')],
            &[Key::Char('d'), Key::Char('d'), Key::Char('u'), Key::Ctrl('r')],
        ] {
            edits_are_described_by_their_record(text, keys);
        }
    }

    /// The same invariant where it is hardest to get right: text that is not
    /// one byte per character, so a record in bytes would slice mid-codepoint.
    #[test]
    fn a_record_is_in_characters_not_bytes() {
        edits_are_described_by_their_record(
            "café ▸ Übung\nmañana\n",
            &[
                Key::Char('l'),
                Key::Char('l'),
                Key::Char('i'),
                Key::Char('é'),
                Key::Esc,
                Key::Char('x'),
            ],
        );
    }

    /// One splice, one record — the property that lets a reader trust the log
    /// instead of diffing. Checked against the offsets a caller can predict,
    /// which is the one place a literal triple is worth asserting on.
    #[test]
    fn one_splice_is_one_record() {
        let mut ed = fresh("hello world");
        let seen = ed.buffer.change_count();
        ed.apply(EditorCommand::DeleteRange(0, 6));
        ed.apply(EditorCommand::MoveTo(2));
        ed.apply(EditorCommand::InsertText("!!".into()));
        assert_eq!(
            ed.buffer.changes_since(seen).unwrap(),
            [
                Change { start: 0, old_end: 6, new_end: 0 },
                Change { start: 2, old_end: 2, new_end: 4 },
            ]
        );
        assert_eq!(ed.buffer.text.to_string(), "wo!!rld");
    }

    /// The bug this was reported as: "LaTeX previews render nicely, but adding
    /// them messes up undo".
    ///
    /// A preview is an overlay over the `$…$` it replaces. Undo used to hand the
    /// buffer a whole-document replacement and then *clamp* the overlays, so an
    /// undo anywhere above a preview left the preview at its old absolute
    /// offset while the text slid out from under it — an image over the wrong
    /// equation, and the same for every fold, diagnostic mark and org-modern
    /// bullet in the file. Nothing about the undo *stack* was ever broken,
    /// which is why it took a document with overlays in it to see.
    #[test]
    fn an_overlay_follows_the_text_back_through_an_undo() {
        let mut ed = fresh("intro\n$x^2$\n");
        // The preview, over the fragment on line 2.
        let ov = 1u64;
        ed.buffer.overlays.add(ov, 6, 11);
        assert_eq!(ed.buffer.overlays.span(ov), Some((6, 11)));

        // An edit *above* it, which pushes it along...
        ed.apply(EditorCommand::Checkpoint);
        ed.apply(EditorCommand::MoveTo(0));
        ed.apply(EditorCommand::InsertText("PREFIX ".into()));
        assert_eq!(ed.buffer.overlays.span(ov), Some((13, 18)));

        // ...and undoing that edit has to bring it back to the equation rather
        // than leaving it seven characters along, still inside the document and
        // over `\n$x^2` instead of `$x^2$`.
        ed.apply(EditorCommand::Undo);
        assert_eq!(ed.buffer.text.to_string(), "intro\n$x^2$\n");
        assert_eq!(ed.buffer.overlays.span(ov), Some((6, 11)));
        assert_eq!(&ed.buffer.slice_string(6, 11), "$x^2$");

        // ...and redo takes it back with the text, which is the same claim in
        // the other direction.
        ed.apply(EditorCommand::Redo);
        assert_eq!(ed.buffer.overlays.span(ov), Some((13, 18)));
        assert_eq!(&ed.buffer.slice_string(13, 18), "$x^2$");
    }

    /// Undo reports the *range* it changed, not the whole document.
    ///
    /// It used to report `0..old -> 0..new`, which was honest about a snapshot
    /// being two ropes and cost a full reparse and a full `didChange` for a `u`
    /// on one character. The two ropes differ by exactly the edit that was
    /// undone, and that is one contiguous range — so undo splices it, and every
    /// reader downstream gets the small record it would have got for the
    /// original keystroke.
    #[test]
    fn undo_and_redo_report_only_what_changed() {
        let mut ed = fresh("abcdef");
        feed(&mut ed, &[Key::Char('x')]);
        let seen = ed.buffer.change_count();
        feed(&mut ed, &[Key::Char('u')]);
        // Putting the `a` back: one character inserted at 0.
        assert_eq!(
            ed.buffer.changes_since(seen).unwrap(),
            [Change { start: 0, old_end: 0, new_end: 1 }]
        );
        let seen = ed.buffer.change_count();
        feed(&mut ed, &[Key::Ctrl('r')]);
        assert_eq!(
            ed.buffer.changes_since(seen).unwrap(),
            [Change { start: 0, old_end: 1, new_end: 0 }]
        );
    }

    /// A revision can move without the text moving. Those two answers must not
    /// collapse, or every minor-mode toggle would cost a reparse and a full
    /// `didChange`.
    #[test]
    fn a_revision_that_moved_without_the_text_records_nothing() {
        let mut ed = fresh("(defun f () 1)");
        let seen = ed.buffer.change_count();
        let revision = ed.revision;
        ed.apply(EditorCommand::SetLanguage(Some("lisp".into())));
        assert_ne!(ed.revision, revision, "the app re-highlights off this");
        assert_eq!(ed.buffer.changes_since(seen), Some(&[][..]));
    }

    /// Falling behind is answered with "reread the text", never with a slice
    /// that starts in the middle of what the reader missed.
    #[test]
    fn a_reader_that_fell_behind_is_told_to_reread_rather_than_handed_a_hole() {
        let mut ed = fresh("");
        let seen = ed.buffer.change_count();
        for _ in 0..CHANGE_LIMIT + 1 {
            ed.apply(EditorCommand::InsertChar('x'));
        }
        assert_eq!(ed.buffer.changes_since(seen), None);
        // ...and the reader that kept up is still served.
        let caught_up = ed.buffer.change_count();
        ed.apply(EditorCommand::InsertChar('y'));
        assert_eq!(ed.buffer.changes_since(caught_up).map(<[_]>::len), Some(1));
    }

    /// A count from another buffer's life is not a slice of this log. Buffers
    /// are switched constantly, so this is the ordinary case rather than an
    /// error, and it takes the same road as an overflow.
    #[test]
    fn a_count_from_a_different_buffer_reads_as_reread() {
        let mut ed = fresh("abc");
        ed.apply(EditorCommand::InsertChar('z'));
        assert_eq!(ed.buffer.changes_since(ed.buffer.change_count() + 5), None);
    }

    /// Loading a file is a whole new document, and the reader has to be told —
    /// silently reusing a pristine buffer would otherwise leave a parser
    /// adjusting offsets into text that was never there.
    #[test]
    fn opening_a_file_records_the_document_it_replaced() {
        let mut ed = Editor::new();
        let seen = ed.buffer.change_count();
        ed.load("fn main() {}\n", Some(PathBuf::from("/tmp/x.rs")), Some("rust".into()));
        let log = ed.buffer.changes_since(seen).unwrap_or_default();
        assert_eq!(Change::coalesce(log).map(|c| c.new_end), Some(13));
    }

    /// The arithmetic in `coalesce`, on the cases that are not simply "widen
    /// the range": a later edit whose offsets are in coordinates the earlier
    /// one created, and one that deletes text the earlier one inserted.
    #[test]
    fn coalescing_translates_later_edits_back_into_the_original_coordinates() {
        // "abc": insert X at 1, then insert Y at 3 => "aXbYc". The unchanged
        // "a" and "c" must stay outside the record.
        let run = [
            Change { start: 1, old_end: 1, new_end: 2 },
            Change { start: 3, old_end: 3, new_end: 4 },
        ];
        assert_eq!(
            Change::coalesce(&run),
            Some(Change { start: 1, old_end: 2, new_end: 4 })
        );
        record_describes("abc", "aXbYc", Change::coalesce(&run).unwrap());

        // "abcdef": delete 1..3, then insert two chars at 0 => "ZZadef".
        let run = [
            Change { start: 1, old_end: 3, new_end: 1 },
            Change { start: 0, old_end: 0, new_end: 2 },
        ];
        assert_eq!(
            Change::coalesce(&run),
            Some(Change { start: 0, old_end: 3, new_end: 3 })
        );
        record_describes("abcdef", "ZZadef", Change::coalesce(&run).unwrap());

        // An insert entirely swallowed by a later delete: the record may not
        // reach back further than the text ever did.
        let run = [
            Change { start: 5, old_end: 5, new_end: 6 },
            Change { start: 0, old_end: 10, new_end: 0 },
        ];
        assert_eq!(
            Change::coalesce(&run),
            Some(Change { start: 0, old_end: 9, new_end: 0 })
        );

        assert_eq!(Change::coalesce(&[]), None);
    }
}
