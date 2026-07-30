//! The status buffer's text, and what each of its lines refers to.
//!
//! Pure: no git, no I/O, so the whole thing is cheap to test against a
//! hand-built [`Status`]. The layout follows Magit — a short header, then
//! untracked, unstaged and staged sections with counts in their headings, one
//! file per line behind its status letter.
//!
//! The load-bearing invariant is that [`render`]'s two returns have the same
//! number of entries: the UI indexes the line map by cursor line, so a drift of
//! one means `s` stages the wrong file. Every line goes through [`Buf::push`],
//! which is the only place either half is appended to.

use std::path::{Path, PathBuf};

use crate::{ChangeKind, FileChange, Status};

/// Which list of the status buffer a line belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Staged,
    Unstaged,
    Untracked,
}

/// One line of the status buffer, and what the user would be acting on there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// Branch, upstream, or in-progress operation.
    Header,
    /// A section heading; acting on it means acting on the whole section.
    Section(Section),
    File {
        path: PathBuf,
        section: Section,
    },
    /// ponytail: never emitted — staging is file-level for now. Kept so the UI
    /// can be written against the final shape of the map.
    Hunk {
        path: PathBuf,
        section: Section,
        index: usize,
    },
    Blank,
    /// Prose with nothing behind it, like "nothing to commit".
    Text,
}

/// Render `status` as buffer text plus one [`Line`] per line of that text.
///
/// `text.lines().count() == map.len()`, always.
pub fn render(status: &Status) -> (String, Vec<Line>) {
    let mut buf = Buf::default();

    let head = match &status.branch {
        Some(branch) => branch.clone(),
        None if status.detached => "(detached)".into(),
        None => "(unknown)".into(),
    };
    let suffix = if status.unborn {
        " (no commits yet)"
    } else {
        ""
    };
    buf.push(&format!("Head:     {head}{suffix}"), Line::Header);

    if let Some(upstream) = &status.upstream {
        let distance = match (status.ahead, status.behind) {
            (0, 0) => String::new(),
            (a, 0) => format!(" (ahead {a})"),
            (0, b) => format!(" (behind {b})"),
            (a, b) => format!(" (ahead {a}, behind {b})"),
        };
        buf.push(&format!("Upstream: {upstream}{distance}"), Line::Header);
    }

    if let Some(op) = status.in_progress {
        buf.push(
            &format!("State:    {} in progress", op.label()),
            Line::Header,
        );
    }

    let untracked: Vec<Row> = status
        .untracked
        .iter()
        .map(|path| Row {
            text: format!("? {}", show(path)),
            path: path.clone(),
        })
        .collect();

    section(&mut buf, Section::Untracked, "Untracked files", &untracked);
    section(
        &mut buf,
        Section::Unstaged,
        "Unstaged changes",
        &rows(&status.unstaged),
    );
    section(
        &mut buf,
        Section::Staged,
        "Staged changes",
        &rows(&status.staged),
    );

    if status.is_clean() {
        buf.push("", Line::Blank);
        buf.push("Nothing to commit, working tree clean", Line::Text);
    }

    (buf.text, buf.lines)
}

struct Row {
    text: String,
    path: PathBuf,
}

fn rows(changes: &[FileChange]) -> Vec<Row> {
    changes
        .iter()
        .map(|change| {
            let letter = change.status.letter();
            let text = match &change.status {
                // Both ends of a rename are worth seeing; the path the UI acts
                // on stays the new one, which is what `git add` wants.
                ChangeKind::Renamed { from } | ChangeKind::Copied { from } => {
                    format!("{letter} {} -> {}", show(from), show(&change.path))
                }
                _ => format!("{letter} {}", show(&change.path)),
            };
            Row {
                text,
                path: change.path.clone(),
            }
        })
        .collect()
}

fn section(buf: &mut Buf, section: Section, title: &str, rows: &[Row]) {
    if rows.is_empty() {
        return;
    }
    buf.push("", Line::Blank);
    buf.push(&format!("{title} ({})", rows.len()), Line::Section(section));
    for row in rows {
        buf.push(
            &row.text,
            Line::File {
                path: row.path.clone(),
                section,
            },
        );
    }
}

/// A path as display text. Control characters are folded to `?` because a
/// newline in a filename is legal on unix and would otherwise split one file
/// across two lines, sliding every entry in the map past it out of alignment.
fn show(path: &Path) -> String {
    path.display()
        .to_string()
        .replace(|c: char| c.is_control(), "?")
}

#[derive(Default)]
struct Buf {
    text: String,
    lines: Vec<Line>,
}

impl Buf {
    /// The only way to append, so text and map cannot drift apart.
    fn push(&mut self, text: &str, line: Line) {
        self.text.push_str(text);
        self.text.push('\n');
        self.lines.push(line);
    }
}
