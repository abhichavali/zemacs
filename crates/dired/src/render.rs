//! The dired buffer's text, and what each of its lines refers to.
//!
//! Pure: no filesystem, no clock, so the whole thing is cheap to test against a
//! hand-built [`Listing`]. The layout follows dired — a header naming the
//! directory, then one line per entry behind a mark column:
//!
//! ```text
//! /home/me/src  (5 entries, by name)
//!
//!   drwxr-xr-x      4.0K  2026-07-30 09:14  ../
//!   drwxr-xr-x      4.0K  2026-07-30 09:14  target/
//! * -rw-r--r--      1.2K  2026-07-29 22:01  Cargo.toml
//! D -rw-r--r--         0  2026-07-30 09:14  scratch.txt
//!   lrwxr-xr-x        11  2026-07-30 09:14  latest -> builds/2026-07
//! ```
//!
//! The load-bearing invariant is that [`render`]'s line map has one entry per
//! line of its text: the UI indexes the map by cursor line, so a drift of one
//! means `d` flags the wrong file for deletion. Every line ends through
//! [`Buf::end`], which is the only place the map is appended to, and every piece
//! of text a *file* contributed goes through [`fold`] first, because a newline
//! in a file name is legal on unix and would otherwise turn one entry into two
//! lines and slide everything after it out of alignment.
//!
//! The third return is the colour: a [`Span`] per run that is not plain
//! foreground. Dired defines no faces of its own — it borrows the syntax faces a
//! theme already sets, so a directory is whatever `(set-syntax-color "type" ...)`
//! says and there is no second palette to keep in step with the first.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Entry, Listing};

/// Marked for an operation — dired's `m`.
pub const MARK_SELECT: char = '*';
/// Flagged for deletion — dired's `d`, executed by `x`.
pub const MARK_DELETE: char = 'D';

/// One line of the dired buffer, and what the user would be acting on there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Line {
    /// The directory and its entry count.
    Header,
    /// An index into [`Listing::entries`].
    Entry(usize),
    Blank,
    /// Prose with nothing behind it, like "(empty)".
    Text,
}

/// A face for one run of the dired buffer, named after the [`crate`]-external
/// highlight class it stands for. Not that enum itself: dired does not depend on
/// the editor core, and does not want to — the whole of the coupling is the
/// caller's match from these names onto its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Face {
    /// The directory being listed.
    Heading1,
    /// A directory entry.
    Type,
    /// An entry with an execute bit.
    Function,
    /// A symlink.
    Link,
    /// The size column.
    Number,
    /// Permissions, timestamps, and everything else the eye skips.
    Comment,
    /// A mark in the left column.
    Constant,
    /// The `->` of a symlink.
    Punctuation,
}

/// A coloured run of the rendered text, in **char** offsets.
///
/// Chars rather than bytes because a name is allowed to hold anything: `café/`
/// is five characters and six bytes, and a byte offset handed to a renderer that
/// counts characters would colour the wrong part of the next entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: Face,
}

/// Render `listing` as buffer text, one [`Line`] per line of that text, and the
/// [`Span`]s that colour it.
///
/// `marks[i]` is the mark on `entries[i]`: `None`, [`MARK_SELECT`] or
/// [`MARK_DELETE`]. The slice may be short or empty — a freshly listed directory
/// has no marks at all, and the line map must not depend on the caller keeping
/// two vectors in step.
///
/// `text.lines().count() == map.len()`, always. Spans are sorted by `start` and
/// never overlap; a run that is plain foreground gets none.
pub fn render(listing: &Listing, marks: &[Option<char>]) -> (String, Vec<Line>, Vec<Span>) {
    let mut buf = Buf::default();

    let count = listing.entries.len();
    let hidden = if listing.show_hidden { ", all" } else { "" };
    buf.face(&fold(&listing.dir.to_string_lossy()), Face::Heading1);
    buf.put("  ");
    buf.face(
        &format!(
            "({count} {}, by {}{hidden})",
            if count == 1 { "entry" } else { "entries" },
            listing.sort.label(),
        ),
        Face::Comment,
    );
    buf.end(Line::Header);
    buf.push("", Line::Blank);

    // Only reachable from a hand-built `Listing`, since `list` always yields at
    // least `..`; still worth a line, so the buffer is never a bare header.
    if listing.entries.is_empty() {
        buf.put("  ");
        buf.face("(empty)", Face::Comment);
        buf.end(Line::Text);
    }

    for (index, entry) in listing.entries.iter().enumerate() {
        let mark = match marks.get(index).copied().flatten() {
            // A control character in the mark column would eat the line as
            // surely as one in a file name.
            Some(mark) if !mark.is_control() => mark,
            _ => ' ',
        };
        row(&mut buf, entry, mark);
        buf.end(Line::Entry(index));
    }

    (buf.text, buf.lines, buf.spans)
}

/// `{mark} {perms}  {size:>8}  {date}  {name}`, written a column at a time so
/// each one's span falls out of writing it rather than out of counting columns.
fn row(buf: &mut Buf, entry: &Entry, mark: char) {
    // ponytail: one face for both marks, so `*` and `D` are the same colour and
    // only the letter tells them apart. Emacs gives deletion its own red; that
    // needs a face this crate cannot reach, since the shared enum is a closed
    // set of *syntax* classes and there is no spare one that means "danger".
    match mark {
        ' ' => buf.put(" "),
        _ => buf.face(&mark.to_string(), Face::Constant),
    }
    buf.put(" ");
    buf.face(&permissions(entry), Face::Comment);

    let size = human_size(entry.len);
    buf.put(&" ".repeat(2 + 8usize.saturating_sub(size.chars().count())));
    buf.face(&size, Face::Number);

    buf.put("  ");
    let date = date(entry.modified);
    // Blank when there is no mtime, and colouring blanks says nothing.
    match date.trim().is_empty() {
        true => buf.put(&date),
        false => buf.face(&date, Face::Comment),
    }

    buf.put("  ");
    name(buf, entry);
}

/// `drwxr-xr-x`, `ls -l` style. The type character carries what the trailing
/// `/` cannot: a symlink is `l` whatever it points at.
fn permissions(entry: &Entry) -> String {
    let kind = match () {
        _ if entry.is_symlink => 'l',
        _ if entry.is_dir => 'd',
        _ => '-',
    };
    let mut out = String::with_capacity(10);
    out.push(kind);
    if cfg!(unix) {
        for shift in [6, 3, 0] {
            let bits = (entry.mode >> shift) & 0o7;
            out.push(if bits & 0o4 != 0 { 'r' } else { '-' });
            out.push(if bits & 0o2 != 0 { 'w' } else { '-' });
            out.push(if bits & 0o1 != 0 { 'x' } else { '-' });
        }
    } else {
        // ponytail: no ACL reading off unix. `readonly` is the only bit the
        // platform offers through `std`, so the column shows that and nothing
        // else — setuid and sticky are not rendered anywhere.
        out.push_str(if entry.readonly {
            "r--r--r--"
        } else {
            "rw-rw-rw-"
        });
    }
    out
}

fn name(buf: &mut Buf, entry: &Entry) {
    let mut text = fold(&entry.name.to_string_lossy());
    // A symlink is marked by its arrow, not by a slash, so the name a user
    // reads is the name they would type.
    if entry.is_dir && !entry.is_symlink {
        text.push('/');
    }
    match face_of(entry) {
        Some(face) => buf.face(&text, face),
        None => buf.put(&text),
    }
    if let Some(target) = &entry.link_target {
        buf.face(" -> ", Face::Punctuation);
        buf.face(&fold(&target.to_string_lossy()), Face::Comment);
    }
}

/// Directory, executable, symlink: the three things a `ls --color` user reads
/// off the colour instead of off the permission column.
///
/// Symlink wins over directory, the same way the `l` in the mode string does — a
/// link is a link whatever it resolves to. A plain file gets `None` rather than
/// a "default" span, because a run with no span already renders in the buffer's
/// foreground and one that says so is one more thing to keep in step with it.
fn face_of(entry: &Entry) -> Option<Face> {
    match () {
        _ if entry.is_symlink => Some(Face::Link),
        _ if entry.is_dir => Some(Face::Type),
        _ if entry.mode & 0o111 != 0 => Some(Face::Function),
        _ => None,
    }
}

/// `4.0K`, `1.2M`, and `0` — one decimal until the number would need three
/// digits, which is what makes a right-aligned column readable.
///
/// Total across `u64`: no unchecked cast, no `unwrap`, no panic at 0 or
/// [`u64::MAX`] (which is `16E`).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["K", "M", "G", "T", "P", "E"];
    if bytes < 1024 {
        return bytes.to_string();
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    // Rounding can carry into the next unit: 1048575 bytes is 1023.999K, which
    // must print as 1.0M rather than as the impossible 1024.0K.
    if size.round() >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if size < 9.995 {
        format!("{size:.1}{}", UNITS[unit])
    } else {
        format!("{}{}", size.round() as u64, UNITS[unit])
    }
}

/// `2026-07-30 09:14`, or blanks of the same width when there is no mtime, so
/// the name column never moves.
///
/// ponytail: UTC, not local time. A timezone means either a database or libc,
/// and the column is for "is this newer than that", which UTC answers.
fn date(modified: Option<SystemTime>) -> String {
    let Some(time) = modified else {
        return " ".repeat(16);
    };
    let seconds = match time.duration_since(UNIX_EPOCH) {
        // Saturating rather than `as i64`, which would wrap an absurd timestamp
        // into a negative one and print a year in the past.
        Ok(since) => since.as_secs().min(i64::MAX as u64) as i64,
        Err(before) => -(before.duration().as_secs().min(i64::MAX as u64) as i64),
    };
    // Euclidean, so a pre-epoch timestamp lands on the previous day rather than
    // on a negative time of day.
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    let (hour, minute) = (rest / 3600, rest % 3600 / 60);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Howard Hinnant's `civil_from_days`: days since the epoch to a proleptic
/// Gregorian date, by shifting the year to start in March so that the leap day
/// is the last day of it and the month lengths become a linear sequence.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468; // epoch moved to 0000-03-01
    let era = shifted.div_euclid(146_097); // 400 years, the full leap cycle
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Display text for something a *file* named, with control characters folded to
/// `?`. A newline would split one entry across two rendered lines and slide the
/// whole line map; a carriage return or an escape would redraw the buffer.
fn fold(text: &str) -> String {
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
    use std::ffi::OsStr;
    use std::path::Path;

    use super::*;

    fn entry(name: &str) -> Entry {
        Entry {
            name: OsStr::new(name).to_os_string(),
            path: Path::new("/tmp").join(name),
            is_dir: false,
            is_symlink: false,
            link_target: None,
            len: 0,
            modified: None,
            readonly: false,
            mode: 0o644,
        }
    }

    #[test]
    fn sizes_are_human_and_never_panic() {
        assert_eq!(human_size(0), "0");
        assert_eq!(human_size(1023), "1023");
        assert_eq!(human_size(1024), "1.0K");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(4096), "4.0K");
        assert_eq!(human_size(10 * 1024), "10K");
        // The carry: 1023.999K is a megabyte.
        assert_eq!(human_size(1024 * 1024 - 1), "1.0M");
        assert_eq!(human_size(1_258_291), "1.2M");
        assert_eq!(human_size(u64::MAX), "16E");
        // Every size renders inside the column it was given.
        for bytes in [0, 1, 1023, 1024, u64::MAX / 3, u64::MAX] {
            assert!(human_size(bytes).len() <= 8, "{bytes}");
        }
    }

    #[test]
    fn dates_are_utc_and_fixed_width() {
        assert_eq!(date(Some(UNIX_EPOCH)), "1970-01-01 00:00");
        let known = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(date(Some(known)), "2023-11-14 22:13");
        // A leap day, which is where the March-shifted arithmetic earns itself.
        let leap = UNIX_EPOCH + std::time::Duration::from_secs(1_709_208_000);
        assert_eq!(date(Some(leap)), "2024-02-29 12:00");
        // Pre-epoch and absent both keep the column width.
        let before = UNIX_EPOCH - std::time::Duration::from_secs(86_400);
        assert_eq!(date(Some(before)), "1969-12-31 00:00");
        assert_eq!(date(None).len(), 16);
    }

    #[test]
    fn permissions_read_like_ls() {
        let mut file = entry("a.txt");
        file.mode = 0o644;
        let mut dir = entry("src");
        dir.is_dir = true;
        dir.mode = 0o755;
        let mut link = entry("latest");
        link.is_symlink = true;
        link.is_dir = true;
        link.mode = 0o777;
        link.link_target = Some("builds/x".into());

        if cfg!(unix) {
            assert_eq!(permissions(&file), "-rw-r--r--");
            assert_eq!(permissions(&dir), "drwxr-xr-x");
            assert_eq!(permissions(&link), "lrwxrwxrwx");
        }
        // A symlink to a directory is an arrow, not a slash.
        let mut buf = Buf::default();
        name(&mut buf, &dir);
        buf.put("|");
        name(&mut buf, &link);
        assert_eq!(buf.text, "src/|latest -> builds/x");
    }

    /// The map has to survive a hostile name without the caller's help.
    #[test]
    fn a_newline_in_a_name_cannot_add_a_line() {
        let listing = Listing {
            dir: "/tmp/dired\ntest".into(),
            entries: vec![entry("two\nlines.txt"), entry("plain.txt")],
            show_hidden: false,
            sort: crate::Sort::Name,
        };
        let (text, map, _) = render(&listing, &[]);
        assert_eq!(text.lines().count(), map.len());
        assert!(text.contains("two?lines.txt"), "{text}");
        assert!(text.starts_with("/tmp/dired?test  (2 entries"), "{text}");
        assert_eq!(map[map.len() - 2], Line::Entry(0));
    }

    /// Marks are the caller's bookkeeping and may not line up with the entries.
    #[test]
    fn a_short_or_silly_marks_slice_does_not_move_anything() {
        let listing = Listing {
            dir: "/tmp".into(),
            entries: vec![entry("a"), entry("b"), entry("c")],
            show_hidden: false,
            sort: crate::Sort::Name,
        };
        let (long, ..) = render(&listing, &[Some(MARK_SELECT), None, Some(MARK_DELETE)]);
        // Missing marks, extra marks, and a mark that would break the line.
        let (short, map, _) = render(&listing, &[Some(MARK_SELECT)]);
        let (odd, ..) = render(
            &listing,
            &[Some(MARK_SELECT), Some('\n'), Some(MARK_DELETE)],
        );
        assert_eq!(long.lines().count(), map.len());
        assert_eq!(short.lines().count(), map.len());
        // A newline mark folds to a blank, so it renders as the unmarked case.
        assert_eq!(odd, long);
        assert!(long.lines().any(|l| l.starts_with("D ")));
        assert!(!short.lines().any(|l| l.starts_with("D ")));
        assert_eq!(odd.lines().count(), map.len());
    }

    #[test]
    fn an_empty_listing_still_renders() {
        let listing = Listing {
            dir: "/tmp".into(),
            entries: vec![],
            show_hidden: true,
            sort: crate::Sort::Size,
        };
        let (text, map, _) = render(&listing, &[]);
        assert_eq!(text.lines().count(), map.len());
        assert_eq!(map, vec![Line::Header, Line::Blank, Line::Text]);
        assert!(
            text.starts_with("/tmp  (0 entries, by size, all)"),
            "{text}"
        );
    }
}
