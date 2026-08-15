//! Characters to cells — the one place the two units are allowed to meet.
//!
//! Every offset in this editor is a *character* index: the rope counts them,
//! markers and overlays are pairs of them, and the whole Lisp API is written in
//! them. A **cell** is a different quantity — a column of the monospace grid the
//! renderer draws on — and the map between them is not the identity:
//!
//! - a tab is several cells, up to the next tab stop;
//! - `漢` and `😀` are two, because that is what every font that has them draws;
//! - a combining mark is none: it is painted on the character before it.
//!
//! This lives in core rather than in the renderer because *two* things need the
//! answer and they must not disagree. The renderer needs it to put a glyph in a
//! column. `j` and `k` need it to move by a **visual** line, which is a row of
//! cells and not a row of characters — and if the two ever computed cells
//! differently, `j` would land the cursor somewhere the block is not drawn.
//!
//! No SDL, no fonts, no window: the width of a character is a property of
//! Unicode, and the *pixel* width of a cell — which really is the renderer's —
//! never appears here.
//!
//! Overlays are here for the same reason `j` is: an org-modern bullet, a
//! checkbox glyph or a revealed link is drawn *instead of* the characters it
//! covers, so it moves every column on its line. That substitution used to live
//! in the renderer alone, which meant core and the screen laid the same line out
//! differently and `j` down a heading landed a character off — see
//! [`line_cells`], which is now the single answer both of them ask for.

use crate::overlay::Overlay;

/// Cells `c` occupies. Two for East Asian wide and most emoji, zero for
/// combining marks, one for everything else.
///
/// `width()` answers `None` for a control character, which cannot be shown as
/// itself. Emacs draws `^G`; we draw nothing, because a caret escape is two
/// cells of invented text and no config has ever wanted it here. ponytail:
/// upgrade path is a substitution in [`expand_line`], where a tab already
/// becomes several cells.
pub fn char_cells(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Cells a whole string occupies — the width of a modeline segment, a
/// completion candidate or a dashboard row. Never `chars().count()`, which is
/// the same lie one cell per character always was.
pub fn str_cells(s: &str) -> usize {
    // The sum of [`char_cells`], and *not* `UnicodeWidthStr::width`, which is
    // not the same function: `width` counts a tab as one column while
    // `UnicodeWidthChar::width('\t')` is `None` — a control character has no
    // intrinsic width — and so `char_cells` calls it zero.
    //
    // Two measurements of the same string that disagree is a bug wherever they
    // meet, and they met in `truncate`/`draw_segments`: `truncate` filled its
    // budget with `char_cells` and the caller subtracted the result measured
    // with `str_cells`, so a modeline segment containing a tab reported *more*
    // cells than were asked for and underflowed a `usize`. That is a panic in a
    // running editor, and it is what `truncate("\t\ta", 2)` did.
    //
    // Summing is the right direction of the two because the renderer advances
    // by `char_cells` per character when it actually draws (`draw_weighted`).
    // A width that disagrees with the advance is wrong by definition, whatever
    // the Unicode annex says a tab ought to be — a tab in a *document* never
    // reaches here anyway, `expand_line` having already turned it into spaces.
    s.chars().map(char_cells).sum()
}

/// One display cell: the glyph drawn in it, and the index *within its line* of
/// the source character it belongs to.
///
/// Several cells sharing one source index is the normal case, not an oddity: a
/// tab has always worked this way, and a wide character is the same shape.
pub type Cell = (char, usize);

/// Visual cells for one line.
///
/// A tab becomes a run of spaces up to the next tab stop, every one of them
/// pointing back at the tab so cursor/selection/highlight math still works in
/// source-char offsets.
///
/// A wide character is that same shape: the glyph, then one blank continuation
/// cell attributed to it. The glyph is blitted at its *natural* width, so it
/// covers both, and the continuation cell is a space precisely so that drawing
/// it a second time is a no-op. A zero-width character contributes no cell at
/// all, so a cursor on one lands on the character it decorates — see
/// [`visual_col`], which already had to answer that question for overlays.
pub fn expand_line(line: &str, tab_width: usize) -> Vec<Cell> {
    let tw = tab_width.max(1);
    let mut out = Vec::with_capacity(line.len());
    for (i, c) in line.chars().enumerate() {
        match c {
            '\t' => {
                for _ in 0..(tw - out.len() % tw) {
                    out.push((' ', i));
                }
            }
            '\n' | '\r' => {}
            _ => {
                let w = char_cells(c);
                if w > 0 {
                    out.push((c, i));
                    out.extend(std::iter::repeat((' ', i)).take(w - 1));
                }
            }
        }
    }
    out
}

/// Replace the cells of each `(start, end, text)` — line-relative *source* char
/// offsets — with `text`'s characters, all attributed to `start`.
///
/// Attributing them to `start` is what keeps everything else working unchanged:
/// [`visual_col`] still finds a column for a cursor inside the hidden range, the
/// highlight cursor still walks monotonically, and wrapping counts the cells
/// that are actually drawn.
///
/// `subs` must be sorted by `start`. Overlapping substitutions are not
/// composable and are not composed — the one that starts first wins and the rest
/// are dropped. In practice they never overlap: LaTeX fragments are disjoint by
/// construction, and so are bullets.
///
/// Offsets in, offsets out: nothing here invents a character, so a substitution
/// hides text from the *screen* and never from an edit. The buffer is still the
/// truth about what `x` deletes.
///
/// A line with no cells at all is the degenerate entry rather than a second
/// shape: the walk below is driven by cells and a blank line has none, so the
/// first substitution is emitted and the loop is never entered.
pub fn substitute(cells: &[Cell], subs: &[(usize, usize, String)]) -> Vec<Cell> {
    // The blank line. A `display` string is drawn *instead of* what it covers,
    // and on an empty line what it covers is nothing — which is a range worth
    // drawing over anyway: ghost text where a body has yet to be typed, a
    // separator rule, an equation on a line of its own. Without this the walk
    // has no first cell to hang off and the string is silently dropped, which
    // is what made an overlay on a blank line invisible even after the renderer
    // started handing one over (`on_line`).
    //
    // The first substitution wins, which is the same rule the loop applies when
    // two claim one column — and on a blank line every sub claims that one
    // column, [`display_subs`] having rebased them all onto `(0, 0)`.
    if cells.is_empty() {
        return match subs.first() {
            Some((s, _, text)) => text.chars().map(|c| (c, *s)).collect(),
            None => Vec::new(),
        };
    }
    let mut out = Vec::with_capacity(cells.len());
    let (mut i, mut si) = (0usize, 0usize);
    while i < cells.len() {
        let src = cells[i].1;
        while si < subs.len() && subs[si].1 <= src {
            si += 1;
        }
        match subs.get(si) {
            Some((s, e, text)) if *s <= src => {
                out.extend(text.chars().map(|c| (c, *s)));
                while i < cells.len() && cells[i].1 < *e {
                    i += 1;
                }
                let e = *e;
                si += 1;
                while si < subs.len() && subs[si].0 < e {
                    si += 1; // started inside the one just applied
                }
            }
            _ => {
                out.push(cells[i]);
                i += 1;
            }
        }
    }
    out
}

/// Whether `o` claims any of the buffer line spanning chars `[start, end)`.
///
/// Half-open like every offset here: an overlay ending exactly at `start`
/// stopped on the line before, and one starting exactly at `end` sits on the
/// newline and belongs to the line after — a diagnostic on the end of one line
/// must not smear onto the next.
///
/// `o.start == start` is the exception a **blank** line forces. An empty line
/// has `start == end`, so `o.start < end` is false for every overlay ever made
/// and such a line could carry nothing at all: no ghost text, no separator, no
/// diagnostic band. On a line with any width the extra clause is already implied
/// by `o.start < end`, so it costs those lines nothing. What it does *not*
/// re-admit is the zero-length overlay — `o.end > start` still rejects one that
/// begins and ends at `start`, exactly as "ended on the line before" does.
///
/// Public because the renderer asks the same question, about the payloads that
/// need no character underneath them (`line_background`, `line_prefix`,
/// `gutter`), while this asks it about the one that replaces cells. The two
/// answering differently is precisely the disagreement [`line_cells`] exists to
/// prevent — the cursor's column and the drawn column would come from two
/// different ideas of which overlays are on the line. So there is one
/// definition and render imports it, rather than a copy per crate that is only
/// correct while both are remembered together.
pub fn on_line(o: &Overlay, start: usize, end: usize) -> bool {
    o.end > start && (o.start < end || o.start == start)
}

/// The substitutions the overlays touching one buffer line — chars
/// `[start, end)` — ask for, rebased onto it and sorted for [`substitute`].
///
/// `display` strings only. An overlay carrying an **image** is deliberately not
/// here: the cells a bitmap reserves are its pixel width over the width of a
/// cell, and a cell's width in pixels is the renderer's alone. Core guessing one
/// would put a second disagreement where this removes the first. The renderer
/// pushes its own image substitutions *in front of* these, so an overlay
/// carrying both still draws as an image — the earlier start wins in
/// [`substitute`], and a stable sort leaves them in that order — while one whose
/// bitmap has not been rasterised yet falls back to its string exactly as it did
/// before.
///
/// An overlay reaching onto later lines substitutes on the first of them and
/// blanks the rest: one bullet, then the empty rows its own source lines have
/// become. That rule is the draw loop's and is copied here rather than left
/// there, because a continuation row genuinely *is* empty on screen and a cursor
/// walking onto it belongs in column zero.
pub fn display_subs<'a>(
    overlays: impl IntoIterator<Item = &'a Overlay>,
    start: usize,
    end: usize,
) -> Vec<(usize, usize, String)> {
    let mut subs: Vec<(usize, usize, String)> = overlays
        .into_iter()
        .filter(|o| on_line(o, start, end) && o.display.is_some())
        .map(|o| {
            let text = match o.start < start {
                true => String::new(),
                false => o.display.clone().unwrap_or_default(),
            };
            (o.start.max(start) - start, o.end.min(end) - start, text)
        })
        .collect();
    // Stable, so that two overlays claiming the same start are still separated
    // by creation order — which is the order the renderer resolves every other
    // overlay attribute in.
    subs.sort_by_key(|&(s, _, _)| s);
    subs
}

/// The cells of one buffer line: [`expand_line`] over its text, then whatever
/// its overlays draw instead of parts of it.
///
/// **The one layout, and the reason this function exists.** Core used to expand
/// a line without overlays while the renderer substituted into it, so on any
/// line carrying a bullet, a checkbox or a shortened link the two disagreed
/// about how many cells there were and where each character sat — and `j` down
/// such a line landed a character off. Both sides call this now.
///
/// `text` is the line's own characters and `start`/`end` its char range in the
/// buffer, because that is the coordinate overlays are stored in.
pub fn line_cells<'a>(
    text: &str,
    tab_width: usize,
    overlays: impl IntoIterator<Item = &'a Overlay>,
    start: usize,
    end: usize,
) -> Vec<Cell> {
    let cells = expand_line(text, tab_width);
    let subs = display_subs(overlays, start, end);
    match subs.is_empty() {
        // Every line of every code buffer, and most lines of an org one: no
        // second vector and no second walk when nothing was substituted.
        true => cells,
        false => substitute(&cells, &subs),
    }
}

/// Visual column of source char `src`; the end of the line if it is past it
/// (which is where the cursor sits on an empty line, or at EOL in insert mode).
///
/// The *first* cell at or after `src`, not the cell for `src` exactly: a
/// substituted range (an overlay `display`, an image) has no cell of its own for
/// the characters it hides, and a cursor inside one belongs at the front of what
/// replaced them. On a line with no overlays every source char still has a cell,
/// so this is the same answer it has always given.
pub fn visual_col(cells: &[Cell], src: usize) -> usize {
    match cells.iter().position(|&(_, i)| i >= src) {
        // The ordinary answer, and the only one on a line with no overlays.
        Some(k) if cells[k].1 == src => k,
        // A source char with no cell of its own, because something replaced the
        // range it was in — or because it is a combining mark, which is drawn on
        // the character before it. Either way the cursor belongs at the *front*
        // of what it shares a column with: landing after it would put the cursor
        // a character further right than the buffer says it is.
        Some(_) => cells.iter().rposition(|&(_, i)| i < src).map_or(0, |k| {
            cells.iter().position(|&(_, i)| i == cells[k].1).unwrap_or(k)
        }),
        // Past the last character — where the cursor sits on an empty line, or
        // at EOL in insert mode.
        None => cells.len(),
    }
}

/// Source char at cell `col`, or the end of the line when `col` is past it.
///
/// The inverse of [`visual_col`], and deliberately *not* exact: landing in the
/// middle of a tab run or on the second half of `漢` answers the character those
/// cells belong to, because there is no character between them to land on.
pub fn char_at_cell(cells: &[Cell], col: usize, line_len: usize) -> usize {
    cells.get(col).map_or(line_len, |&(_, i)| i)
}

/// The cell each display row of `cells` starts at, in a pane `cols` wide.
///
/// **The one answer to "where does this line break", and every other question
/// about wrapping is asked of the list it returns.** It used to be division —
/// row `r` started at `r * cols` — and division is exactly what breaks a word
/// in half. A break position depends on the *text*, so a number of columns is
/// no longer enough to compute one, and the arithmetic has to become a lookup.
///
/// Greedy, and a word moves down whole: within each row, the break goes after
/// the last space that fits. A word wider than the pane is broken at the edge
/// rather than dropped, which is the one case where breaking mid-word is right
/// — a 200-character URL in a 40-column pane has to go somewhere.
///
/// Spaces only, and no dictionary: a tab is already spaces by the time it gets
/// here ([`expand_line`]), and hyphens, slashes and CJK are deliberately not
/// break opportunities. ponytail: so `foo/bar/baz` and a Japanese sentence (no
/// spaces at all) still break at the pane edge. Ceiling: a long path or a CJK
/// paragraph wraps exactly as it used to. Upgrade path is UAX #14, which is a
/// table and not a line of code, and is worth it the day this editor is used
/// for prose in a language that does not space its words.
///
/// Always at least one row, so an empty line still has one for its gutter
/// number and its cursor. `cols == 0` — a pane narrower than its own line
/// numbers — is the infinite-loop case: it yields one row rather than dividing
/// by zero, and every caller advances past the line either way.
pub fn wrap_breaks(cells: &[Cell], cols: usize) -> Vec<usize> {
    let mut rows = vec![0];
    if cols == 0 {
        return rows;
    }
    let mut start = 0;
    while start + cols < cells.len() {
        let limit = start + cols;
        // The last position in `(start, limit]` with a space in front of it —
        // that is, the start of the last word that would still fit. Searched
        // backwards from the edge because the greedy answer is the *latest*
        // break, not the earliest.
        let brk = (start + 1..=limit)
            .rev()
            .find(|&k| cells[k - 1].0 == ' ')
            // No space anywhere in the row: one word, wider than the pane. Cut
            // it at the edge, which is the only place left.
            .unwrap_or(limit);
        rows.push(brk);
        start = brk;
    }
    rows
}

/// Which display row cell `cell` is drawn on, given a line's [`wrap_breaks`].
pub fn wrap_row_of(breaks: &[usize], cell: usize) -> usize {
    breaks.partition_point(|&s| s <= cell).saturating_sub(1)
}

/// The cell range `[start, end)` of display row `row`, for a line of `len`
/// cells. A row past the end is empty at the end of the line, which is what a
/// caller that clamped its own row count would want.
pub fn wrap_row_range(breaks: &[usize], row: usize, len: usize) -> (usize, usize) {
    let start = breaks.get(row).copied().unwrap_or(len);
    (start, breaks.get(row + 1).copied().unwrap_or(len).max(start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_character_takes_two_cells_and_a_combining_mark_none() {
        assert_eq!(char_cells('a'), 1);
        assert_eq!(char_cells('漢'), 2);
        assert_eq!(char_cells('😀'), 2);
        assert_eq!(char_cells('\u{0301}'), 0); // combining acute
        assert_eq!(str_cells("日本語"), 6);
        assert_eq!(str_cells("e\u{0301}"), 1); // e + acute is one column
    }

    /// The two must be the same measurement, because the renderer *advances* by
    /// `char_cells` per character and *measures* with `str_cells`. They were
    /// not: `UnicodeWidthStr::width` calls a tab one column and
    /// `UnicodeWidthChar::width('\t')` is `None`, so a string with a tab in it
    /// measured wider than it drew — which underflowed the modeline's remaining
    /// budget and panicked the editor.
    #[test]
    fn str_cells_is_exactly_the_sum_of_char_cells() {
        for s in [
            "", "a", "日本語", "e\u{0301}", "🙂🙂", "a\tb", "\t\ta", "\u{0}\u{1}x",
            "*tutor-lisp*", "linear-algebra.org", "…", "a\u{301}漢\t…",
        ] {
            assert_eq!(
                str_cells(s),
                s.chars().map(char_cells).sum::<usize>(),
                "disagreement on {s:?}"
            );
        }
        // The specific case, spelled out: a tab has no intrinsic width here.
        assert_eq!(char_cells('\t'), 0);
        assert_eq!(str_cells("\t\ta"), 1);
    }

    /// The cursor and the selection are both resolved through [`visual_col`], so
    /// this is the whole of "the cursor lands on the right cell" — every
    /// consumer (block cursor, selection run, wrap point, `j`) reads this number.
    #[test]
    fn the_cursor_lands_on_the_cell_a_wide_character_starts_in() {
        // "a漢b": cells are a | 漢 | (blank) | b.
        let cells = expand_line("a漢b", 4);
        assert_eq!(cells.len(), 4);
        assert_eq!(cells.iter().map(|&(c, _)| c).collect::<String>(), "a漢 b");
        // ...and both cells of 漢 point back at source char 1, exactly as a
        // tab's do, so nothing downstream has to know it was wide.
        assert_eq!(cells[1].1, 1);
        assert_eq!(cells[2].1, 1);
        assert_eq!(visual_col(&cells, 0), 0);
        assert_eq!(visual_col(&cells, 1), 1); // on the glyph, not past it
        assert_eq!(visual_col(&cells, 2), 3); // 'b' is a column further right
        assert_eq!(visual_col(&cells, 3), 4); // past EOL

        // A selection over just 漢 is source chars [1, 2) and covers *both* its
        // cells: the run is [visual_col(1), visual_col(2)).
        assert_eq!((visual_col(&cells, 1), visual_col(&cells, 2)), (1, 3));

        // ...and back again, including from the cell 漢 does not start in.
        assert_eq!(char_at_cell(&cells, 1, 3), 1);
        assert_eq!(char_at_cell(&cells, 2, 3), 1);
        assert_eq!(char_at_cell(&cells, 3, 3), 2);
        assert_eq!(char_at_cell(&cells, 9, 3), 3); // past the end
    }

    /// Emoji are the case that used to walk the cursor off the end of a line:
    /// four of them are eight cells, not four.
    #[test]
    fn emoji_are_two_cells_each() {
        let cells = expand_line("😀😀😀😀", 4);
        assert_eq!(cells.len(), 8);
        assert_eq!(visual_col(&cells, 2), 4);
        // Wrapping counts cells, so this is two rows in a four-column pane where
        // a character count would have said one.
        assert_eq!(wrap_breaks(&cells, 4).len(), 2);
    }

    /// The rule: a word moves down whole. Everything else here is what that
    /// costs at the edges.
    #[test]
    fn a_line_wraps_between_words_and_only_breaks_one_it_has_to() {
        let rows = |text: &str, cols| {
            let cells = expand_line(text, 4);
            let breaks = wrap_breaks(&cells, cols);
            (0..breaks.len())
                .map(|r| {
                    let (s, e) = wrap_row_range(&breaks, r, cells.len());
                    cells[s..e].iter().map(|&(c, _)| c).collect::<String>()
                })
                .collect::<Vec<_>>()
        };

        // The whole point: `hello world` in ten columns is two words, not
        // `hello worl` and a stranded `d`.
        assert_eq!(rows("hello world", 10), ["hello ", "world"]);
        // The break is greedy — as many words as fit, and the space that ends
        // the row stays on it rather than starting the next.
        assert_eq!(rows("aaa bb cc ddddd", 10), ["aaa bb cc ", "ddddd"]);
        // A word wider than the pane is cut at the edge. Anything else would
        // mean a 200-character URL had nowhere to go.
        assert_eq!(rows("aaa bbbbbbbbbb", 6), ["aaa ", "bbbbbb", "bbbb"]);
        assert_eq!(rows("supercalifragilistic", 5), ["super", "calif", "ragil", "istic"]);
        // A line that fits is one row, and an empty line is still a row.
        assert_eq!(rows("short", 10), ["short"]);
        assert_eq!(rows("", 10), [""]);
        // Exactly the width: one row, and no phantom empty second one.
        assert_eq!(rows("abcde", 5), ["abcde"]);
        // A pane with no columns at all yields one row rather than looping.
        assert_eq!(wrap_breaks(&expand_line("anything", 4), 0), [0]);

        // ...and the lookups agree with the ranges, which is the invariant every
        // caller depends on: `j` finds a row and the renderer draws it.
        let cells = expand_line("aaa bb cc ddddd", 4);
        let breaks = wrap_breaks(&cells, 10);
        assert_eq!(breaks, [0, 10]);
        assert_eq!(wrap_row_of(&breaks, 0), 0);
        assert_eq!(wrap_row_of(&breaks, 9), 0);
        assert_eq!(wrap_row_of(&breaks, 10), 1);
        assert_eq!(wrap_row_of(&breaks, 99), 1, "past the end is the last row");
        assert_eq!(wrap_row_range(&breaks, 1, cells.len()), (10, 15));
    }

    /// A combining mark decorates the character before it rather than occupying
    /// a column of its own, so the cursor on one lands on what it decorates and
    /// the characters after it do not shift right.
    #[test]
    fn a_combining_mark_occupies_no_column() {
        let cells = expand_line("e\u{0301}x", 4);
        assert_eq!(cells.len(), 2);
        assert_eq!(visual_col(&cells, 0), 0);
        assert_eq!(visual_col(&cells, 1), 0); // the mark, on its base
        assert_eq!(visual_col(&cells, 2), 1); // 'x' has not moved
    }

    #[test]
    fn tabs_expand_to_the_next_tab_stop() {
        assert_eq!(expand_line("\tx", 4).len(), 5);
        // "ab\tc": two chars, then 2 spaces to reach column 4, then 'c'.
        let cells = expand_line("ab\tc", 4);
        assert_eq!(cells.iter().map(|&(c, _)| c).collect::<String>(), "ab  c");
        // Every expanded space maps back to the tab at source index 2.
        assert_eq!(cells[2].1, 2);
        assert_eq!(cells[3].1, 2);
        assert_eq!(cells[4].1, 3);
        // tab_width 0 must not divide by zero.
        assert_eq!(expand_line("\t", 0).len(), 1);
    }

    // --- overlays ---------------------------------------------------------
    //
    // The half of a line's layout that is not its text. Everything below is
    // about one claim: what core counts and what the renderer draws are the
    // same cells, on a line whose stars became a bullet or whose `[ ]` became
    // one box.

    use crate::overlay::{OverlayEdit, OverlayId, Overlays};

    /// `display` overlays as `(id, start, end, text)` in **buffer** char
    /// offsets — the coordinate they are really stored in, so that clipping a
    /// multi-line one onto a later line is testable at all.
    fn overlays(spans: &[(OverlayId, usize, usize, &str)]) -> Overlays {
        let mut ovs = Overlays::default();
        for &(id, s, e, text) in spans {
            ovs.add(id, s, e);
            ovs.edit(OverlayEdit::Display(id, Some(text.to_string())));
        }
        ovs
    }

    fn text_of(cells: &[Cell]) -> String {
        cells.iter().map(|&(c, _)| c).collect()
    }

    /// Motion has to stay **total** over a substituted line: every source char
    /// still has a column, every column still has a source char, and neither map
    /// ever goes backwards. Asserted as a property rather than by example
    /// because the failure mode this guards is a cursor that skips a character
    /// or cannot be moved off one.
    fn motion_is_total(cells: &[Cell], line_len: usize) {
        let mut last = 0;
        for src in 0..=line_len {
            let col = visual_col(cells, src);
            assert!(col >= last, "visual_col went backwards at {src}");
            assert!(col <= cells.len(), "visual_col past the line at {src}");
            last = col;
            // Landing on a column and asking what is under it never answers a
            // character *after* the one asked about — that is the shape of a
            // cursor that walks right on its own.
            if col < cells.len() {
                assert!(char_at_cell(cells, col, line_len) <= src, "overshot at {src}");
            }
        }
        for col in 0..cells.len() {
            let src = char_at_cell(cells, col, line_len);
            assert!(src < line_len.max(1), "char_at_cell past the text at {col}");
        }
    }

    /// The bug this whole path exists to close. org-modern draws `** ` as one
    /// bullet, so every column after it moves — and core used to expand the line
    /// without knowing that, which is how `j` down a heading landed a character
    /// left of the block.
    #[test]
    fn a_heading_is_laid_out_with_its_bullet_and_not_with_its_stars() {
        let line = "** a heading";
        let ovs = overlays(&[(1, 0, 2, "◉")]);
        let cells = line_cells(line, 4, ovs.all(), 0, line.chars().count());
        assert_eq!(text_of(&cells), "◉ a heading");

        // The two stars share the bullet's one column, and the space after them
        // — deliberately outside the overlay — keeps a column of its own.
        assert_eq!(visual_col(&cells, 0), 0);
        assert_eq!(visual_col(&cells, 1), 0);
        assert_eq!(visual_col(&cells, 2), 1);
        assert_eq!(visual_col(&cells, 3), 2, "'a' is drawn two columns in");
        // ...and back: any column inside the substitution answers its start,
        // which is a real character the buffer has and `x` can delete.
        assert_eq!(char_at_cell(&cells, 0, 12), 0);
        assert_eq!(char_at_cell(&cells, 2, 12), 3);
        motion_is_total(&cells, 12);
    }

    /// The newest and worst of them: three characters become one glyph, so the
    /// cursor sits on the same column for all three. That is the price of a
    /// substitution and it must not become a trap — `l` still walks off the far
    /// side, because offsets are the truth and only cells were replaced.
    #[test]
    fn a_checkbox_puts_three_characters_in_one_column_without_trapping_the_cursor() {
        let line = "- [ ] milk";
        let ovs = overlays(&[(1, 2, 5, "☐")]);
        let cells = line_cells(line, 4, ovs.all(), 0, line.chars().count());
        assert_eq!(text_of(&cells), "- ☐ milk");

        assert_eq!(visual_col(&cells, 2), 2, "'['");
        assert_eq!(visual_col(&cells, 3), 2, "the space inside it");
        assert_eq!(visual_col(&cells, 4), 2, "']'");
        assert_eq!(visual_col(&cells, 5), 3, "the space after it moved left");
        assert_eq!(visual_col(&cells, 6), 4, "'m'");
        // Three `l`s cross the box: the column does not move for two of them and
        // then it does. No offset was skipped and none was invented.
        assert_eq!(char_at_cell(&cells, 2, 10), 2);
        assert_eq!(char_at_cell(&cells, 3, 10), 5);
        motion_is_total(&cells, 10);
    }

    /// A substitution changes where a line *breaks*, which is the half of this
    /// that `j` feels rather than sees: the row below a wrapped heading starts
    /// at a different word once the stars are one glyph.
    #[test]
    fn wrapping_counts_the_cells_a_substitution_left_behind() {
        let line = "** aaa bbb ccc";
        let len = line.chars().count();
        let plain = expand_line(line, 4);
        let ovs = overlays(&[(1, 0, 2, "◉")]);
        let cells = line_cells(line, 4, ovs.all(), 0, len);

        // 14 cells break after "** aaa ", 13 break after "◉ aaa bbb " — one
        // fewer cell is a whole extra word on the first row.
        assert_eq!(wrap_breaks(&plain, 10), [0, 7]);
        assert_eq!(wrap_breaks(&cells, 10), [0, 10]);
        let breaks = wrap_breaks(&cells, 10);
        assert_eq!(wrap_row_of(&breaks, visual_col(&cells, 11)), 1, "'ccc' is on row 1");
        // Row 1 is "ccc" and its first character is source char 11.
        let (s, e) = wrap_row_range(&breaks, 1, cells.len());
        assert_eq!(text_of(&cells[s..e]), "ccc");
        assert_eq!(char_at_cell(&cells, s, len), 11);
    }

    /// An overlay reaching onto later lines draws on the first and blanks the
    /// rest, and core has to agree or a cursor on a row that is empty on screen
    /// would be reported somewhere in the middle of it.
    #[test]
    fn a_multi_line_overlay_blanks_the_lines_after_the_one_it_draws_on() {
        // "abc\ndef\n": line 0 is chars [0,3), line 1 is [4,7). The overlay runs
        // from 'b' to 'e'.
        let ovs = overlays(&[(1, 1, 6, "X")]);
        assert_eq!(text_of(&line_cells("abc", 4, ovs.all(), 0, 3)), "aX");
        assert_eq!(text_of(&line_cells("def", 4, ovs.all(), 4, 7)), "f");
        // ...and a line the overlay does not touch is untouched.
        assert_eq!(text_of(&line_cells("ghi", 4, ovs.all(), 8, 11)), "ghi");
    }

    /// A line with no characters is the one place a substitution had nothing to
    /// attach to, and it was invisible twice over: the filter dropped the
    /// overlay because `o.start < end` cannot hold when `start == end`, and
    /// [`substitute`] walks cells, of which a blank line has none. So ghost text
    /// on an empty line, a separator rule, an equation on a line of its own —
    /// all drew nothing at all.
    #[test]
    fn a_display_string_draws_on_a_line_with_no_characters() {
        // "a\n\nb": line 1 is the blank one, chars [2, 2), and an overlay on it
        // can only be the newline it ends with — which is how Lisp makes one.
        let ovs = overlays(&[(1, 2, 3, "— — —")]);
        let cells = line_cells("", 4, ovs.all(), 2, 2);
        assert_eq!(text_of(&cells), "— — —");
        // The cursor on that line is on char 0 of it, which is the front of what
        // replaced nothing, and the line is as wide as what is drawn.
        assert_eq!(visual_col(&cells, 0), 0);
        assert_eq!(char_at_cell(&cells, 3, 0), 0);
        motion_is_total(&cells, 0);

        // ...and the bend is only for the blank line. A zero-length overlay is
        // still nothing — it "ended on the line before" like any other — and so
        // is one that merely ends where this line starts.
        let mut empty = Overlays::default();
        empty.add(1, 2, 2);
        empty.edit(OverlayEdit::Display(1, Some("x".into())));
        assert!(text_of(&line_cells("", 4, empty.all(), 2, 2)).is_empty());
        assert!(text_of(&line_cells("", 4, overlays(&[(1, 0, 2, "x")]).all(), 2, 2)).is_empty());

        // An overlay *through* the blank line is a continuation row: it draws on
        // its own first line and blanks the rest, and a row that is blank on
        // screen has to stay blank here or a cursor would be reported inside it.
        assert!(text_of(&line_cells("", 4, overlays(&[(1, 0, 5, "x")]).all(), 2, 2)).is_empty());
    }

    /// The documented ceiling, asserted so it is a decision and not a surprise:
    /// core leaves an image overlay's text alone, because the cells a bitmap
    /// reserves are pixels over a cell width it has not got.
    #[test]
    fn core_does_not_try_to_size_an_image() {
        let mut ovs = Overlays::default();
        ovs.add(1, 4, 9);
        ovs.edit(OverlayEdit::Image(1, Some(7)));
        let line = "see $x^2$ here";
        let cells = line_cells(line, 4, ovs.all(), 0, line.chars().count());
        assert_eq!(text_of(&cells), line);
        // A `display` string on the *same* overlay is still laid out, which is
        // what the renderer falls back to before the bitmap has been rasterised.
        ovs.edit(OverlayEdit::Display(1, Some("[eq]".into())));
        let cells = line_cells(line, 4, ovs.all(), 0, line.chars().count());
        assert_eq!(text_of(&cells), "see [eq] here");
    }

    /// Clipped, rebased and sorted — the contract [`substitute`] is written
    /// against, and the one thing the renderer relies on when it appends these
    /// behind its own image substitutions.
    #[test]
    fn display_subs_are_clipped_to_the_line_and_sorted_by_start() {
        // Line 1 of a buffer is chars [6, 10).
        let ovs = overlays(&[(1, 8, 9, "b"), (2, 0, 20, "a"), (3, 10, 12, "c")]);
        assert_eq!(
            display_subs(ovs.all(), 6, 10),
            // The first covers the whole line and starts before it, so it is a
            // continuation row and blanks; the second is rebased onto it; the
            // third only touches the newline and the line after it.
            vec![(0, 4, String::new()), (2, 3, "b".to_string())]
        );
        // A face-only overlay hides nothing and is not a substitution.
        let mut plain = Overlays::default();
        plain.add(9, 0, 4);
        assert!(display_subs(plain.all(), 0, 4).is_empty());
    }

    /// End to end, because the claim is about a *motion* and not about a
    /// function: `j` from a body line onto a substituted heading lands in the
    /// column the heading is really drawn in.
    #[test]
    fn j_lands_in_the_column_the_substituted_line_draws() {
        let mut ed = crate::Editor::new();
        ed.mode = crate::Mode::Normal;
        ed.buffer = crate::Buffer::from_str("body text here\n** a heading\n");
        ed.settings.line_overflow = crate::LineOverflow::Wrap;
        ed.wrap_cols = 40; // what the renderer parks there every frame
        // org-modern: line 1 starts at char 15, and its two stars are a bullet.
        ed.buffer.overlays.add(1, 15, 17);
        ed.buffer.overlays.edit(OverlayEdit::Display(1, Some("◉".into())));

        ed.buffer.cursor = 3; // line 0, column 3
        assert_eq!(ed.cursor_vcol(), 3);
        // Column 3 of "◉ a heading" is the space after 'a' — source char 4 of
        // the line, char 19 of the buffer. Laid out without the bullet it would
        // have been char 18, which is drawn in column *2*: one column left of
        // where the block goes, which is the whole bug.
        let at = ed.visual_target(true, 1, ed.cursor_vcol());
        assert_eq!(at, 19);
        ed.buffer.cursor = at;
        assert_eq!(ed.cursor_vcol(), 3, "and it is still column 3 once it lands");

        // Back up again: `k` from there returns to the column it left.
        assert_eq!(ed.visual_target(false, 1, ed.cursor_vcol()), 3);
    }
}
