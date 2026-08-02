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

/// Display rows a line of `len` cells occupies in a pane `cols` wide.
///
/// Always at least one, so an empty line still has a row for its gutter number
/// and its cursor. `cols == 0` — a pane narrower than its own line numbers — is
/// the infinite-loop case: it yields one row rather than dividing by zero, and
/// every caller advances past the line either way.
pub fn wrap_row_count(len: usize, cols: usize) -> usize {
    match cols {
        0 => 1,
        c => len.div_ceil(c).max(1),
    }
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
        assert_eq!(wrap_row_count(cells.len(), 4), 2);
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
}
