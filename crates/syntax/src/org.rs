//! Org-mode highlighting, by hand.
//!
//! There is no usable grammar to lean on here: `tree-sitter-org` on crates.io
//! was last published in 2022 and pins `tree-sitter >=0.19, <0.21`, four ABI
//! revisions behind the 0.26 the rest of this crate is built on. Vendoring and
//! regenerating it would be a lot of machinery for a format that is, by
//! construction, line-oriented — org decides what a line *is* by looking at its
//! first few characters, and nothing but `#+begin_`/`#+end_` blocks carries
//! state across lines. So: one pass, one line at a time.
//!
//! Everything is in **char** offsets from the start, never bytes — the rope and
//! the renderer index by char, and there is no conversion step to get wrong.
//!
//! The delicate part is that `*` means two different things. At column 0 and
//! followed by a space it opens a heading; mid-line it opens emphasis; in
//! `2 * 3` it is just multiplication. Org's real rule for an emphasis marker is
//! a triple: the character *before* the opener must be line-start, whitespace or
//! one of [`PRE`]; the body must neither begin nor end with whitespace (or a
//! comma); and the character *after* the closer must be line-end, whitespace or
//! one of [`POST`]. That is what [`emphasis_at`] implements, and it is also what
//! keeps `http://example.com/a/b/` from turning into italics — the first `/` is
//! preceded by `:`, which is in neither set.
//!
//! ponytail: emphasis is single-line (org allows one newline inside a run), and
//! the ceiling is a re-scan of the whole buffer per revision, same as the
//! tree-sitter path. Table rows are painted whole rather than split into cells.
//! Deliberately absent: drawers, timestamps, footnotes, tag and priority
//! cookies on headings, bare (unbracketed) URLs, and any highlighting of the
//! *contents* of a `#+begin_src` block in its own language — that needs a real
//! injection mechanism, so blocks are simply left plain rather than mis-read as
//! prose markup.

use zemacs_core::{HlKind, Span};

/// Characters that may sit immediately before an emphasis opener, besides
/// whitespace and the start of the line. Org's `org-emphasis-regexp-components`
/// default, minus the dashes we do not care to distinguish.
const PRE: &[char] = &['-', '(', '\'', '"', '{'];

/// Characters that may sit immediately after an emphasis closer, besides
/// whitespace and the end of the line.
const POST: &[char] = &['-', '.', ',', ':', '!', '?', ';', '\'', '"', ')', '}', '['];

/// The face each emphasis marker paints its *body* with.
///
/// `_underline_` and `+strike+` have no face of their own: underline borrows
/// [`HlKind::Italic`] (the other "quiet emphasis"), strike-through borrows
/// [`HlKind::Comment`], which is the dimmest thing in the theme and reads
/// correctly as deleted text.
fn marker_kind(c: char) -> Option<HlKind> {
    Some(match c {
        '*' => HlKind::Bold,
        '/' | '_' => HlKind::Italic,
        '=' | '~' => HlKind::Code,
        '+' => HlKind::Comment,
        _ => return None,
    })
}

/// Highlight org text. Spans come back sorted and non-overlapping by
/// construction: lines are visited in order, and within a line every span is
/// pushed left to right over a region the previous one already skipped past.
pub(crate) fn highlight(text: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let mut in_block = false;
    let mut base = 0;
    for raw in text.split('\n') {
        // ponytail: a `Vec<char>` per line. Random access by char index is what
        // makes the marker rules readable; the ceiling is allocation churn on
        // very large files, fixable with a reusable buffer if it ever shows up.
        let mut line: Vec<char> = raw.chars().collect();
        let stride = line.len() + 1; // + the '\n' that `split` ate
        if line.last() == Some(&'\r') {
            line.pop();
        }
        line_spans(&line, base, &mut in_block, &mut out);
        base += stride;
    }
    out
}

/// Classify one line and emit its spans. `in_block` carries `#+begin_`/`#+end_`
/// state across the call.
fn line_spans(line: &[char], base: usize, in_block: &mut bool, out: &mut Vec<Span>) {
    let indent = line
        .iter()
        .position(|c| !c.is_whitespace())
        .unwrap_or(line.len());
    let body = &line[indent..];
    let Some(&first) = body.first() else {
        return; // blank line
    };

    // `#+...` is a directive; `# ...` is a comment. Getting these two the wrong
    // way round paints every directive grey, so they are tested separately.
    let directive = first == '#' && body.get(1) == Some(&'+');
    if *in_block {
        // Inside a block only the terminator is interesting. An unterminated
        // block quietly swallows the rest of the file, which beats guessing.
        if directive && eq_ci(&body[2..], "end_") {
            *in_block = false;
            push(out, base, 0, line.len(), HlKind::Keyword);
        }
        return;
    }
    if directive {
        *in_block = eq_ci(&body[2..], "begin_");
        push(out, base, 0, line.len(), HlKind::Keyword);
        return;
    }
    if first == '#' && body.get(1).is_none_or(|c| c.is_whitespace()) {
        push(out, base, 0, line.len(), HlKind::Comment);
        return;
    }

    // A heading owns its whole line: stars, text, and any `*bold*` inside it.
    // Two faces over the same characters would break the non-overlap contract,
    // and the heading is the one the reader is scanning for.
    if indent == 0 {
        let stars = line.iter().take_while(|&&c| c == '*').count();
        if stars > 0 && line.get(stars) == Some(&' ') {
            heading(line, base, stars, out);
            return;
        }
    }

    // A table row goes to the structure face whole, exactly as `org-table`
    // does in Emacs. Not laziness for its own sake: a rule like
    // `|----+----------+----|` otherwise reads as a `+strike+` run.
    if first == '|' {
        push(out, base, indent, line.len(), HlKind::Punctuation);
        return;
    }

    let bullet = bullet_len(body).unwrap_or(0);
    push(out, base, indent, indent + bullet, HlKind::Punctuation);
    inline(line, indent + bullet, base, out);
}

/// `* Heading`, `** TODO Sub-heading`. Level 1/2/3-or-deeper.
fn heading(line: &[char], base: usize, stars: usize, out: &mut Vec<Span>) {
    let kind = match stars {
        1 => HlKind::Heading1,
        2 => HlKind::Heading2,
        _ => HlKind::Heading3,
    };
    let text = stars + 1;
    let end = text + line[text..].iter().position(|c| *c == ' ').unwrap_or(line.len() - text);
    // ponytail: only the two built-in keywords. Real org reads the todo
    // sequence out of `#+TODO:` / `org-todo-keywords`; wiring that up means
    // parsing the buffer's own configuration first.
    let word: String = line[text..end].iter().collect();
    let keyword = match word.as_str() {
        "TODO" => HlKind::Constant,
        "DONE" => HlKind::Comment,
        _ => {
            push(out, base, 0, line.len(), kind);
            return;
        }
    };
    push(out, base, 0, text, kind);
    push(out, base, text, end, keyword);
    push(out, base, end, line.len(), kind);
}

/// Length of a `-`, `+`, `1.` or `1)` list bullet at the start of `body`.
///
/// `*` bullets are not recognized: indented `* item` and indented `*bold* item`
/// are genuinely ambiguous, and guessing wrong on emphasis is the louder error.
fn bullet_len(body: &[char]) -> Option<usize> {
    let spaced = |n: usize| body.get(n).is_some_and(|c| c.is_whitespace());
    if matches!(body[0], '-' | '+') && spaced(1) {
        return Some(1);
    }
    let digits = body.iter().take_while(|c| c.is_ascii_digit()).count();
    (digits > 0 && matches!(body.get(digits), Some('.' | ')')) && spaced(digits + 1))
        .then_some(digits + 1)
}

/// Walk the rest of a prose line, emitting links and emphasis runs.
fn inline(line: &[char], from: usize, base: usize, out: &mut Vec<Span>) {
    let mut i = from;
    while i < line.len() {
        if let Some(next) = link_at(line, i, base, out) {
            i = next;
        } else if let Some((close, kind)) = emphasis_at(line, i) {
            push(out, base, i, i + 1, HlKind::Markup);
            push(out, base, i + 1, close, kind);
            push(out, base, close, close + 1, HlKind::Markup);
            i = close + 1; // markup does not nest: skip the whole body
        } else {
            i += 1;
        }
    }
}

/// An emphasis run opening at `i`, as (index of the closing marker, body face).
///
/// See the module docs for the pre/body/post rule this is implementing.
fn emphasis_at(line: &[char], i: usize) -> Option<(usize, HlKind)> {
    let c = line[i];
    let kind = marker_kind(c)?;
    if !(i == 0 || line[i - 1].is_whitespace() || PRE.contains(&line[i - 1])) {
        return None;
    }
    let &open = line.get(i + 1)?;
    if !border(open) || open == c {
        return None; // `* 3` (arithmetic), `**` (empty body)
    }
    (i + 2..line.len())
        .find(|&j| {
            line[j] == c
                && border(line[j - 1])
                && line.get(j + 1).is_none_or(|&n| n.is_whitespace() || POST.contains(&n))
        })
        .map(|j| (j, kind))
}

/// Org's BORDER: an emphasis body may not begin or end with whitespace, nor
/// with a comma.
fn border(c: char) -> bool {
    !c.is_whitespace() && c != ','
}

/// `[[target]]` or `[[target][description]]`, emitting its spans and returning
/// the index one past the closing `]]`.
fn link_at(line: &[char], i: usize, base: usize, out: &mut Vec<Span>) -> Option<usize> {
    if line[i] != '[' || line.get(i + 1) != Some(&'[') {
        return None;
    }
    let open = i + 2;
    let pair = |j: usize, c: char| line[j] == ']' && line.get(j + 1) == Some(&c);
    let close = (open..line.len()).find(|&j| pair(j, ']'))?;
    if close == open {
        return None; // `[[]]` — nothing to link to
    }
    push(out, base, i, open, HlKind::Markup);
    match (open..close).find(|&j| pair(j, '[')) {
        Some(split) => {
            push(out, base, open, split, HlKind::Link);
            push(out, base, split, split + 2, HlKind::Markup);
            push(out, base, split + 2, close, HlKind::Link);
        }
        None => push(out, base, open, close, HlKind::Link),
    }
    push(out, base, close, close + 2, HlKind::Markup);
    Some(close + 2)
}

/// Push a span at line-relative `[start, end)`, dropping empty ones so callers
/// can hand over degenerate ranges without checking first.
fn push(out: &mut Vec<Span>, base: usize, start: usize, end: usize, kind: HlKind) {
    if start < end {
        out.push(Span {
            start: base + start,
            end: base + end,
            kind,
        });
    }
}

/// Does `line` start with `word` (ASCII, case-insensitively)? `#+BEGIN_SRC` and
/// `#+begin_src` are the same directive.
fn eq_ci(line: &[char], word: &str) -> bool {
    line.len() >= word.len()
        && line
            .iter()
            .zip(word.chars())
            .all(|(a, b)| a.eq_ignore_ascii_case(&b))
}
