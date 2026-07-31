//! The status buffer's text, and what each of its lines refers to.
//!
//! Pure: no git, no I/O, so the whole thing is cheap to test against a
//! hand-built [`View`]. The layout follows Magit — a short header, then the
//! rebase todo if one is running, untracked, unstaged and staged changes, the
//! stash, and a short log. Each section counts itself in its heading and folds
//! shut on its own.
//!
//! The load-bearing invariant is that [`render`]'s line map has one entry per
//! line of its text: the UI indexes the map by cursor line, so a drift of one
//! means `s` stages the wrong file. Every line ends through [`Buf::end`], which
//! is the only place the map is appended to. That invariant is why a hunk's body
//! is split here rather than handed to the UI as a blob, and why a control
//! character in a path or a diff line is folded to `?` — a newline in a filename
//! is legal on unix and would otherwise slide every entry after it.
//!
//! Folding is subtraction, not marking: a shut section's rows are simply not
//! emitted, so there are no hidden lines for the map to have to describe. The
//! UI re-renders after every fold, which it does after every command anyway.
//!
//! The third return is the colour: a [`Span`] per run that is not plain
//! foreground. The status buffer defines no faces of its own — it borrows the
//! syntax faces a theme already sets, so which side of the index a file sits on
//! is carried by `string` against `keyword` and there is no second palette to
//! keep in step with the first.

use std::path::{Path, PathBuf};

use crate::{ChangeKind, Commit, FileChange, FileDiff, Rebase, TodoItem, View};

/// Which list of the status buffer a line belongs to, and the unit a fold works
/// on. Doubles as "which side of the index" for the sections that have one:
/// only [`Section::Staged`] and [`Section::Unstaged`] carry diffs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Staged,
    Unstaged,
    Untracked,
    /// What is left of the rebase in progress.
    Rebase,
    Stashes,
    /// The short log at the bottom.
    Recent,
}

/// One line of the status buffer, and what the user would be acting on there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// Branch, upstream, or in-progress operation.
    Header,
    /// A section heading; acting on it means acting on the whole section, and
    /// folding on it folds that section.
    Section(Section),
    File {
        path: PathBuf,
        section: Section,
    },
    /// Any line of a hunk's body, its `@@` header included: staging from
    /// anywhere inside a hunk stages that hunk. `index` indexes
    /// [`FileDiff::hunks`] of the diff [`View::diff_of`] returns for
    /// `(section, path)`.
    Hunk {
        path: PathBuf,
        section: Section,
        index: usize,
    },
    /// A commit in the log. The hash is abbreviated and only means anything in
    /// this repository, which is the only place it will be used.
    Commit {
        hash: String,
    },
    /// A stash entry; the name is `stash@{n}`, good until the next refresh.
    Stash {
        name: String,
    },
    /// A remaining step of the rebase in progress: an index into
    /// [`Rebase::todo`].
    Todo {
        index: usize,
    },
    Blank,
    /// Prose with nothing behind it, like "nothing to commit".
    Text,
}

/// A face for one run of the status buffer, named after the [`crate`]-external
/// highlight class it stands for. Not that enum itself: this crate does not
/// depend on the editor core, and does not want to — the whole of the coupling
/// is the caller's match from these names onto its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Face {
    /// The branch HEAD is on.
    Heading1,
    /// A staged file, the heading over the staged section, and an added line in
    /// a diff — everything that is on its way into the next commit.
    String,
    /// An unstaged one, and a removed line in a diff.
    Keyword,
    /// The upstream branch — a pointer somewhere else.
    Link,
    /// Counts: how many files in a section, how far ahead of the upstream, how
    /// far through a rebase.
    Number,
    /// Untracked files, field labels, the old half of a rename, and the log.
    Comment,
    /// The status letter in the left column, and a commit hash.
    Constant,
    /// A conflict, and a half-finished merge or rebase — the two states that
    /// want the eye before anything else on the screen does.
    Bold,
    /// The `->` of a rename, a hunk's `@@` header, and the mark on a folded
    /// section: the punctuation of the buffer rather than its content.
    Punctuation,
}

/// A coloured run of the rendered text, in **char** offsets.
///
/// Chars rather than bytes because git tracks paths as bytes and `café.rs` is
/// seven characters and eight of them; a byte offset handed to a renderer that
/// counts characters would colour the wrong part of the next line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: Face,
}

/// Render `view` as buffer text, one [`Line`] per line of that text, and the
/// [`Span`]s that colour it.
///
/// `text.lines().count() == map.len()`, always. Spans are sorted by `start` and
/// never overlap; a run that is plain foreground gets none.
pub fn render(view: &View) -> (String, Vec<Line>, Vec<Span>) {
    let status = &view.status;
    let mut buf = Buf::default();

    let head = match &status.branch {
        Some(branch) => branch.clone(),
        None if status.detached => "(detached)".into(),
        None => "(unknown)".into(),
    };
    buf.face("Head:", Face::Comment);
    buf.put("     ");
    buf.face(&head, Face::Heading1);
    if status.unborn {
        buf.face(" (no commits yet)", Face::Comment);
    }
    buf.end(Line::Header);

    if let Some(upstream) = &status.upstream {
        buf.face("Upstream:", Face::Comment);
        buf.put(" ");
        buf.face(upstream, Face::Link);
        let distance = match (status.ahead, status.behind) {
            (0, 0) => String::new(),
            (a, 0) => format!("(ahead {a})"),
            (0, b) => format!("(behind {b})"),
            (a, b) => format!("(ahead {a}, behind {b})"),
        };
        if !distance.is_empty() {
            buf.put(" ");
            buf.face(&distance, Face::Number);
        }
        buf.end(Line::Header);
    }

    if let Some(op) = status.in_progress {
        buf.face("State:", Face::Comment);
        buf.put("    ");
        buf.face(&format!("{} in progress", op.label()), Face::Bold);
        buf.end(Line::Header);
    }

    if let Some(rebase) = &status.rebase {
        rebase_header(&mut buf, rebase);
    }

    let untracked: Vec<Row> = status
        .untracked
        .iter()
        .map(|path| Row {
            letter: '?',
            from: None,
            conflicted: false,
            path: path.clone(),
        })
        .collect();

    if let Some(rebase) = &status.rebase {
        todo(&mut buf, view, &rebase.todo);
    }
    files(
        &mut buf,
        view,
        Section::Untracked,
        "Untracked files",
        &untracked,
    );
    files(
        &mut buf,
        view,
        Section::Unstaged,
        "Unstaged changes",
        &rows(&status.unstaged),
    );
    files(
        &mut buf,
        view,
        Section::Staged,
        "Staged changes",
        &rows(&status.staged),
    );
    stashes(&mut buf, view, &status.stashes);
    recent(&mut buf, view, &status.recent);

    if status.is_clean() {
        buf.push("", Line::Blank);
        buf.face("Nothing to commit, working tree clean", Face::Comment);
        buf.end(Line::Text);
    }

    (buf.text, buf.lines, buf.spans)
}

/// Where the rebase is up to, and what stopped it. Two header lines rather than
/// a section, because it is the state of the repository and not a list.
fn rebase_header(buf: &mut Buf, rebase: &Rebase) {
    buf.face("Rebase:", Face::Comment);
    buf.put("   ");
    buf.face(
        rebase.branch.as_deref().unwrap_or("(detached)"),
        Face::Heading1,
    );
    buf.put(" onto ");
    buf.face(&rebase.onto, Face::Constant);
    buf.put(" ");
    buf.face(&format!("({}/{})", rebase.done, rebase.total), Face::Number);
    buf.end(Line::Header);

    let Some(stopped) = &rebase.stopped else {
        return;
    };
    buf.face("Stopped:", Face::Comment);
    buf.put("  ");
    buf.face(&stopped.hash, Face::Constant);
    buf.put(" ");
    // A stop with no conflicts is an `edit` or a `break` waiting patiently; a
    // stop with conflicts is the thing to deal with before anything else, and
    // the conflicted files are already listed under Unstaged as `U`.
    if rebase.is_conflicted() {
        buf.face(&clean(&stopped.subject), Face::Bold);
        buf.put(" ");
        buf.face(
            &format!("({} conflicted)", rebase.conflicts.len()),
            Face::Bold,
        );
    } else {
        buf.face(&clean(&stopped.subject), Face::Comment);
    }
    buf.end(Line::Header);
}

/// A file line, still in pieces, because each piece is coloured separately.
struct Row {
    letter: char,
    /// The old path of a rename or a copy.
    from: Option<PathBuf>,
    conflicted: bool,
    path: PathBuf,
}

fn rows(changes: &[FileChange]) -> Vec<Row> {
    changes
        .iter()
        .map(|change| Row {
            letter: change.status.letter(),
            // Both ends of a rename are worth seeing; the path the UI acts on
            // stays the new one, which is what `git add` wants.
            from: match &change.status {
                ChangeKind::Renamed { from } | ChangeKind::Copied { from } => Some(from.clone()),
                _ => None,
            },
            conflicted: change.status == ChangeKind::Unmerged,
            path: change.path.clone(),
        })
        .collect()
}

/// A heading, and whether its body follows. A folded section still draws its
/// heading and its count, or there would be no way to find it again.
fn heading(buf: &mut Buf, view: &View, section: Section, title: &str, count: usize) -> bool {
    buf.push("", Line::Blank);
    buf.face(title, face_of(section));
    buf.put(" ");
    buf.face(&format!("({count})"), Face::Number);
    let open = !view.is_collapsed(section);
    if !open {
        buf.face(" ...", Face::Punctuation);
    }
    buf.end(Line::Section(section));
    open
}

fn files(buf: &mut Buf, view: &View, section: Section, title: &str, rows: &[Row]) {
    if rows.is_empty() {
        return;
    }
    if !heading(buf, view, section, title, rows.len()) {
        return;
    }

    for row in rows {
        buf.face(&row.letter.to_string(), Face::Constant);
        buf.put(" ");
        if let Some(from) = &row.from {
            buf.face(&show(from), Face::Comment);
            buf.face(" -> ", Face::Punctuation);
        }
        // A conflict is not really on either side of the index — it is the
        // thing to deal with before anything else — so it keeps its own face
        // wherever the section puts it.
        let face = if row.conflicted {
            Face::Bold
        } else {
            face_of(section)
        };
        buf.face(&show(&row.path), face);
        buf.end(Line::File {
            path: row.path.clone(),
            section,
        });
        if let Some(diff) = view.diff_of(section, &row.path) {
            hunks(buf, diff);
        }
    }
}

/// A file's diff, one buffer line per patch line, every one of them pointing
/// back at the hunk it came from so that staging works from anywhere inside it.
fn hunks(buf: &mut Buf, diff: &FileDiff) {
    if diff.hunks.is_empty() {
        buf.face("  (no textual diff)", Face::Comment);
        buf.end(Line::Text);
        return;
    }
    for (index, hunk) in diff.hunks.iter().enumerate() {
        for line in hunk.body.lines() {
            let face = match line.as_bytes().first() {
                Some(b'@') => Some(Face::Punctuation),
                Some(b'+') => Some(Face::String),
                Some(b'-') => Some(Face::Keyword),
                // "\ No newline at end of file", which is git talking rather
                // than a change.
                Some(b'\\') => Some(Face::Comment),
                _ => None,
            };
            let text = clean(line);
            match face {
                Some(face) => buf.face(&text, face),
                None => buf.put(&text),
            }
            buf.end(Line::Hunk {
                path: diff.path.clone(),
                section: diff.section,
                index,
            });
        }
    }
}

fn todo(buf: &mut Buf, view: &View, items: &[TodoItem]) {
    if items.is_empty() {
        return;
    }
    if !heading(buf, view, Section::Rebase, "Rebase todo", items.len()) {
        return;
    }
    for (index, item) in items.iter().enumerate() {
        buf.face(item.action.keyword(), Face::Keyword);
        if !item.hash.is_empty() {
            buf.put(" ");
            buf.face(&item.hash, Face::Constant);
        }
        if !item.subject.is_empty() {
            buf.put(" ");
            buf.put(&clean(&item.subject));
        }
        buf.end(Line::Todo { index });
    }
}

fn stashes(buf: &mut Buf, view: &View, entries: &[crate::Stash]) {
    if entries.is_empty() {
        return;
    }
    if !heading(buf, view, Section::Stashes, "Stashes", entries.len()) {
        return;
    }
    for entry in entries {
        buf.face(&entry.name, Face::Constant);
        buf.put(" ");
        buf.face(&clean(&entry.subject), Face::Comment);
        buf.end(Line::Stash {
            name: entry.name.clone(),
        });
    }
}

fn recent(buf: &mut Buf, view: &View, commits: &[Commit]) {
    if commits.is_empty() {
        return;
    }
    if !heading(buf, view, Section::Recent, "Recent commits", commits.len()) {
        return;
    }
    for commit in commits {
        buf.face(&commit.hash, Face::Constant);
        buf.put(" ");
        buf.put(&clean(&commit.subject));
        buf.end(Line::Commit {
            hash: commit.hash.clone(),
        });
    }
}

/// Which side of the index a line sits on, as a colour: staged is what the next
/// commit will contain, unstaged is what it will not, untracked is what git is
/// not even watching. The three sections that are not working-tree changes are
/// furniture, except the rebase, which is a state and shouts.
fn face_of(section: Section) -> Face {
    match section {
        Section::Staged => Face::String,
        Section::Unstaged => Face::Keyword,
        Section::Untracked => Face::Comment,
        Section::Rebase => Face::Bold,
        Section::Stashes => Face::Comment,
        Section::Recent => Face::Comment,
    }
}

/// A path as display text. Control characters are folded to `?` because a
/// newline in a filename is legal on unix and would otherwise split one file
/// across two lines, sliding every entry in the map past it out of alignment.
fn show(path: &Path) -> String {
    clean(&path.display().to_string())
}

/// The same, for text that came out of a commit subject or a diff body.
fn clean(text: &str) -> String {
    text.replace(|c: char| c.is_control(), "?")
}

#[derive(Default)]
struct Buf {
    text: String,
    lines: Vec<Line>,
    spans: Vec<Span>,
    /// Char offset of the end of `text`. Tracked rather than recomputed because
    /// a `String` only knows its length in bytes.
    at: usize,
}

impl Buf {
    /// Append text in the buffer's plain foreground.
    fn put(&mut self, text: &str) {
        self.text.push_str(text);
        self.at += text.chars().count();
    }

    /// Append text and colour exactly it. Spans come out sorted and disjoint
    /// because this is the only thing that makes one and it never looks back.
    fn face(&mut self, text: &str, kind: Face) {
        let start = self.at;
        self.put(text);
        if self.at > start {
            let end = self.at;
            self.spans.push(Span { start, end, kind });
        }
    }

    /// Terminate the line. The only place `lines` grows, so text and map cannot
    /// drift apart.
    fn end(&mut self, line: Line) {
        self.text.push('\n');
        self.at += 1;
        self.lines.push(line);
    }

    /// A whole line at once, uncoloured.
    fn push(&mut self, text: &str, line: Line) {
        self.put(text);
        self.end(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Hunk, Status};

    fn view_with(diff: FileDiff) -> View {
        let mut status = Status {
            branch: Some("main".into()),
            ..Status::default()
        };
        status.unstaged.push(FileChange {
            path: diff.path.clone(),
            status: ChangeKind::Modified,
        });
        let mut view = View::of(status);
        view.diffs.push(diff);
        view
    }

    fn diff_of_two_hunks() -> FileDiff {
        FileDiff {
            path: "a.txt".into(),
            section: Section::Unstaged,
            preamble: "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n".into(),
            hunks: vec![
                Hunk {
                    header: "@@ -1,2 +1,2 @@".into(),
                    body: "@@ -1,2 +1,2 @@\n-one\n+ONE\n two\n".into(),
                },
                Hunk {
                    header: "@@ -9,2 +9,2 @@".into(),
                    body: "@@ -9,2 +9,2 @@\n-nine\n+NINE\n ten\n".into(),
                },
            ],
        }
    }

    /// The map is indexed by cursor line, so every line of a hunk — the `@@`
    /// header included — has to name the hunk it belongs to.
    #[test]
    fn every_line_of_a_hunk_points_at_that_hunk() {
        let view = view_with(diff_of_two_hunks());
        let (text, map, _) = render(&view);
        assert_eq!(text.lines().count(), map.len(), "{text}");

        let indices: Vec<usize> = map
            .iter()
            .filter_map(|line| match line {
                Line::Hunk { index, path, .. } if path == Path::new("a.txt") => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(indices, vec![0, 0, 0, 0, 1, 1, 1, 1]);

        // The body is the patch, so it is in the buffer verbatim.
        assert!(
            text.contains("@@ -1,2 +1,2 @@\n-one\n+ONE\n two\n"),
            "{text}"
        );
    }

    /// Folding is subtraction: the rows are gone, the heading and its count stay
    /// so the section can be found again.
    #[test]
    fn a_folded_section_keeps_its_heading_and_loses_its_rows() {
        let mut view = view_with(diff_of_two_hunks());
        view.toggle_section(Section::Unstaged);
        assert!(view.is_collapsed(Section::Unstaged));

        let (text, map, _) = render(&view);
        assert_eq!(text.lines().count(), map.len());
        assert!(text.contains("Unstaged changes (1) ..."), "{text}");
        assert!(!text.contains("a.txt"), "{text}");
        assert!(!map.iter().any(|l| matches!(l, Line::Hunk { .. })));
        assert!(map.contains(&Line::Section(Section::Unstaged)));

        view.toggle_section(Section::Unstaged);
        assert!(render(&view).0.contains("M a.txt"));
    }

    /// A control character in a subject would split one commit across two lines
    /// and slide the whole map after it.
    #[test]
    fn a_control_character_never_breaks_a_line() {
        let mut status = Status::default();
        status.recent.push(Commit {
            hash: "abc1234".into(),
            subject: "first\nsecond".into(),
        });
        let (text, map, _) = render(&View::of(status));
        assert_eq!(text.lines().count(), map.len());
        assert!(text.contains("abc1234 first?second"), "{text}");
    }
}
