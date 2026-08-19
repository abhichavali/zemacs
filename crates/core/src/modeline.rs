//! What the modeline says.
//!
//! **The shape is Lisp's and the expansion is Rust's**, which is rule 5 of
//! `docs/boundary.org` — *"is it hot enough that Lisp doing it per keystroke
//! would be felt? Then it is Rust, and Lisp gets a knob rather than a hook"* —
//! applied to the one strip on screen that every config wants to bend.
//!
//! This file used to *be* the modeline: a function that pushed a pill, then two
//! spaces, then the buffer name, then a dot if modified, in that order, in those
//! colours. Everything about that was policy and none of it was reachable. It is
//! now a list of [`Spec`]s the image sets once with `modeline-segment`, each a
//! little template of `%` codes; [`segments`] expands them per pane per frame,
//! which is a scan of a few dozen bytes and no Lisp call at all.
//!
//! A callback per frame was the other design and is the one Emacs has. It is
//! also why a slow `mode-line-format` in Emacs makes the whole editor feel slow,
//! and `docs/threading.org` is a document about never doing that.
//!
//! # The codes
//!
//! | Code | Expands to                                          |
//! |------|-----------------------------------------------------|
//! | `%m` | The modal state — `NORMAL`, `INSERT`. Active pane only. |
//! | `%b` | The buffer's name.                                  |
//! | `%f` | Its path, or the name again when it has none.       |
//! | `%+` | `●` when the buffer has unsaved changes.            |
//! | `%r` | `◈` when it is read-only *and* unmodified.          |
//! | `%s` | The last message. Active pane only.                 |
//! | `%k` | The half-typed key sequence. Active pane only.      |
//! | `%P` | Unix permissions, when there is a file behind it.   |
//! | `%M` | The major mode, as a word: `Rust`.                  |
//! | `%n` | The minor modes, `+each` in turn.                   |
//! | `%l` | The line number, 1-based.                           |
//! | `%c` | The column, 1-based.                                |
//! | `%p` | Where you are: `Top`, `Bot`, `All`, or a percentage. |
//! | `%%` | A literal `%`.                                      |
//!
//! **A segment whose codes all expand to nothing is dropped whole**, and that is
//! the whole of the conditional logic — it is what lets `"  %P"` put two spaces
//! before the permissions *and* disappear with them when the buffer has no file.
//! Without it a format language needs `if`, and a format language with `if` in it
//! is a programming language written in strings.

use crate::{Buffer, Editor, HlKind, Mode};

/// Which colour a segment takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Face {
    /// The strip's own foreground.
    #[default]
    Default,
    Named(HlKind),
    /// *The colour of the mode you are in* — the one face that cannot be named
    /// ahead of time, since it is a different one in each state.
    ///
    /// This is what makes the pill work. Modal editing's one recurring cost is
    /// losing track of which mode you are in, and the thing that fixes it is a
    /// shape at the left edge that is a different colour in each — recognisable
    /// at the edge of vision, which a word is not.
    Mode,
}

// ponytail: which face each mode takes is the table below and is not settable —
// a config can move the pill, restyle it or delete it, but not recolour INSERT.
// Ceiling: someone who wants visual and insert swapped. Upgrade path: a
// `modeline-mode-face` verb keyed by the state name `set-evil-state` already
// takes.
fn mode_face(mode: Mode) -> HlKind {
    match mode {
        Mode::Insert => HlKind::String,
        Mode::Visual | Mode::VisualLine | Mode::VisualBlock => HlKind::Keyword,
        Mode::Terminal => HlKind::Function,
        Mode::Magit | Mode::Dired | Mode::Dashboard => HlKind::Type,
        Mode::Normal => HlKind::Constant,
    }
}

/// One entry of the format: a template and how to draw what it expands to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    /// Literal text with `%` codes in it — see the module docs.
    pub template: String,
    pub face: Face,
    pub bold: bool,
    /// Paint the face as a *block* behind the text instead of on it, and knock
    /// the text out to the strip's own colour.
    ///
    /// A second boolean rather than a `fill: Option<HlKind>`, because the two
    /// would never differ: a filled segment is the same claim as a coloured one
    /// — "this is what mode you are in" — stated loudly. Two faces would be two
    /// things for a theme to keep in agreement and no way to be in disagreement
    /// usefully.
    ///
    /// Knocked out rather than drawn over: a colour chosen to be legible *on*
    /// the bar is by construction not legible *as* the bar, so the text has to
    /// swap to the ground it is now sitting on. That ground is the strip, which
    /// is why the renderer resolves it and this file does not.
    pub filled: bool,
}

impl Spec {
    /// A plain run of literal text.
    pub fn text(template: &str) -> Self {
        Self {
            template: template.into(),
            face: Face::Default,
            bold: false,
            filled: false,
        }
    }
}

/// The whole strip, in two groups.
///
/// Split left and right so position and mode sit against the right edge the way
/// Emacs puts them, instead of drifting with the length of the file name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Format {
    pub left: Vec<Spec>,
    pub right: Vec<Spec>,
}

/// What a modeline nobody has configured says.
///
/// Deliberately almost nothing — the shipped strip is `runtime/init.lisp`'s, and
/// this is what a headless [`Editor`] or a checkout with a broken config shows.
/// The two facts worth having with no config at all are which buffer you are
/// looking at and whether you have saved it.
impl Default for Format {
    fn default() -> Self {
        Self {
            left: vec![
                Spec {
                    template: "  %b".into(),
                    face: Face::Default,
                    bold: true,
                    filled: false,
                },
                Spec {
                    template: " %+%r".into(),
                    face: Face::Named(HlKind::Warning),
                    bold: false,
                    filled: false,
                },
            ],
            right: vec![Spec::text("%l:%c  ")],
        }
    }
}

impl Format {
    /// Take both sides down, so `modeline-segment` can build a strip from
    /// scratch rather than appending to whatever was there.
    pub fn clear(&mut self) {
        self.left.clear();
        self.right.clear();
    }

    pub fn push(&mut self, right: bool, spec: Spec) {
        match right {
            true => self.right.push(spec),
            false => self.left.push(spec),
        }
    }
}

/// One expanded segment, ready to draw.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub text: String,
    pub bold: bool,
    /// `None` takes the modeline's own foreground.
    pub face: Option<HlKind>,
    pub filled: bool,
}

/// The modeline for one pane: the segments that hug the left edge, and the ones
/// that hug the right.
pub fn segments(editor: &Editor, buf: &Buffer, active: bool) -> (Vec<Segment>, Vec<Segment>) {
    let (left, right) = drawn(editor, buf, active);
    let drop = |v: Vec<(&str, Segment)>| v.into_iter().map(|(_, s)| s).collect();
    (drop(left), drop(right))
}

/// [`segments`], each one still paired with the template it came from.
///
/// The template is what a *click* on the strip has to answer with: the text is
/// whatever the pane happened to be showing — `Rust`, `12:4`, a bullet — and
/// nothing downstream could tell one `●` from another, while `" %+"` says which
/// segment was hit whatever it expanded to that frame. A borrow rather than a
/// copy, so the draw loop pays nothing for a question only the mouse asks.
#[allow(clippy::type_complexity)]
pub fn drawn<'a>(
    editor: &'a Editor,
    buf: &Buffer,
    active: bool,
) -> (Vec<(&'a str, Segment)>, Vec<(&'a str, Segment)>) {
    let fields = Fields::of(editor, buf, active);
    let expand = |specs: &'a [Spec]| -> Vec<(&'a str, Segment)> {
        specs
            .iter()
            .filter_map(|s| {
                let text = fields.expand(&s.template)?;
                Some((
                    s.template.as_str(),
                    Segment {
                        text,
                        bold: s.bold,
                        face: match s.face {
                            Face::Default => None,
                            Face::Named(k) => Some(k),
                            Face::Mode => Some(mode_face(editor.mode)),
                        },
                        filled: s.filled,
                    },
                ))
            })
            .collect()
    };
    (
        expand(&editor.modeline.left),
        expand(&editor.modeline.right),
    )
}

/// Everything a code can name, gathered once per pane rather than once per
/// segment — `%l` and `%c` come off the same cursor lookup, and a format is
/// free to mention either of them twice.
struct Fields<'a> {
    editor: &'a Editor,
    buf: &'a Buffer,
    active: bool,
    line: usize,
    col: usize,
    lines: usize,
}

impl<'a> Fields<'a> {
    fn of(editor: &'a Editor, buf: &'a Buffer, active: bool) -> Self {
        let (line, col) = buf.cursor_line_col();
        Self {
            editor,
            buf,
            active,
            line,
            col,
            lines: buf.len_lines().max(1),
        }
    }

    /// What one code stands for. `""` means "nothing to say", which is what
    /// [`Fields::expand`] counts to decide whether a segment survives.
    fn code(&self, c: char) -> Option<String> {
        // Everything about the *focused window* is blank in an inactive pane:
        // the mode, the messages and the half-typed key describe a window this
        // pane is not, and repeating them in every pane is the same lie N times.
        let live = |s: String| match self.active {
            true => s,
            false => String::new(),
        };
        Some(match c {
            'm' => live(self.editor.mode.label().to_string()),
            's' => live(self.editor.status.clone()),
            // A mode's standing note. `live` like the message beside it: it
            // describes work the *focused* window's editor is doing, and one
            // note repeated down every pane is the same sentence N times.
            'N' => live(self.editor.modeline_note.clone()),
            'k' => live(self.editor.pending_hint().to_string()),
            'b' => self.buf.name(),
            'f' => match &self.buf.path {
                Some(p) => p.display().to_string(),
                None => self.buf.name(),
            },
            // Modified beats read-only: a generated buffer cannot be modified,
            // so the two rarely compete, and a buffer a mode froze *can* have
            // been edited before it was frozen — which is the case where they
            // do, and unsaved work is the more urgent fact.
            '+' => match self.buf.modified {
                true => "●".into(),
                false => String::new(),
            },
            'r' => match !self.buf.modified && self.buf.read_only() != crate::ReadOnly::No {
                true => "◈".into(),
                false => String::new(),
            },
            'P' => self.buf.file_mode.map(permissions).unwrap_or_default(),
            'M' => major_mode_label(self.buf),
            'n' => self.buf.minor_modes.iter().map(|m| format!("+{m} ")).collect(),
            'l' => (self.line + 1).to_string(),
            'c' => (self.col + 1).to_string(),
            'p' => scroll_label(self.line, self.lines),
            _ => return None,
        })
    }

    /// Expand one template, or `None` when the whole segment should be dropped.
    ///
    /// Dropped when it mentioned at least one code and every code it mentioned
    /// came back empty — see the module docs. A template of pure literal text is
    /// never dropped, which is what keeps a separator a separator.
    fn expand(&self, template: &str) -> Option<String> {
        let mut out = String::with_capacity(template.len());
        let mut chars = template.chars();
        let (mut codes, mut filled) = (0usize, 0usize);
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                None => out.push('%'),
                Some('%') => out.push('%'),
                Some(k) => match self.code(k) {
                    Some(v) => {
                        codes += 1;
                        filled += usize::from(!v.is_empty());
                        out.push_str(&v);
                    }
                    // An unknown code stays as it was typed. A modeline reading
                    // `%z` is how someone finds out they invented a code; a
                    // modeline that silently ate it is how they file a bug about
                    // a missing field.
                    None => {
                        out.push('%');
                        out.push(k);
                    }
                },
            }
        }
        match codes > 0 && filled == 0 {
            true => None,
            false => Some(out),
        }
    }
}

/// `rust-mode` reads better as `Rust` on a strip this narrow, and the `-mode`
/// suffix is the same on every one of them.
pub(crate) fn major_mode_label(buf: &Buffer) -> String {
    let name = buf.major_mode.strip_suffix("-mode").unwrap_or(&buf.major_mode);
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => name.to_string(),
    }
}

/// Where the cursor is in the file, as Emacs writes it: `Top`, `Bot`, `All`
/// when the whole thing fits, and a percentage in between. A number that reads
/// as a position rather than as arithmetic.
fn scroll_label(line: usize, lines: usize) -> String {
    if lines <= 1 {
        return "All".into();
    }
    match line {
        0 => "Top".into(),
        l if l + 1 >= lines => "Bot".into(),
        l => format!("{}%", l * 100 / (lines - 1)),
    }
}

/// Unix mode bits as `ls -l` writes them, minus the leading file-type char —
/// the buffer name already says which file this is.
pub fn permissions(mode: u32) -> String {
    const FLAGS: [(u32, char); 9] = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    let mut out: String = FLAGS
        .iter()
        .map(|&(bit, c)| if mode & bit != 0 { c } else { '-' })
        .collect();

    // setuid, setgid and the sticky bit replace the matching execute slot, as
    // `ls` does — an `s` where an `x` would be, uppercase when the execute bit
    // underneath is *not* set, which is the distinction that actually matters.
    for (bit, at, lower, upper) in [
        (0o4000u32, 2usize, 's', 'S'),
        (0o2000, 5, 's', 'S'),
        (0o1000, 8, 't', 'T'),
    ] {
        if mode & bit != 0 {
            let executable = out.as_bytes()[at] == b'x';
            out.replace_range(at..at + 1, &(if executable { lower } else { upper }).to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BufferKind, EditorCommand};

    /// The shipped strip lives in `runtime/init.lisp`, so the tests that are
    /// about *it* are in `crates/lisp/tests/modeline.rs` where the image can be
    /// asked. What is here is the expander those specs are fed to.
    fn joined(ed: &Editor, active: bool) -> String {
        let (left, right) = segments(ed, &ed.buffer, active);
        left.iter()
            .chain(right.iter())
            .map(|s| s.text.clone())
            .collect()
    }

    fn only(ed: &Editor, template: &str) -> Option<String> {
        Fields::of(ed, &ed.buffer, true).expand(template)
    }

    #[test]
    fn permissions_read_the_way_ls_writes_them() {
        assert_eq!(permissions(0o644), "rw-r--r--");
        assert_eq!(permissions(0o755), "rwxr-xr-x");
        assert_eq!(permissions(0o600), "rw-------");
        assert_eq!(permissions(0o777), "rwxrwxrwx");
        assert_eq!(permissions(0o000), "---------");
        // setuid over an execute bit is a lowercase `s`; without one it is
        // uppercase, which is how `ls` says "this bit is set but inert".
        assert_eq!(permissions(0o4755), "rwsr-xr-x");
        assert_eq!(permissions(0o4644), "rwSr--r--");
        assert_eq!(permissions(0o2755), "rwxr-sr-x");
        assert_eq!(permissions(0o1777), "rwxrwxrwt");
    }

    #[test]
    fn position_reads_as_a_place_not_a_calculation() {
        assert_eq!(scroll_label(0, 1), "All"); // nothing to scroll
        assert_eq!(scroll_label(0, 100), "Top");
        assert_eq!(scroll_label(99, 100), "Bot");
        assert_eq!(scroll_label(50, 101), "50%");
    }

    #[test]
    fn the_major_mode_loses_its_suffix() {
        let mut buf = Buffer::from_str("");
        buf.major_mode = "rust-mode".into();
        assert_eq!(major_mode_label(&buf), "Rust");
        buf.major_mode = "fundamental-mode".into();
        assert_eq!(major_mode_label(&buf), "Fundamental");
    }

    /// The whole of the conditional logic, and the reason the format needs no
    /// `if`: a segment carries its own separators and leaves with them.
    #[test]
    fn a_segment_whose_codes_all_came_back_empty_is_dropped() {
        let mut ed = Editor::new();
        ed.load("hello", None, None);

        // No file behind the buffer, so no mode bits — and the two spaces that
        // would have separated them go too.
        assert_eq!(only(&ed, "  %P"), None);
        ed.buffer.file_mode = Some(0o644);
        assert_eq!(only(&ed, "  %P").as_deref(), Some("  rw-r--r--"));

        // One code answering is enough to keep the rest of the segment.
        assert_eq!(only(&ed, "%+%P").as_deref(), Some("rw-r--r--"));

        // Literal text is never dropped: a separator with nothing to separate is
        // still what the author wrote.
        assert_eq!(only(&ed, "  ").as_deref(), Some("  "));
        // ...and `%%` is an escape rather than a code, so it does not keep a
        // segment alive on its own.
        ed.buffer.file_mode = None;
        assert_eq!(only(&ed, "%%%P"), None);
        assert_eq!(only(&ed, "%%").as_deref(), Some("%"));
    }

    #[test]
    fn an_unknown_code_survives_as_itself() {
        let ed = Editor::new();
        assert_eq!(only(&ed, "%z").as_deref(), Some("%z"));
        // ...and a trailing `%` is a `%`, not a panic.
        assert_eq!(only(&ed, "100%").as_deref(), Some("100%"));
    }

    #[test]
    fn an_unsaved_buffer_is_marked_and_a_generated_one_is_not() {
        let mut ed = Editor::new();
        ed.load("hello", None, None);
        ed.apply(EditorCommand::SetMode(Mode::Normal));
        assert!(!joined(&ed, true).contains('●'));
        ed.apply(EditorCommand::InsertChar('x'));
        assert!(joined(&ed, true).contains('●'), "an edited buffer says so");

        ed.show_special(BufferKind::Dired, "listing");
        let text = joined(&ed, true);
        assert!(text.contains('◈') && !text.contains('●'), "{text}");
    }

    /// Everything about the focused window is blank in a pane that is not it.
    #[test]
    fn an_inactive_pane_says_nothing_about_the_window_it_is_not() {
        let mut ed = Editor::new();
        ed.load("hello", None, None);
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.status = "saved".into();
        assert_eq!(only(&ed, "%m").as_deref(), Some("INSERT"));
        assert_eq!(only(&ed, "%s").as_deref(), Some("saved"));

        let f = Fields::of(&ed, &ed.buffer, false);
        assert_eq!(f.expand("%m"), None);
        assert_eq!(f.expand("%s"), None);
        // ...but the buffer's own facts are the pane's own and stay.
        assert_eq!(f.expand("%b").as_deref(), Some(ed.buffer.name().as_str()));
    }

    /// The mode's colour is the one face a format cannot name, because it is a
    /// different one in each state.
    #[test]
    fn the_mode_face_follows_the_mode() {
        let mut ed = Editor::new();
        ed.load("hello", None, None);
        ed.modeline.clear();
        ed.modeline.push(
            false,
            Spec {
                template: " %m ".into(),
                face: Face::Mode,
                bold: true,
                filled: true,
            },
        );
        let face = |ed: &Editor| segments(ed, &ed.buffer, true).0[0].face;
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        assert_eq!(face(&ed), Some(HlKind::String));
        ed.apply(EditorCommand::SetMode(Mode::Normal));
        assert_eq!(face(&ed), Some(HlKind::Constant));
        // ...and the pill leaves with the mode when the pane is not the focused
        // one, because `%m` is the only code in it.
        assert!(segments(&ed, &ed.buffer, false).0.is_empty());
    }

    /// A modeline nobody configured still says which buffer you are in and
    /// whether it is saved — a headless editor, or a config that failed to load.
    #[test]
    fn the_default_format_names_the_buffer_and_its_state() {
        let mut ed = Editor::new();
        ed.load("hello", None, None);
        ed.apply(EditorCommand::InsertChar('x'));
        let text = joined(&ed, true);
        assert!(text.contains(&ed.buffer.name()), "{text}");
        assert!(text.contains('●'), "{text}");
        assert!(text.contains("1:2"), "{text}");
    }
}
