//! What the modeline says.
//!
//! A list of [`Segment`]s rather than one formatted string, because the parts
//! want different weights and colours — the mode indicator bold and tinted, the
//! file name bold, the permissions dim — and a single string can only ever be
//! drawn one way.
//!
//! Split left and right so position and mode sit against the right edge the way
//! Emacs puts them, instead of drifting with the length of the file name.

use crate::{Buffer, Editor, HlKind, Mode};

/// A run of modeline text drawn as one piece.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub text: String,
    pub bold: bool,
    /// `None` takes the modeline's own foreground.
    pub face: Option<HlKind>,
}

impl Segment {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            bold: false,
            face: None,
        }
    }

    fn bold(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            bold: true,
            face: None,
        }
    }

    fn faced(text: impl Into<String>, face: HlKind, bold: bool) -> Self {
        Self {
            text: text.into(),
            bold,
            face: Some(face),
        }
    }
}

/// The modeline for one pane: the segments that hug the left edge, and the ones
/// that hug the right.
pub fn segments(editor: &Editor, buf: &Buffer, active: bool) -> (Vec<Segment>, Vec<Segment>) {
    let (line, col) = buf.cursor_line_col();
    let lines = buf.len_lines().max(1);

    let mut left = Vec::new();
    if active {
        // The modal state, in the colour that state is drawn in elsewhere, so
        // the eye can read the mode without reading the word.
        let face = match editor.mode {
            Mode::Insert => HlKind::String,
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock => HlKind::Keyword,
            Mode::Terminal => HlKind::Function,
            Mode::Magit | Mode::Dired | Mode::Dashboard => HlKind::Type,
            Mode::Normal => HlKind::Constant,
        };
        left.push(Segment::faced(
            format!(" {} ", editor.mode.label()),
            face,
            true,
        ));
    }

    left.push(Segment::plain("  "));
    left.push(Segment::bold(buf.name()));
    // Modified beats read-only: a generated buffer cannot be modified, so the
    // two never compete, and showing "unsaved" matters more than showing why a
    // buffer cannot be saved.
    //
    // A buffer a mode has frozen *can* have been modified before it was frozen,
    // which is the one case where they do compete — and the dot still wins, for
    // the same reason: unsaved work is the more urgent fact.
    if buf.modified {
        left.push(Segment::faced(" ●", HlKind::Bold, false));
    } else if buf.read_only() != crate::ReadOnly::No {
        left.push(Segment::faced(" ◈", HlKind::Comment, false));
    }

    // The message the last command produced, and the half-typed key sequence.
    // Both belong on the left, after the file, because both are transient and
    // pushing the position around as they come and go is exactly what the
    // right-hand group avoids.
    if active {
        let pending = editor.pending_hint();
        if !editor.status.is_empty() {
            left.push(Segment::plain("  "));
            left.push(Segment::faced(&editor.status, HlKind::Comment, false));
        }
        if !pending.is_empty() {
            left.push(Segment::plain("  "));
            left.push(Segment::faced(pending, HlKind::Constant, true));
        }
    }

    let mut right = Vec::new();
    if let Some(mode) = buf.file_mode {
        right.push(Segment::faced(permissions(mode), HlKind::Comment, false));
        right.push(Segment::plain("  "));
    }
    right.push(Segment::faced(major_mode_label(buf), HlKind::Type, false));
    right.push(Segment::plain("  "));
    for m in &buf.minor_modes {
        right.push(Segment::faced(format!("+{m} "), HlKind::Comment, false));
    }
    right.push(Segment::bold(format!("{}:{}", line + 1, col + 1)));
    right.push(Segment::plain("  "));
    right.push(Segment::faced(scroll_label(line, lines), HlKind::Number, false));
    right.push(Segment::plain(" "));
    (left, right)
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

    #[test]
    fn an_unsaved_buffer_is_marked_and_a_generated_one_is_not() {
        let mut ed = Editor::new();
        ed.load("hello", None, None);
        ed.apply(EditorCommand::SetMode(Mode::Normal));
        let joined = |ed: &Editor| {
            let (left, _) = segments(ed, &ed.buffer, true);
            left.iter().map(|s| s.text.clone()).collect::<String>()
        };
        assert!(!joined(&ed).contains('●'));
        ed.apply(EditorCommand::InsertChar('x'));
        assert!(joined(&ed).contains('●'), "an edited buffer says so");

        ed.show_special(BufferKind::Dired, "listing");
        let text = joined(&ed);
        assert!(text.contains('◈') && !text.contains('●'));
    }

    /// The mode indicator is the first thing on the strip and is bold, so the
    /// state is readable at a glance rather than by reading a word.
    #[test]
    fn the_modal_state_leads_and_is_emphasised() {
        let mut ed = Editor::new();
        ed.load("hello", None, None);
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        let (left, right) = segments(&ed, &ed.buffer, true);
        assert!(left[0].text.contains("INSERT"));
        assert!(left[0].bold);
        assert_eq!(left[0].face, Some(HlKind::String));

        // ...and an inactive pane drops it entirely: only one pane has a mode.
        let (left, _) = segments(&ed, &ed.buffer, false);
        assert!(!left.iter().any(|s| s.text.contains("INSERT")));

        // Position rides on the right, so it does not move when the file name
        // or the status message changes length.
        assert!(right.iter().any(|s| s.text == "1:1"));
    }

    #[test]
    fn permissions_appear_only_when_there_is_a_file() {
        let mut ed = Editor::new();
        ed.load("hello", None, None);
        let has_perms = |ed: &Editor| {
            let (_, right) = segments(ed, &ed.buffer, true);
            right.iter().any(|s| s.text.len() == 9 && s.text.contains('r'))
        };
        assert!(!has_perms(&ed), "a buffer with no file has no mode bits");
        ed.buffer.file_mode = Some(0o644);
        assert!(has_perms(&ed));
    }
}
