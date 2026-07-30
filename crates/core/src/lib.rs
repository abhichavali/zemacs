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
pub mod evil;
pub mod frame;
pub mod minibuffer;

use std::collections::HashMap;
use std::path::PathBuf;

use ropey::Rope;

pub use dashboard::Dashboard;
pub use frame::{BufferId, Frame, Rect, Window, WindowId};
pub use minibuffer::{CompletionStyle, Prompt, PromptKind};

/// Editing mode — the heart of the modal ("Evil") feel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Normal,
    Insert,
    Visual,
    VisualLine,
    /// The startup screen. Its own mode because its keymap is entirely its own.
    Dashboard,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "V-LINE",
            Mode::Dashboard => "DASHBOARD",
        }
    }

    /// Name used by Lisp `define-key` and by keymap lookup.
    pub fn from_name(s: &str) -> Option<Mode> {
        match s.to_ascii_lowercase().as_str() {
            "normal" => Some(Mode::Normal),
            "insert" => Some(Mode::Insert),
            "visual" => Some(Mode::Visual),
            "visual-line" | "vline" => Some(Mode::VisualLine),
            "dashboard" => Some(Mode::Dashboard),
            _ => None,
        }
    }

    fn is_visual(self) -> bool {
        matches!(self, Mode::Visual | Mode::VisualLine)
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
    // ponytail: Enter is the only *named* key that needs modifiers today (the
    // window splits). Generalize to a modifier bitset over a `Named` enum when
    // a second one does.
    CtrlEnter,
    CtrlMetaEnter,
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
}

// --- Syntax highlighting types -------------------------------------------
// Defined here (not in zemacs-syntax) so the renderer can consume highlights
// without depending on tree-sitter.

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
}

impl HlKind {
    pub const ALL: [HlKind; 14] = [
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
    /// `"minibuffer"`, `"bottom"` (consult-like) or `"center"` (telescope-like).
    SetCompletionStyle(String),
    /// Negative sinks the modeline instead of raising it.
    SetModelineRelief(i32),
    SetModelinePad(i32),

    /// Names offered by `M-x`. The Lisp image publishes these at startup and
    /// after a config reload.
    ClearCommands,
    RegisterCommand(String),
    /// Index into [`Editor::buffer_names`]; 0 is the current buffer.
    SwitchBuffer(usize),

    // --- windows and frames ---
    /// Split the focused window; the new one shows the same buffer.
    SplitWindow(frame::Split),
    CloseWindow,
    FocusNextWindow,
    FocusWindow(frame::WindowId),
    /// A new OS window, opening on the dashboard.
    NewFrame,
    CloseFrame,

    // --- dashboard, configured from Lisp ---
    SetDashboardBanner(String),
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
    SaveFile(Option<PathBuf>),
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
        }
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
}

/// A text document plus a cursor expressed as a character index into the rope.
pub struct Buffer {
    /// Stable handle. Windows refer to buffers by this, never by index.
    pub id: BufferId,
    pub kind: BufferKind,
    pub text: Rope,
    pub cursor: usize,
    pub path: Option<PathBuf>,
    pub modified: bool,
    /// Language id for tree-sitter, e.g. `"rust"`, `"lisp"`. `None` = plain text.
    pub language: Option<String>,
    /// Scroll position, parked here while another buffer is on screen.
    pub saved_scroll: usize,
    /// Undo history lives with the buffer, not the editor: switching files
    /// must not let one buffer's `u` restore another's text.
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

impl Buffer {
    pub fn from_str(s: &str) -> Self {
        Self {
            id: 0,
            kind: BufferKind::Text,
            text: Rope::from_str(s),
            cursor: 0,
            path: None,
            modified: false,
            language: None,
            saved_scroll: 0,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    /// Display name for the status line and the buffer switcher.
    pub fn name(&self) -> String {
        match (&self.path, self.kind) {
            (Some(p), _) => p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string()),
            (None, BufferKind::Dashboard) => "*dashboard*".into(),
            (None, BufferKind::Scratch) => "*scratch*".into(),
            (None, BufferKind::Text) => "*untitled*".into(),
        }
    }

    /// True for a throwaway buffer that opening a file should replace rather
    /// than stack behind. `*dashboard*` and `*scratch*` are never throwaway,
    /// however empty they look — they are named buffers you can come back to.
    fn is_pristine(&self) -> bool {
        self.kind == BufferKind::Text
            && self.path.is_none()
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

    fn insert_char(&mut self, c: char) {
        self.text.insert_char(self.cursor.min(self.len_chars()), c);
        self.cursor += 1;
        self.modified = true;
    }

    fn insert_text(&mut self, s: &str) {
        self.text.insert(self.cursor.min(self.len_chars()), s);
        self.cursor += s.chars().count();
        self.modified = true;
    }

    fn delete_backward(&mut self) {
        if self.cursor > 0 {
            self.text.remove(self.cursor - 1..self.cursor);
            self.cursor -= 1;
            self.modified = true;
        }
    }

    fn delete_forward(&mut self) {
        let (line, col) = self.cursor_line_col();
        if col < self.line_len(line) {
            self.text.remove(self.cursor..self.cursor + 1);
            self.modified = true;
        }
    }

    fn delete_range(&mut self, start: usize, end: usize) {
        let n = self.len_chars();
        let (start, end) = (start.min(n), end.min(n));
        if start >= end {
            return;
        }
        self.text.remove(start..end);
        self.cursor = start.min(self.len_chars());
        self.modified = true;
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
    /// Command names published by the Lisp image, for `M-x` completion.
    pub commands: Vec<String>,
    pub mode: Mode,
    pub settings: Settings,
    pub theme: Theme,
    pub dashboard: Dashboard,

    /// User keymap: (mode, "g d") -> Lisp function name. Consulted before the
    /// built-in Evil grammar, which is how Lisp config wins.
    pub keymap: HashMap<(Mode, String), String>,

    /// `Some` while the `:` or `/` prompt is active.
    pub prompt: Option<Prompt>,
    /// Last message, shown in the status line.
    pub status: String,
    pub should_quit: bool,

    /// Highlight spans for the current buffer, recomputed by the app.
    pub highlights: Vec<Span>,
    /// Bumped on every document mutation; the app re-highlights when it moves.
    pub revision: u64,

    /// First visible line, and how many lines fit (set by the renderer).
    pub scroll: usize,
    pub viewport_lines: usize,

    /// Evil pending state: counts, operators, multi-key prefixes.
    pub(crate) pending: evil::Pending,
    /// Anchor of the visual selection.
    pub(crate) visual_anchor: Option<usize>,

    register: String,
    register_linewise: bool,
    last_search: String,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

const UNDO_LIMIT: usize = 500;

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
            frames: vec![frame::Frame::new(0)],
            focus_frame: 0,
            next_buffer_id: 2,
            commands: Vec::new(),
            mode: Mode::Dashboard,
            settings: Settings::default(),
            theme: Theme::default(),
            dashboard: Dashboard::default(),
            keymap: HashMap::new(),
            prompt: None,
            status: String::from("zemacs — Common Lisp inside."),
            should_quit: false,
            highlights: Vec::new(),
            revision: 0,
            scroll: 0,
            viewport_lines: 24,
            pending: evil::Pending::default(),
            visual_anchor: None,
            register: String::new(),
            register_linewise: false,
            last_search: String::new(),
        }
    }

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

    /// Park the live cursor and scroll on the focused window. Must run before
    /// anything changes which window is focused.
    fn sync_window(&mut self) {
        let (cursor, scroll, lines, id) = (
            self.buffer.cursor,
            self.scroll,
            self.viewport_lines,
            self.buffer.id,
        );
        let w = self.frame_mut().current_window_mut();
        w.cursor = cursor;
        w.scroll = scroll;
        w.viewport_lines = lines;
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
        self.highlights.clear();
        self.revision += 1;
        self.mode = match self.buffer.kind {
            BufferKind::Dashboard => Mode::Dashboard,
            _ => Mode::Normal,
        };
    }

    /// Open buffer names, active first — the candidate list for the switcher.
    pub fn buffer_names(&self) -> Vec<String> {
        std::iter::once(&self.buffer)
            .chain(self.others.iter())
            .map(|b| {
                let mark = if b.modified { " [+]" } else { "" };
                format!("{}{mark}", b.name())
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
        self.highlights.clear();
        self.revision += 1;
        self.status = format!("switched to {}", self.buffer.name());
        self.mode = match self.buffer.kind {
            BufferKind::Dashboard => Mode::Dashboard,
            _ => Mode::Normal,
        };
        // The window now shows this buffer — otherwise the next focus change
        // would swap the old one straight back in.
        let (id, cursor, scroll) = (self.buffer.id, self.buffer.cursor, self.scroll);
        let w = self.frame_mut().current_window_mut();
        w.buffer = id;
        w.cursor = cursor;
        w.scroll = scroll;
    }

    /// The one and only document mutator.
    pub fn apply(&mut self, cmd: EditorCommand) {
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
                let n = self.register.chars().count();
                self.status = format!("yanked {n} chars");
            }
            EditorCommand::Paste { after } => self.paste(after),
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
            EditorCommand::Message(m) => self.status = m,
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
            EditorCommand::ClearCommands => self.commands.clear(),
            EditorCommand::RegisterCommand(name) => {
                if !self.commands.contains(&name) {
                    self.commands.push(name);
                }
            }
            EditorCommand::SwitchBuffer(i) => self.switch_buffer(i),
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
                None => self.status = format!("unknown mode in define-key: {mode}"),
            },
            // The app intercepts these; reaching `apply` means nothing is listening.
            EditorCommand::CallLisp(name) => {
                self.status = format!("no Lisp runtime to call {name}")
            }
            EditorCommand::OpenFile(p) => self.status = format!("cannot open {}", p.display()),
            EditorCommand::SaveFile(_) => self.status = "cannot save: no file backend".into(),
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
        self.buffer.kind = BufferKind::Text;
        self.highlights.clear();
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
            Mode::Visual | Mode::VisualLine if !self.mode.is_visual() => {
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
    }

    /// The inclusive char range covered by the visual selection, if any.
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

    /// In Normal mode the cursor sits *on* a character, never past the last one.
    fn clamp_cursor(&mut self) {
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
    if s.contains(' ') || s.contains('-') {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        s.chars().map(|c| c.to_string()).collect::<Vec<_>>().join(" ")
    }
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
    }

    #[test]
    fn selection_is_inclusive_of_cursor_char() {
        let mut ed = fresh("abcdef");
        feed(&mut ed, &[Key::Char('v'), Key::Char('l')]);
        assert_eq!(ed.selection(), Some((0, 2)));
    }
}
