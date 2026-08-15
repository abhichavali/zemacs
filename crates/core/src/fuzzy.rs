//! The matcher behind every completing prompt.
//!
//! One algorithm, used by `M-x`, the buffer switcher, find-file, project-file,
//! `consult-line` and every Lisp `completing-read`: an alignment search that
//! asks *where in this candidate does the query fit best*, rather than the
//! first place it fits at all.
//!
//! The difference is the whole point of a fuzzy finder. A greedy left-to-right
//! subsequence scan matches `abc` against `a-b-xxabc` at positions 0, 2 and 8 —
//! three scattered letters — and never sees the literal `abc` sitting at the
//! end. Scoring that alignment says "scattered", so the candidate ranks below
//! things that are worse. Here every placement is considered and the best one
//! is the score, so a run of adjacent characters, a word start, or a camelCase
//! hump is found wherever it is.
//!
//! The units are [fzf's]: a match is worth [`MATCH`], the *context* a character
//! lands in is worth a bonus on top of it, and skipping characters between two
//! matches costs. Ranking is therefore about position and adjacency rather than
//! about how many characters happened to line up, which is what makes `sb` find
//! `switch-buffer` ahead of `superbly-fine`.
//!
//! [fzf's]: https://github.com/junegunn/fzf/blob/master/src/algo/algo.go
//!
//! **Case is smart**: an all-lowercase query matches either case, and a query
//! with a capital in it means the capital. That is fzf's rule, and it is the
//! only way one prompt can serve both `readme` finding `README.md` and `Foo`
//! finding `Foo` rather than `foo` in a `consult-line` over source.
//!
//! Allocation happens once per prompt, not once per candidate: a [`Matcher`]
//! owns its scratch and is reused across the whole list. `consult-line` runs
//! this over every line of the buffer on every keystroke, so the per-candidate
//! path is a scan and a fill of buffers that are already the right size.

/// One matched character.
const MATCH: i32 = 16;
/// The first character of a word — after a `-`, `_`, `/`, `.` or a space. Half
/// a match, so context is a real term in the ranking without ever outweighing
/// having matched at all.
const BOUNDARY: i32 = 8;
/// The `B` of `fooBar` and the `8` of `utf8`: a boundary with no delimiter to
/// announce it, and worth a shade less than one that has.
const CAMEL: i32 = 7;
/// Two matched characters side by side. Exactly the cost of the gap it avoids,
/// so a contiguous run is preferred by the width of the hole it would leave.
const CONSECUTIVE: i32 = 4;
/// Opening a gap between two matched characters, and each further character
/// skipped inside it.
const GAP_START: i32 = -3;
const GAP_EXTEND: i32 = -1;
/// The first matched character's context counts double. A query is usually the
/// start of a word someone has in mind, so *where it starts* says more about
/// the candidate than where the rest of it lands.
const FIRST: i32 = 2;

/// Below any reachable score: "there is no alignment ending here".
const NONE: i32 = i32::MIN / 4;

/// Candidates wider than this are scored by the greedy alignment rather than
/// the best one. The grid is `needle × window` cells, and a minified line or a
/// 200KB single-line JSON is a window nothing should be quadratic in.
// ponytail: a fixed cap, so a very long line ranks by a worse alignment than
// a short one would. Upgrade path: run the grid over a sliding window if
// anyone ever files a bug about a long line ranking oddly.
const WIDE: usize = 1024;

/// What a character is, for deciding whether the one after it starts a word.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Start of string, and whitespace — the strongest boundary there is.
    White,
    /// `-`, `_`, `/`, `.`, and every other separator.
    Punct,
    Digit,
    Lower,
    Upper,
}

/// ASCII first, and not as a micro-optimisation: this runs once per character
/// of every candidate, and a `consult-line` prompt over a large file is
/// millions of characters per keystroke. `char::is_lowercase` and its siblings
/// are Unicode table lookups.
fn class(c: char) -> Class {
    match c {
        'a'..='z' => Class::Lower,
        'A'..='Z' => Class::Upper,
        '0'..='9' => Class::Digit,
        ' ' | '\t' | '\n' | '\r' => Class::White,
        // Everything that is not a letter, a digit or a space: `-`, `_`, `/`,
        // `.`, `(`. CJK lands here too, which costs it a boundary bonus it
        // would not have earned anyway — there are no word starts to find.
        c if c.is_ascii() => Class::Punct,
        c if c.is_whitespace() => Class::White,
        c if c.is_numeric() => Class::Digit,
        c if c.is_lowercase() => Class::Lower,
        c if c.is_uppercase() => Class::Upper,
        _ => Class::Punct,
    }
}

/// What landing on a character of class `cur`, just after one of class `prev`,
/// is worth beyond the match itself.
fn bonus(prev: Class, cur: Class) -> i32 {
    match (prev, cur) {
        // A word start. Whitespace is the loudest one — `main` in `pub fn main`
        // beats `main` in `remaining`.
        (Class::White, c) if c != Class::White => BOUNDARY + 2,
        (Class::Punct, c) if c != Class::Punct => BOUNDARY,
        (Class::Lower, Class::Upper) => CAMEL,
        (p, Class::Digit) if p != Class::Digit => CAMEL,
        _ => 0,
    }
}

/// The lowercase of `c` as *one* character.
///
/// `char::to_lowercase` is an iterator because a few characters lower to
/// several — İ is the one everybody quotes. Taking the first keeps the lowered
/// text index-for-index with the original, which is what lets a matched
/// position be handed back to a renderer that is drawing the original.
fn lower(c: char) -> char {
    match c {
        'A'..='Z' => (c as u8 + 32) as char,
        c if c.is_ascii() => c,
        c => c.to_lowercase().next().unwrap_or(c),
    }
}

/// A query, compiled once and run against many candidates.
pub struct Matcher {
    /// The query, lowered unless it asked for case.
    needle: Vec<char>,
    /// The query had a capital in it, so case is significant — smart case.
    cased: bool,
    /// The candidate's window, as characters, lowered to match `needle`.
    /// Indexed from the window's own start, not the candidate's.
    hay: Vec<char>,
    /// [`bonus`] at each position of `hay`, from the *original* case: lowering
    /// the text loses the camelCase humps, so this is computed on the way in.
    bonus: Vec<i32>,
    /// Where each query character lands in the greedy left-to-right match —
    /// absolute positions in the candidate, and the rejection test on the way
    /// to them.
    firsts: Vec<usize>,
    /// And where each lands coming greedily from the other end. The pair bounds
    /// one row of the grid: query character `i` cannot land before `firsts[i]`
    /// (everything before it has to fit) and cannot land after `lasts[i]`
    /// (everything after it has to), so the rest of the row is cells no
    /// alignment can reach.
    ///
    /// This is most of the speed. `handle` against a line ending in `Response`
    /// spans forty columns, because `e` is a common letter and the window runs
    /// to the last one — but `h`, `a`, `n` and `d` are pinned to a few columns
    /// each, and without this every row would be walked as if it were the
    /// widest one.
    lasts: Vec<usize>,
    /// `needle.len() × width` best-scores: `grid[i * width + j]` is the best
    /// total for matching the query up to and including character `i`, with
    /// that character landing on candidate column `start + j`.
    ///
    /// Grown and never blanked, so a row's cells outside `firsts[i]..=lasts[i]`
    /// hold whatever the previous candidate left there. Every read is bounded
    /// by that span, which is what makes reuse safe — and what makes it worth
    /// having.
    grid: Vec<i32>,
    start: usize,
    width: usize,
}

impl Matcher {
    pub fn new(needle: &str) -> Self {
        let cased = needle.chars().any(char::is_uppercase);
        Self {
            needle: needle
                .chars()
                .map(|c| if cased { c } else { lower(c) })
                .collect(),
            cased,
            hay: Vec::new(),
            bonus: Vec::new(),
            firsts: Vec::new(),
            lasts: Vec::new(),
            grid: Vec::new(),
            start: 0,
            width: 0,
        }
    }

    /// An empty query, which matches everything with nothing to say about the
    /// order. Callers use it to skip the whole pass.
    pub fn is_empty(&self) -> bool {
        self.needle.is_empty()
    }

    /// How well `hay` matches, or `None` when it does not match at all.
    pub fn score(&mut self, hay: &str) -> Option<i32> {
        if self.needle.is_empty() {
            return Some(0);
        }
        if !self.place(hay) {
            return None;
        }
        Some(match self.width > WIDE {
            true => self.greedy_score(),
            false => {
                self.fill();
                self.best().1
            }
        })
    }

    /// The character ranges of the best alignment, as half-open indices into
    /// `hay`'s characters, so a renderer can show *why* a candidate matched.
    ///
    /// Computed on demand for the handful of rows on screen rather than kept
    /// for every candidate — a `consult-line` over a large buffer has hundreds
    /// of thousands of matches and forty of them are visible.
    pub fn spans(&mut self, hay: &str) -> Vec<(usize, usize)> {
        if self.needle.is_empty() || !self.place(hay) {
            return Vec::new();
        }
        // The greedy placement is already the answer for a candidate too wide
        // to align properly, and it is what `score` ranked it by.
        if self.width > WIDE {
            return runs(&self.firsts);
        }
        self.fill();
        let mut at = self.best().0;
        let mut cols = vec![self.start + at];
        for i in (1..self.needle.len()).rev() {
            at = self.parent(i, at);
            cols.push(self.start + at);
        }
        cols.reverse();
        runs(&cols)
    }

    /// Find the window worth searching, and fill the scratch with just that.
    ///
    /// One pass over the candidate, which is the pass that decides whether
    /// anything else happens at all: a query that is not a subsequence leaves
    /// here having allocated nothing and written nothing.
    ///
    /// `firsts[i]` is the earliest position query character `i` can take given
    /// the ones before it — the greedy leftmost placement. That is not the
    /// answer (the grid is), but it bounds one end of the search: no alignment
    /// can put character `i` before it. The other end is the *last* place the
    /// query's final character occurs, because an alignment is free to slide
    /// right and often should — `cf` matches `café_foo` at the `f` of `foo`,
    /// which the greedy placement walks straight past.
    ///
    /// Only that window is copied into the scratch, and everything downstream
    /// indexes into it rather than into the candidate. On a `consult-line` over
    /// a large file the difference is the whole line versus the handful of
    /// characters the query spans, once per line per keystroke.
    fn place(&mut self, hay: &str) -> bool {
        let (m, cased) = (self.needle.len(), self.cased);
        let last = self.needle[m - 1];
        self.firsts.clear();
        // The class of the character *before* the window, which is what decides
        // whether the window's first character starts a word.
        let mut before = Class::White;
        let mut prev = Class::White;
        let mut byte_start = 0;
        let mut end = 0;
        let mut done = 0;
        for (at, (byte, c)) in hay.char_indices().enumerate() {
            let k = if cased { c } else { lower(c) };
            if done < m {
                if k == self.needle[done] {
                    if done == 0 {
                        (before, byte_start, self.start) = (prev, byte, at);
                    }
                    self.firsts.push(at);
                    done += 1;
                    end = at;
                }
                // Only until the window opens: after that `before` is settled
                // and the second pass computes the classes inside it.
                if done == 0 {
                    prev = class(c);
                }
            } else if k == last {
                end = at;
            }
        }
        if done < m {
            return false;
        }
        self.width = end - self.start + 1;
        self.hay.clear();
        self.bonus.clear();
        let mut p = before;
        for c in hay[byte_start..].chars().take(self.width) {
            let k = class(c);
            self.bonus.push(bonus(p, k));
            self.hay.push(if cased { c } else { lower(c) });
            p = k;
        }
        // The same placement from the right. It cannot fail: `firsts` is
        // already a valid alignment, so every character has somewhere to go at
        // or after where the forward pass put it.
        self.lasts.clear();
        self.lasts.resize(m, 0);
        let mut k = self.width - 1;
        for i in (0..m).rev() {
            while self.hay[k] != self.needle[i] {
                k -= 1;
            }
            self.lasts[i] = self.start + k;
            if i > 0 {
                k -= 1;
            }
        }
        true
    }

    /// Score the greedy placement, for a candidate too wide for the grid.
    fn greedy_score(&self) -> i32 {
        let mut total = 0;
        for (i, &col) in self.firsts.iter().enumerate() {
            let adjacent = i > 0 && col == self.firsts[i - 1] + 1;
            let b = self.bonus[col - self.start];
            total += MATCH
                + match (i, adjacent) {
                    (0, _) => b * FIRST,
                    (_, true) => b.max(CONSECUTIVE),
                    (_, false) => b + GAP_START,
                };
        }
        total
    }

    /// Fill the grid: every placement of every query character, scored.
    ///
    /// Row `i` only starts at `firsts[i]` — the query's first `i` characters
    /// have to fit before it — and the whole grid stops at the last row's
    /// greedy landing, so the work is the window the query spans and not the
    /// candidate.
    ///
    /// `carry` is what keeps this linear. The alternative to extending a run is
    /// jumping from *any* earlier column, and re-scanning for the best one per
    /// cell would be quadratic in the window; instead the best-so-far is
    /// carried along and decayed by [`GAP_EXTEND`] each column, which is
    /// exactly what a longer gap costs.
    fn fill(&mut self) {
        let (m, width, start) = (self.needle.len(), self.width, self.start);
        // Grown, never cleared. Every cell the walk below can *read* is one the
        // walk above wrote, so blanking the grid first would be a second write
        // of `needle × window` cells per candidate for nothing — and that is the
        // one cost here that is paid even by candidates the query barely
        // touches.
        if self.grid.len() < m * width {
            self.grid.resize(m * width, NONE);
        }
        for i in 0..m {
            let want = self.needle[i];
            let row = i * width;
            let (lo, hi) = (self.firsts[i] - start, self.lasts[i] - start);
            // The row above, and the only columns of it that hold anything.
            let (plo, phi) = match i {
                0 => (lo, lo),
                _ => (self.firsts[i - 1] - start, self.lasts[i - 1] - start),
            };
            let mut carry = NONE;
            // Where this row can first *land* is not where the walk starts: the
            // row above begins two columns earlier at the latest, and `carry`
            // has to have absorbed it by the time the walk reaches `lo`.
            // Starting at `lo` was a matcher that could only ever extend a run
            // — it lost every alignment with a gap in it.
            for j in plo..=hi {
                // Everything in the row above up to `j - 2`, plus the gap it
                // would have to jump. `j - 1` is the consecutive case below.
                if i > 0 && j >= plo + 2 {
                    carry = carry.saturating_add(GAP_EXTEND);
                    if j - 2 <= phi {
                        let jump = self.grid[row - width + j - 2].saturating_add(GAP_START);
                        carry = carry.max(jump);
                    }
                }
                if j < lo {
                    continue;
                }
                if self.hay[j] != want {
                    self.grid[row + j] = NONE;
                    continue;
                }
                let b = self.bonus[j];
                self.grid[row + j] = match i {
                    // Nothing before it, and nothing skipped before it either:
                    // where the query starts inside a candidate is a fact about
                    // context, which `b` already prices.
                    0 => MATCH + b * FIRST,
                    _ => {
                        let run = match j > plo && j - 1 <= phi {
                            true => self.grid[row - width + j - 1],
                            false => NONE,
                        };
                        let side_by_side = run.saturating_add(MATCH + b.max(CONSECUTIVE));
                        let after_gap = carry.saturating_add(MATCH + b);
                        side_by_side.max(after_gap)
                    }
                };
            }
        }
    }

    /// The best cell of the last row: where the query's final character lands
    /// in the winning alignment, and what that alignment scored.
    /// Only from the last row's own earliest landing: anything before that is a
    /// cell [`Matcher::fill`] never walked, and the grid is not blanked.
    fn best(&self) -> (usize, i32) {
        let m = self.needle.len();
        let (row, lo) = ((m - 1) * self.width, self.firsts[m - 1] - self.start);
        self.grid[row + lo..row + self.width]
            .iter()
            .enumerate()
            .max_by_key(|&(j, &s)| (s, std::cmp::Reverse(j)))
            .map(|(j, &s)| (j + lo, s))
            .unwrap_or((lo, 0))
    }

    /// Which column query character `i - 1` took, given that character `i` took
    /// `j`. Recomputed from the grid rather than recorded during [`fill`]: a
    /// parent per cell is a second grid written for every candidate, and this
    /// is only ever asked about the rows on screen.
    fn parent(&self, i: usize, j: usize) -> usize {
        let above = (i - 1) * self.width;
        let want = self.grid[i * self.width + j];
        let b = self.bonus[j];
        let plo = self.firsts[i - 1] - self.start;
        let phi = self.lasts[i - 1] - self.start;
        if j > plo && j - 1 <= phi {
            let run = self.grid[above + j - 1].saturating_add(MATCH + b.max(CONSECUTIVE));
            if run == want {
                return j - 1;
            }
        }
        // Otherwise it came over a gap, from whichever earlier column paid for
        // it best — the column `carry` was holding when `fill` reached `j`.
        let stop = phi.min(j.saturating_sub(2));
        match j >= 2 && plo <= stop {
            true => (plo..=stop)
                .max_by_key(|&k| self.grid[above + k] + GAP_EXTEND * (j - 2 - k) as i32)
                .unwrap_or(plo),
            false => plo,
        }
    }
}

/// Sorted columns to half-open ranges, merging the ones that touch.
fn runs(cols: &[usize]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &c in cols {
        match out.last_mut() {
            Some(last) if last.1 == c => last.1 = c + 1,
            _ => out.push((c, c + 1)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(needle: &str, hay: &str) -> Option<i32> {
        Matcher::new(needle).score(hay)
    }

    /// The candidate `needle` prefers, out of `hay`.
    fn best<'a>(needle: &str, hay: &[&'a str]) -> &'a str {
        let mut m = Matcher::new(needle);
        let mut scored: Vec<(i32, &str)> = hay.iter().filter_map(|h| Some((m.score(h)?, *h))).collect();
        scored.sort_by_key(|&(s, h)| (std::cmp::Reverse(s), h.len()));
        scored[0].1
    }

    #[test]
    fn a_query_that_is_not_a_subsequence_does_not_match() {
        assert!(score("xyz", "switch-buffer").is_none());
        assert!(score("bs", "switch-buffer").is_none()); // order still counts
        assert_eq!(score("", "anything"), Some(0));
    }

    /// The reason this is not a greedy scan. Greedy takes `a` at 1, `b` at 3
    /// and `c` at 8 and calls it scattered; the literal `abc` is right there.
    #[test]
    fn the_best_alignment_wins_not_the_leftmost_one() {
        assert_eq!(Matcher::new("abc").spans("xaxbxxabc"), vec![(6, 9)]);
        assert!(score("abc", "xaxbxxabc").unwrap() > score("abc", "xaxbxxxxc").unwrap());
    }

    #[test]
    fn word_starts_outrank_the_middle_of_a_word() {
        assert_eq!(best("sb", &["superbly-fine", "switch-buffer"]), "switch-buffer");
        assert_eq!(best("ff", &["off-of", "find-file"]), "find-file");
        assert_eq!(best("main", &["remaining", "pub fn main"]), "pub fn main");
    }

    #[test]
    fn a_prefix_beats_a_hit_further_in() {
        assert_eq!(best("buf", &["kill-buffer", "buffer-list"]), "buffer-list");
    }

    #[test]
    fn camel_case_is_a_boundary_even_without_a_delimiter() {
        assert_eq!(best("fb", &["fizzbuzz", "fooBar"]), "fooBar");
    }

    #[test]
    fn case_is_smart() {
        // lowercase asks for either
        assert!(score("readme", "README.md").is_some());
        // a capital asks for that capital
        assert!(score("Foo", "foo_bar").is_none());
        assert!(score("Foo", "let Foo = 1").is_some());
    }

    /// The spans are what the renderer paints, so they have to be indices into
    /// the candidate's own characters — including when those are not one byte.
    #[test]
    fn spans_are_character_indices_through_multibyte_text() {
        let mut m = Matcher::new("cf");
        assert_eq!(m.spans("café_foo"), vec![(0, 1), (5, 6)]);
        assert_eq!(Matcher::new("éf").spans("café_foo"), vec![(3, 4), (5, 6)]);
    }

    /// A very long candidate skips the grid, and still has to answer with a
    /// score and with spans that line up with each other.
    #[test]
    fn a_very_wide_candidate_falls_back_without_lying() {
        let long = format!("n{}d", "x ".repeat(WIDE));
        let mut m = Matcher::new("nd");
        let s = m.score(&long).unwrap();
        assert!(s > 0, "{s}");
        let spans = m.spans(&long);
        assert_eq!(spans.len(), 2, "{spans:?}");
        let chars: Vec<char> = long.chars().collect();
        assert_eq!(chars[spans[0].0], 'n');
        assert_eq!(chars[spans[1].0], 'd');
    }

    /// Reusing one matcher over a list must not let one candidate's scratch
    /// leak into the next one's answer.
    /// Every alignment, scored by hand with the same rules the grid uses.
    /// The grid exists to find the best one without enumerating them.
    fn brute(needle: &str, hay: &str) -> Option<i32> {
        let n: Vec<char> = needle.chars().collect();
        let h: Vec<char> = hay.chars().collect();
        let mut prev = Class::White;
        let b: Vec<i32> = h
            .iter()
            .map(|&c| {
                let k = class(c);
                let v = bonus(prev, k);
                prev = k;
                v
            })
            .collect();
        // Every strictly increasing placement of `n` into `h`, depth first.
        fn walk(n: &[char], h: &[char], b: &[i32], i: usize, from: usize, prev: Option<usize>) -> Option<i32> {
            if i == n.len() {
                return Some(0);
            }
            let mut best = None;
            for p in from..h.len() {
                if h[p].to_lowercase().next() != n[i].to_lowercase().next() {
                    continue;
                }
                let gain = MATCH
                    + match prev {
                        None => b[p] * FIRST,
                        Some(q) if q + 1 == p => b[p].max(CONSECUTIVE),
                        Some(q) => b[p] + GAP_START + GAP_EXTEND * (p - q - 2) as i32,
                    };
                if let Some(rest) = walk(n, h, b, i + 1, p + 1, Some(p)) {
                    best = Some(best.map_or(gain + rest, |x: i32| x.max(gain + rest)));
                }
            }
            best
        }
        walk(&n, &h, &b, 0, 0, None)
    }

    /// The grid against the definition of what it is computing, over enough
    /// shapes that a bad bound or an off-by-one in the traceback shows up.
    #[test]
    fn the_grid_agrees_with_brute_force() {
        // A tiny LCG: the cases have to be many and reproducible, and a
        // dependency for that would be absurd.
        let mut seed = 0x2545F491u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 33) as usize
        };
        let alphabet: Vec<char> = "ab-cA B1".chars().collect();
        for _ in 0..4000 {
            let hay: String = (0..next() % 14 + 1)
                .map(|_| alphabet[next() % alphabet.len()])
                .collect();
            let needle: String = (0..next() % 3 + 1)
                .map(|_| alphabet[next() % alphabet.len()])
                .collect();
            // Lowercase queries only: `brute` does not model smart case, which
            // `case_is_smart` covers on its own.
            let needle = needle.to_lowercase();
            assert_eq!(
                Matcher::new(&needle).score(&hay),
                brute(&needle, &hay),
                "needle {needle:?} hay {hay:?}"
            );
        }
    }

    /// And the spans have to describe the alignment the score came from, not
    /// some other one that happens to fit.
    #[test]
    fn the_spans_rebuild_the_alignment_that_was_scored() {
        let mut seed = 0x9E3779B9u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 33) as usize
        };
        let alphabet: Vec<char> = "ab-c d".chars().collect();
        for _ in 0..2000 {
            let hay: String = (0..next() % 14 + 1)
                .map(|_| alphabet[next() % alphabet.len()])
                .collect();
            let needle: String = (0..next() % 3 + 1)
                .map(|_| alphabet[next() % alphabet.len()])
                .collect();
            let mut m = Matcher::new(&needle);
            let Some(want) = m.score(&hay) else { continue };
            let cols: Vec<usize> = m
                .spans(&hay)
                .into_iter()
                .flat_map(|(s, e)| s..e)
                .collect();
            let h: Vec<char> = hay.chars().collect();
            let n: Vec<char> = needle.chars().collect();
            assert_eq!(cols.len(), n.len(), "needle {needle:?} hay {hay:?}");
            // the positions spell the query out, in order...
            for (k, &p) in cols.iter().enumerate() {
                assert_eq!(h[p].to_lowercase().next(), n[k].to_lowercase().next());
            }
            // ...and they are worth exactly what the score said.
            let mut prev = Class::White;
            let b: Vec<i32> = h
                .iter()
                .map(|&c| {
                    let k = class(c);
                    let v = bonus(prev, k);
                    prev = k;
                    v
                })
                .collect();
            let mut total = 0;
            for (k, &p) in cols.iter().enumerate() {
                total += MATCH
                    + match k {
                        0 => b[p] * FIRST,
                        _ if cols[k - 1] + 1 == p => b[p].max(CONSECUTIVE),
                        _ => b[p] + GAP_START + GAP_EXTEND * (p - cols[k - 1] - 2) as i32,
                    };
            }
            assert_eq!(total, want, "needle {needle:?} hay {hay:?} cols {cols:?}");
        }
    }

    #[test]
    fn a_matcher_is_reusable_across_candidates() {
        let mut m = Matcher::new("ff");
        let first = m.score("find-file").unwrap();
        assert!(m.score("no-match-here").is_none());
        assert!(m.score("xxxxxxxxxxff").is_some());
        assert_eq!(m.score("find-file"), Some(first));
    }
}
