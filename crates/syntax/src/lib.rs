//! zemacs-syntax — tree-sitter highlighting, flattened into [`zemacs_core::Span`].
//!
//! The whole crate is three functions ([`language_for_path`], [`highlight`],
//! [`languages`]) over one idea: run a `highlights.scm` over a parse tree, keep
//! the innermost capture covering each byte, and convert byte offsets to char
//! offsets, which is what the rope and the renderer index by. Most of those
//! queries are the grammar's own; `lisp.scm`, `python.scm` and `json.scm` are
//! ours, and each says in its header why.
//!
//! The exception is org, which has no grammar we can use (see [`org`]) and is
//! scanned by hand, a line at a time, straight into char offsets.
//!
//! Design rules:
//!
//! * **Never fail loudly.** An unknown language, a query that won't compile, a
//!   grammar that panics on garbage input — all of it degrades to "no spans",
//!   i.e. plain text. Bad highlighting must never take the editor down.
//! * **Build each [`Query`] once.** Compiling a query is milliseconds;
//!   highlighting runs on every buffer revision.
//! * A grammar that will not compile against our `tree-sitter` version is
//!   dropped rather than pinning the workspace backwards.
//!
//! # Why this stopped using `tree-sitter-highlight`
//!
//! That crate parses the whole source on every call and offers no way to hand
//! it a tree — the `Tree` it builds lives inside its iterator and dies with it.
//! Incremental reparsing is the one optimisation that matters here, and it is
//! unreachable through that API, so [`spans`] below does the flattening
//! instead. Owning the flattening turned out to buy the second optimisation as
//! well: [`requery`] runs the query over the part of the tree that moved and
//! splices, which needs both the previous spans and a say in how they are put
//! together. With no injection and no locals query — which is what this crate
//! configured either way, "lexical color only" — the algorithm it replaces
//! reduces to three rules, all of them in [`spans`]: captures arrive in tree
//! order, the last capture on a node wins, and a capture nested inside another
//! covers it for as long as it lasts.

mod org;

pub use org::{bullets, latex_fragment_at, latex_fragments, Bullet, BulletKind, LatexFragment};

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use tree_sitter::{
    InputEdit, Language, Parser, Point, Query, QueryCursor, StreamingIterator, Tree,
};
use zemacs_core::{BufferId, Change, HlKind, Span};

/// The capture names we recognize, and the [`HlKind`] each folds onto.
///
/// [`kind_for`] matches a query's capture name against this list by
/// dot-separated parts, longest match wins: `function.macro` lands on
/// `function` and `punctuation.bracket` on `punctuation`, while a name spelled
/// out here in full takes its own face instead of its parent's. That last part
/// is the whole mechanism for saying "`len` is not one of your functions" and
/// "a docstring is prose" without adding a face for either — a face costs a
/// variant in [`HlKind`], a line in every one of the eleven themes, and a
/// colour nobody had picked. Capture names absent from this list (`embedded`,
/// `spell`, ...) simply get no highlight.
const CAPTURES: &[(&str, HlKind)] = &[
    ("attribute", HlKind::Constant),
    ("boolean", HlKind::Constant),
    ("comment", HlKind::Comment),
    ("constant", HlKind::Constant),
    ("constructor", HlKind::Type),
    ("delimiter", HlKind::Punctuation),
    ("escape", HlKind::String),
    ("function", HlKind::Function),
    ("function.builtin", HlKind::Constant),
    ("keyword", HlKind::Keyword),
    ("label", HlKind::Constant),
    ("number", HlKind::Number),
    ("operator", HlKind::Operator),
    ("property", HlKind::Variable),
    ("punctuation", HlKind::Punctuation),
    ("string", HlKind::String),
    ("string.documentation", HlKind::Comment),
    ("type", HlKind::Type),
    ("variable", HlKind::Variable),
    // `self` in Rust, `this` and `super` in JavaScript, `self`/`cls` in Python:
    // every grammar's word for "the name the language gave you", and a keyword
    // in all three languages' own `-ts-mode`.
    ("variable.builtin", HlKind::Keyword),
];

/// Language id -> the file extensions that select it. First match wins, so the
/// order here is the order `languages()` reports.
///
/// All the Lisps share one id: the Common Lisp grammar is happy enough with
/// Elisp and Scheme for the purpose of coloring atoms, and one grammar is a lot
/// less to carry than three. `org` is the odd one out: it has no grammar at all,
/// [`highlight`] routes it to the hand-rolled scanner in [`org`].
const LANGS: &[(&str, &[&str])] = &[
    ("rust", &["rs"]),
    ("lisp", &["lisp", "cl", "lsp", "asd", "el", "scm", "ss", "sexp"]),
    ("python", &["py", "pyi"]),
    ("json", &["json"]),
    ("toml", &["toml"]),
    ("c", &["c", "h"]),
    ("javascript", &["js", "mjs", "cjs", "jsx"]),
    ("org", &["org"]),
];

/// Language id for a path, from its extension. `None` = plain text.
pub fn language_for_path(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    LANGS
        .iter()
        .find(|(_, exts)| exts.contains(&ext.as_str()))
        .map(|(id, _)| (*id).to_string())
}

/// The language ids this build supports.
pub fn languages() -> &'static [&'static str] {
    static IDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    IDS.get_or_init(|| LANGS.iter().map(|(id, _)| *id).collect())
}

/// Highlight `text` as `lang`. Spans come back sorted by `start`,
/// non-overlapping, in **char** offsets. Unknown language or parse failure
/// yields an empty `Vec`; runs with no highlight get no span at all, since the
/// renderer paints uncovered text in the default color.
///
/// A full parse every time, which is what a caller with no document to name
/// wants — a test, or anything holding a string rather than a buffer. Editing
/// goes through [`Session`], which is the same code with a tree kept.
pub fn highlight(lang: &str, text: &str) -> Vec<Span> {
    // A `Parser` is worth reusing across calls and cannot cross threads
    // safely, which is exactly what a thread-local is for.
    thread_local! {
        static ANON: RefCell<Session> = RefCell::new(Session::new());
    }
    ANON.with(|s| s.borrow_mut().highlight(None, lang, text, None))
}

/// Every structural range in `text` worth folding, as **1-based inclusive line
/// numbers**, outermost first.
///
/// Lines and not char offsets, deliberately: a fold hides whole lines, the
/// caller turns a line into an offset with `line-start`/`line-end` anyway, and
/// tree-sitter hands out a node's rows without anyone having to walk the text to
/// convert bytes to characters. The one conversion this feature would otherwise
/// have needed, avoided by asking the question in the units the answer is used
/// in.
///
/// The rule is *every named node spanning more than one line*, and there is no
/// `folds.scm` anywhere: a fold query per grammar is a file per language to
/// write and keep in step with upstream, where this is one predicate that works
/// on every grammar in the build and on the next one added. What it costs is
/// precision — a multi-line argument list is a node too, so it is offered as a
/// fold — and the caller picks which of the nested ranges it wants, which is the
/// policy half and belongs in Lisp.
///
/// Unnamed nodes are skipped, so a bare `{ … }` delimiter pair is not a fold of
/// its own beside the block it delimits.
///
/// "Spanning more than one line" is measured with [`end_row`] and not with the
/// node's own end row, which is a *cursor* and not a character — see there.
///
/// Unknown language, no grammar, or a parse failure yields an empty `Vec` — the
/// crate's "never fail loudly" rule, and the caller has one thing to check.
pub fn fold_ranges(lang: &str, text: &str) -> Vec<(usize, usize)> {
    thread_local! {
        static ANON: RefCell<Parser> = RefCell::new(Parser::new());
    }
    let Some(config) = config(lang) else {
        return Vec::new();
    };
    ANON.with(|p| {
        let mut parser = p.borrow_mut();
        if parser.set_language(&config.language).is_err() {
            return Vec::new();
        }
        let Some(tree) = parser.parse(text, None) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // Pre-order, which is what makes the list outermost-first: a caller
        // wanting the innermost range containing a line takes the *last* match,
        // and one wanting the top-level ones takes those no earlier range covers.
        //
        // Iterative rather than recursive because the depth here is the depth of
        // the *parse tree*, and a file of nothing but nested brackets is a stack
        // overflow in a crate whose rule is that bad input costs colour and never
        // costs the process.
        let mut cursor = tree.walk();
        let mut depth = 0usize;
        let mut down = true;
        loop {
            // `depth > 0` skips the root, which spans the file: folding it hides
            // everything, and it would swallow every other range a caller
            // filtered for the outermost ones.
            if down && depth > 0 {
                let node = cursor.node();
                let (a, b) = (node.start_position().row, end_row(&node));
                if node.is_named() && b > a {
                    out.push((a + 1, b + 1));
                }
            }
            if down && cursor.goto_first_child() {
                depth += 1;
                continue;
            }
            if cursor.goto_next_sibling() {
                down = true;
                continue;
            }
            if !cursor.goto_parent() {
                return out;
            }
            depth -= 1;
            down = false;
        }
    })
}

/// The last row a node actually puts a character on.
///
/// A tree-sitter end position is where the cursor stops, not where the last
/// character is, so a node that swallows its own trailing newline — which is
/// every `line_comment` in tree-sitter-rust, doc comments included — ends at
/// column 0 of the *following* row while occupying none of it. Taking that row
/// at face value made every single-line `//` comment look two rows tall, so
/// `fold-all` on any commented file folded each comment over its innocent
/// neighbour: a fold that hides a line having nothing to do with the thing you
/// folded, which is the one failure the "every named node" rule promised not to
/// have. Found by folding `crates/core/src/marker.rs` in the running editor,
/// where ten lines of `//!` header collapsed into five.
///
/// Costs nothing anywhere else: a node ending at `}` ends after it, at column 1
/// or more, and keeps its row.
fn end_row(node: &tree_sitter::Node) -> usize {
    let end = node.end_position();
    if end.column == 0 {
        end.row.saturating_sub(1)
    } else {
        end.row
    }
}

/// Flatten a tree's captures into sorted, non-overlapping **byte** spans.
///
/// The three rules, in order of appearance below:
///
/// * captures arrive in tree order, so a capture that starts at or after the
///   top of the stack ends it;
/// * every capture on one *node* collapses to the last of them, because the
///   query's later patterns are its more specific ones — and a last capture
///   that maps to no [`HlKind`] means the node gets nothing, rather than
///   falling back to the earlier pattern that did;
/// * a node's range is either disjoint from or wholly inside every earlier
///   one — they came out of a tree — so a stack is enough to know which
///   highlight is innermost, and no interval arithmetic is needed.
///
/// `range` is how much of the tree to look at, and `0..text.len()` is all of
/// it. A narrower one is the whole of [`requery`]: the cursor still walks from
/// the root, so a pattern rooted anywhere above the range is entered and
/// matched as usual, and what the range buys is not descending into the
/// subtrees that fall outside it. Captures landing outside are the caller's to
/// discard — a pattern rooted at the file root can put one anywhere.
fn spans(
    config: &Config,
    cursor: &mut QueryCursor,
    tree: &Tree,
    text: &str,
    range: std::ops::Range<usize>,
) -> Vec<Span> {
    // Set on every call and never inherited from the last one: the cursor
    // outlives this function, and a range left over from some other buffer's
    // edit would silently colour a fraction of the file.
    cursor.set_byte_range(range);
    // Read out before flattening rather than streamed, because deciding what a
    // node's highlight is means looking at the capture *after* it, and the
    // query cursor is a streaming iterator that cannot be peeked. One `usize`
    // quadruple per capture is a few MB on the largest file anyone edits here
    // and it is freed on the way out.
    let mut caps: Vec<(usize, usize, usize, u32)> = Vec::new();
    let mut it = cursor.captures(&config.query, tree.root_node(), text.as_bytes());
    while let Some((m, i)) = it.next() {
        let capture = m.captures[*i];
        let range = capture.node.byte_range();
        caps.push((capture.node.id(), range.start, range.end, capture.index));
    }

    let mut out: Vec<Span> = Vec::new();
    let mut stack: Vec<(usize, HlKind)> = Vec::new();
    let mut at = 0usize;
    let mut i = 0usize;
    while i < caps.len() {
        let (node, start, end, mut capture) = caps[i];
        while i + 1 < caps.len() && caps[i + 1].0 == node {
            i += 1;
            capture = caps[i].3;
        }
        i += 1;
        while let Some(&(ends, _)) = stack.last() {
            if ends > start {
                break;
            }
            paint(&mut out, &mut at, ends, stack.last().map(|&(_, k)| k));
            stack.pop();
        }
        let Some(kind) = config.kinds[capture as usize] else {
            continue;
        };
        paint(&mut out, &mut at, start, stack.last().map(|&(_, k)| k));
        stack.push((end, kind));
    }
    while let Some(&(ends, _)) = stack.last() {
        paint(&mut out, &mut at, ends, stack.last().map(|&(_, k)| k));
        stack.pop();
    }
    out
}

/// Colour `at..to` with whatever is innermost there, and move `at` up to it.
fn paint(out: &mut Vec<Span>, at: &mut usize, to: usize, kind: Option<HlKind>) {
    if to <= *at {
        return;
    }
    if let Some(kind) = kind {
        push(out, *at, to, kind);
    }
    *at = to;
}

/// Add `start..end` to a span list, gluing it onto the run before it when they
/// meet and agree.
///
/// Nesting splits a run at every boundary and this is what puts the pieces that
/// ended up the same colour back together — the reason no two spans in a list
/// are ever adjacent and equal. [`requery`] needs the same rule at its seams,
/// where a run that a full query would have produced in one piece arrives as an
/// old piece and a new one.
fn push(out: &mut Vec<Span>, start: usize, end: usize, kind: HlKind) {
    if end <= start {
        return;
    }
    match out.last_mut() {
        Some(last) if last.kind == kind && last.end == start => last.end = end,
        _ => out.push(Span { start, end, kind }),
    }
}

/// Rewrite byte offsets to char offsets in place.
///
/// Doing this per span with `char_indices().position(..)` would be quadratic and
/// is the classic place to accidentally assume `byte == char`; instead we walk
/// the text once, exploiting the fact that span boundaries are non-decreasing.
fn to_char_offsets(text: &str, spans: &mut [Span]) {
    if text.is_ascii() {
        return; // one byte per char, nothing to do
    }
    // (char index, byte offset) pairs, with a sentinel for one-past-the-end.
    let mut chars = text
        .char_indices()
        .map(|(b, _)| b)
        .chain([text.len()])
        .enumerate();
    let mut cur = chars.next().unwrap_or((0, 0));
    let mut char_of = |byte: usize| {
        while cur.1 < byte {
            match chars.next() {
                Some(next) => cur = next,
                None => break,
            }
        }
        cur.0
    };
    for s in spans.iter_mut() {
        s.start = char_of(s.start);
        s.end = char_of(s.end);
    }
}

/// A grammar, its highlight query, and what each of the query's captures
/// paints. Immutable once built, which is what lets one live in a `static`.
struct Config {
    language: Language,
    query: Query,
    /// Indexed by capture id, so the hot path is an array read rather than a
    /// string comparison per capture.
    kinds: Vec<Option<HlKind>>,
}

/// Which [`HlKind`], if any, a query capture name paints.
///
/// The dot-separated prefix rule tree-sitter queries are written against:
/// `function.macro` and `function.builtin` both land on `function`, longest
/// recognised name wins, and ties go to the first in [`CAPTURES`]. A capture
/// name absent from that list — `embedded`, `spell` — gets no highlight, which
/// is how a grammar's query can carry captures we have no face for.
fn kind_for(capture_name: &str) -> Option<HlKind> {
    let parts: Vec<&str> = capture_name.split('.').collect();
    let (mut best, mut best_len) = (None, 0);
    for (name, kind) in CAPTURES {
        let mut len = 0;
        let mut matches = true;
        for part in name.split('.') {
            len += 1;
            if !parts.contains(&part) {
                matches = false;
                break;
            }
        }
        if matches && len > best_len {
            best = Some(*kind);
            best_len = len;
        }
    }
    best
}

/// Every supported language's configuration, built once on first use.
///
/// Building all of them together (rather than one `OnceLock` per language) costs
/// a few milliseconds once and saves a pile of machinery. A grammar whose query
/// fails to compile is simply left out of the map.
fn config(lang: &str) -> Option<&'static Config> {
    static CACHE: OnceLock<HashMap<&'static str, Config>> = OnceLock::new();
    CACHE.get_or_init(build_configs).get(lang)
}

fn build_configs() -> HashMap<&'static str, Config> {
    let mut map = HashMap::new();
    let mut add = |id: &'static str, language: tree_sitter::Language, query: &str| {
        // Only the highlights query is compiled: no embedded-language
        // highlighting, no local-variable resolution. Lexical color only.
        if let Ok(query) = Query::new(&language, query) {
            let kinds = query.capture_names().iter().map(|n| kind_for(n)).collect();
            map.insert(id, Config { language, query, kinds });
        }
    };

    add(
        "rust",
        tree_sitter_rust::LANGUAGE.into(),
        tree_sitter_rust::HIGHLIGHTS_QUERY,
    );
    add(
        "lisp",
        tree_sitter_commonlisp::LANGUAGE_COMMONLISP.into(),
        include_str!("lisp.scm"),
    );
    // Python and JSON carry their own queries for the same reason lisp does,
    // arrived at from the other end: the stock ones exist but are thin — see the
    // headers of those two files for what each was missing.
    add(
        "python",
        tree_sitter_python::LANGUAGE.into(),
        include_str!("python.scm"),
    );
    add(
        "json",
        tree_sitter_json::LANGUAGE.into(),
        include_str!("json.scm"),
    );
    add(
        "toml",
        tree_sitter_toml_ng::LANGUAGE.into(),
        tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
    );
    add(
        "c",
        tree_sitter_c::LANGUAGE.into(),
        tree_sitter_c::HIGHLIGHT_QUERY,
    );
    add(
        "javascript",
        tree_sitter_javascript::LANGUAGE.into(),
        tree_sitter_javascript::HIGHLIGHT_QUERY,
    );
    map
}

// --- incremental parsing ---------------------------------------------------

/// The tree one buffer was last parsed into, and the text it was parsed from.
struct Parsed {
    buffer: BufferId,
    lang: String,
    /// The *old* side of the next edit. Keeping it is not optional: a
    /// [`Change`] is in character offsets, because every offset in this editor
    /// is, and tree-sitter wants bytes and row/columns — neither of which can
    /// be worked out from a character offset without the text it indexes.
    ///
    /// The translation lives here rather than in core for the same reason: it
    /// is one consumer's coordinate system, and a core that recorded bytes
    /// would be recording them for everyone else's benefit too, in the one
    /// place a stray byte offset silently lies about an accented character.
    text: String,
    tree: Tree,
    /// What this text last flattened to, in **bytes** — the one span list in
    /// the crate kept in tree-sitter's units rather than the editor's, because
    /// bytes are what the next call's changed ranges will be in. The char
    /// offsets everyone else indexes by are made from it on the way out, to a
    /// copy, which is one walk of the text and no state to keep in step.
    spans: Vec<Span>,
}

/// How many documents keep a tree.
///
/// It used to be one, on the argument that only the live buffer is ever
/// highlighted. That stopped being true the moment a parked buffer could be
/// reverted under the editor and want colour back: a single slot meant the
/// parked file's parse evicted the tree of the buffer being typed into, so an
/// agent rewriting six files bought six full reparses of the file in front of
/// you — the one path a user actually feels.
///
/// ponytail: a hard cap and a `Vec`, not a `HashMap` with a real LRU. Four is
/// the live buffer plus the couple you are switching between, a linear scan of
/// four is cheaper than hashing, and the seventh open file costs one full parse
/// when it is next looked at. The upgrade, if a big project ever makes that
/// felt, is a *byte* budget rather than a count — what this holds is the text,
/// so four small files and four large ones are not the same amount of memory.
const TREES: usize = 4;

/// A parser that remembers, so a keystroke costs a reparse of what changed
/// rather than of the file.
///
/// Newest parse at the back of `parsed`: every reparse lifts its entry out and
/// pushes it again, so the front is the least recently parsed and dropping it
/// is an LRU with no bookkeeping at all.
pub struct Session {
    parser: Parser,
    cursor: QueryCursor,
    parsed: Vec<Parsed>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Self {
            parser: Parser::new(),
            cursor: QueryCursor::new(),
            parsed: Vec::new(),
        }
    }

    /// Highlight `text` as `lang`, reusing the tree from the previous call when
    /// `edits` says exactly how this text differs from the one it was built
    /// from.
    ///
    /// `buffer` names the document, and `None` means "do not keep this" — the
    /// caller has a string rather than a buffer, so there is nothing a tree
    /// could be reused for and no reason to hold a megabyte of it.
    ///
    /// `edits` is `None` for "assume nothing", which is the honest answer after
    /// a buffer switch, after a language change, or when the reader fell behind
    /// core's change log. It costs a full parse, which is what every parse used
    /// to cost.
    pub fn highlight(
        &mut self,
        buffer: Option<BufferId>,
        lang: &str,
        text: &str,
        edits: Option<&[Change]>,
    ) -> Vec<Span> {
        // This document's own entry, lifted out at the top so that every way
        // out below — org, an unknown language, a grammar that will not load, a
        // parse that fails — leaves behind no tree claiming to describe text it
        // was not built from. It goes back at the bottom or not at all.
        let mine = buffer
            .and_then(|id| self.parsed.iter().position(|p| p.buffer == id))
            .map(|i| self.parsed.remove(i));
        if lang == "org" {
            return org::highlight(text); // hand-rolled scanner, no tree to keep
        }
        let Some(config) = config(lang) else {
            return Vec::new();
        };
        if self.parser.set_language(&config.language).is_err() {
            return Vec::new();
        }
        // A tree may only be reused for the same document in the same
        // language, and only when the caller can say what happened in between.
        // Anything else is a different text, and handing tree-sitter an old
        // tree that does not describe it is the one way to get a *wrong* parse
        // rather than a slow one.
        let mut edit = None;
        let old = mine.filter(|p| p.lang == lang).and_then(|mut old| {
            edits.map(|edits| {
                // One `InputEdit` for the whole run rather than one each:
                // the intermediate texts are gone, and only the two ends
                // are here to convert offsets against. A wider edit than
                // strictly happened costs a wider reparse and nothing else.
                if let Some(change) = Change::coalesce(edits) {
                    edit = Some(input_edit(&old.text, text, change));
                    old.tree.edit(edit.as_ref().expect("just filled"));
                }
                old
            })
        });
        let Some(tree) = self.parser.parse(text, old.as_ref().map(|old| &old.tree)) else {
            return Vec::new();
        };
        // The colouring is incremental too, and on the same terms as the parse:
        // with a tree to compare against, only what moved is re-queried. See
        // [`requery`] for why a span outside the changed set cannot need
        // recolouring, and for the file shape where that saves nothing.
        let bytes = match &old {
            Some(old) => requery(config, &mut self.cursor, old, &tree, text, edit),
            None => spans(config, &mut self.cursor, &tree, text, 0..text.len()),
        };
        // From here down the work is the size of the *file* again — a copy of
        // every span, and a walk of the text to put the copy in char offsets.
        //
        // This carried a `ponytail:` claiming that now dominates a keystroke in
        // a megabyte buffer "at a few milliseconds", and proposing that the
        // caller be handed the changed run instead. Measured, it does not: at
        // 2.8 MB the clone is 0.073 ms and `text.to_string()` 0.046 ms of a
        // 9.35 ms warm keystroke, because `to_char_offsets` early-returns on
        // ASCII. The 9.35 ms is the incremental parse and the widening in
        // `requery`. Left as it is, and the note deleted rather than kept:
        // an upgrade path pointing at 1% of the cost sends the next reader to
        // the wrong end of the function.
        let mut out = bytes.clone();
        to_char_offsets(text, &mut out);
        if let Some(buffer) = buffer {
            if self.parsed.len() == TREES {
                self.parsed.remove(0); // the least recently parsed; see [`TREES`]
            }
            self.parsed.push(Parsed {
                buffer,
                lang: lang.to_string(),
                text: text.to_string(),
                tree,
                spans: bytes,
            });
        }
        out
    }
}

/// Re-run the query only where the tree moved, and splice the result into the
/// spans the previous call left behind.
///
/// The parse was already incremental; this is the other half, and it turns
/// entirely on one question: can a span *outside* the changed ranges need
/// recolouring? It can, and `tree.changed_ranges()` alone does not say where.
///
/// * A query match is a pure function of the subtree it is rooted at —
///   tree-sitter patterns cannot look at a parent, or at anything outside their
///   own root — so a match can only change when that subtree does.
/// * But `changed_ranges` does not report a changed subtree, only the parts of
///   it that are not byte-identical: it skips any child whose symbol, size,
///   parse state and scanner state all still agree. Type a statement above a
///   Python docstring and the *string* is untouched and goes unreported, while
///   `(module . (expression_statement (string) @string.documentation))` quietly
///   stops matching it — the docstring is the second statement now, not the
///   first.
///
/// So the changed set is widened to whole **top-level items**: every child of
/// the root it touches, and one item either side. Every match then lies wholly
/// inside a re-queried item, where the fresh spans replace it outright, or
/// wholly inside an item tree-sitter reused byte for byte, where it cannot have
/// moved. The one item of slack is for patterns rooted at the file root itself,
/// which are the only ones that span two items at once: the seven queries here
/// have exactly one, the module docstring above, and it reaches one child
/// either side of the change.
///
/// Runaway edits need no special case, which is the pleasant surprise: opening
/// a `/*` or a `"` at the top of a file *does* recolour everything below, and
/// it reparses everything below too, so the changed set is already the rest of
/// the file and the query follows it there. What the changed set does not
/// survive is error recovery, and that one is not a surprise at all — see
/// [`dirty`]'s last rule, the single place this stops believing it.
///
/// ponytail: the ceiling is a file whose top level is a single item — a JSON
/// document, a Rust file that is one `mod` — where "the item that changed" is
/// the whole file and this saves nothing but the parse. The upgrade is to
/// descend instead of stopping at the root's children: the same argument holds
/// at any depth, with the deepest pattern in the query as the number of
/// ancestors that have to be swept up rather than "all of them".
fn requery(
    config: &Config,
    cursor: &mut QueryCursor,
    old: &Parsed,
    tree: &Tree,
    text: &str,
    edit: Option<InputEdit>,
) -> Vec<Span> {
    let Some((lo, hi)) = dirty(&old.tree, tree, edit, text.len()) else {
        return old.spans.clone(); // nothing moved, so nothing shifted either
    };
    let fresh = spans(config, cursor, tree, text, lo..hi);
    if lo == 0 && hi >= text.len() {
        return fresh; // the whole file moved; there is nothing to splice it into
    }
    // Where a byte after the edit ended up. Only ever asked about offsets at or
    // past `old_end`, where this is exactly "add the delta"; below it the
    // saturating subtraction keeps the arithmetic from wrapping and the answer
    // is thrown away by the `max(hi)` at the one place it could be read.
    let (old_end, new_end) = edit.map_or((0, 0), |e| (e.old_end_byte, e.new_end_byte));
    let shift = |at: usize| (at + new_end).saturating_sub(old_end);

    let mut out = Vec::with_capacity(old.spans.len() + fresh.len());
    for span in &old.spans {
        if span.start >= lo {
            break;
        }
        push(&mut out, span.start, span.end.min(lo), span.kind);
    }
    for span in &fresh {
        push(&mut out, span.start.max(lo), span.end.min(hi), span.kind);
    }
    for span in &old.spans {
        let end = shift(span.end);
        if end > hi {
            push(&mut out, shift(span.start).max(hi), end, span.kind);
        }
    }
    // A record that never described these two texts can leave an old span
    // hanging past the end of the text now in hand, and these offsets are what
    // the renderer slices with. Same rule as everywhere else here: a caller's
    // bug costs colours, not the editor.
    while out.last().is_some_and(|span| span.start >= text.len()) {
        out.pop();
    }
    if let Some(last) = out.last_mut() {
        last.end = last.end.min(text.len());
    }
    out
}

/// The byte range to re-run the query over: everywhere the tree moved, widened
/// to whole top-level items with one item of slack either side, or `None` when
/// nothing moved at all.
///
/// The edit's own range goes in beside the changed ranges, which costs nothing
/// and closes two holes: an edit that changes no *structure* — a space typed
/// into indentation — reports no changed range whatsoever, and the spans over
/// the edited bytes have to be rebuilt regardless, because the text underneath
/// them is not the text they were made from.
///
/// Nothing here needs unioning across a run of coalesced edits, which is the
/// hazard the worker's queue has: these ranges are read off the two trees, and
/// the old tree has already taken every edit in the run. Whatever a dropped
/// request cost, it did not cost this.
fn dirty(old: &Tree, tree: &Tree, edit: Option<InputEdit>, len: usize) -> Option<(usize, usize)> {
    let root = tree.root_node();
    // A document whose top level is a single item has only one answer here —
    // [`requery`]'s stated ceiling — and `changed_ranges` charges a walk of that
    // item's children to arrive at it: 10 ms on a flat 40 000-key JSON file, to
    // be told what the shape of the file already said.
    if root.child_count() <= 1 {
        return Some((0, len));
    }
    let (mut lo, mut hi) = (usize::MAX, 0);
    for range in old.changed_ranges(tree) {
        lo = lo.min(range.start_byte);
        hi = hi.max(range.end_byte);
    }
    if let Some(edit) = edit {
        lo = lo.min(edit.start_byte);
        hi = hi.max(edit.new_end_byte);
    }
    if lo > hi {
        return None;
    }
    // One walk of the root's children, which is the file's items and not its
    // nodes. `a` ends up on the last item that finishes before the change and
    // `b` on the first that starts after it — the slack — and everything
    // between them is the run of items the change is inside of.
    let mut walk = root.walk();
    let (mut a, mut b) = (0, len);
    for item in root.children(&mut walk) {
        if item.end_byte() <= lo {
            a = item.start_byte();
        }
        if item.start_byte() >= hi {
            b = item.end_byte();
            break;
        }
    }
    let (a, b) = (a.min(lo), b.max(hi));
    // And the one place `changed_ranges` is not to be believed. It walks the
    // two trees in step and skips any pair of subtrees agreeing on symbol,
    // size, parse state and error cost, on the reasoning that a deterministic
    // parse of the same bytes from the same state is the same tree. Under error
    // recovery that reasoning fails: two *different* error subtrees can agree on
    // all four, and the walk skips over a region that really did change. Found
    // by the fuzz below — one deletion in a file of broken Python re-lexed a
    // `from` a thousand bytes away from an identifier into a keyword, and
    // nothing in the changed set said so.
    //
    // So an item that did not parse cleanly is only trusted where it is being
    // re-queried anyway. Typing inside the item you have just broken — which is
    // where the error is while anyone is typing — stays on the fast path; an
    // error left behind somewhere else in the file costs the whole query, which
    // is what every keystroke used to cost.
    let mut walk = root.walk();
    let suspect = root
        .children(&mut walk)
        .any(|item| item.has_error() && (item.start_byte() < a || item.end_byte() > b));
    if suspect {
        return Some((0, len));
    }
    Some((a, b))
}

/// A [`Change`] in the two shapes tree-sitter wants: byte offsets, and
/// zero-based row/column where the column is *also* counted in bytes.
///
/// `start` is looked up in both texts and agrees in both — everything before it
/// is identical by the record's own definition, and does for every record this
/// editor produces. The earlier of the two is taken anyway, because a record
/// that arrived from some *other* text clamps differently on each side, and the
/// result must be a wider reparse rather than an edit whose start is past its
/// own end, which is not an edit at all.
fn input_edit(old: &str, new: &str, change: Change) -> InputEdit {
    let [in_old, old_end] = locate(old, [change.start, change.old_end]);
    let [in_new, new_end] = locate(new, [change.start, change.new_end]);
    let start = in_old.min(in_new);
    InputEdit {
        start_byte: start.0,
        old_end_byte: old_end.0,
        new_end_byte: new_end.0,
        start_position: start.1,
        old_end_position: old_end.1,
        new_end_position: new_end.1,
    }
}

/// Byte offset and row/column of each character offset in `wanted`, which must
/// be ascending, in one walk of `text`.
///
/// One walk and not one per offset: this runs on every keystroke, and the whole
/// point of the exercise is to stop doing work proportional to the file. An
/// offset past the end clamps to the end rather than panicking — the crate's
/// "never fail loudly" rule, and cheap insurance against a record that arrived
/// from a text this one is not.
fn locate<const N: usize>(text: &str, wanted: [usize; N]) -> [(usize, Point); N] {
    let mut out = [(0, Point { row: 0, column: 0 }); N];
    let (mut byte, mut chars, mut row, mut line_start) = (0usize, 0usize, 0usize, 0usize);
    let mut filled = 0;
    let mut it = text.chars();
    loop {
        while filled < N && wanted[filled] <= chars {
            out[filled] = (byte, Point { row, column: byte - line_start });
            filled += 1;
        }
        if filled == N {
            return out;
        }
        let Some(c) = it.next() else {
            let end = (byte, Point { row, column: byte - line_start });
            out[filled..].fill(end);
            return out;
        };
        byte += c.len_utf8();
        chars += 1;
        if c == '\n' {
            row += 1;
            line_start = byte;
        }
    }
}

// --- background highlighting ---------------------------------------------

/// A highlighting thread.
///
/// Parsing is the one job in the editor whose cost grows with the file and
/// which the user must never wait on. So it runs off the UI thread:
/// [`Worker::request`] hands over a snapshot and returns immediately,
/// [`Worker::poll`] picks up whatever has finished.
///
/// The queue is *coalescing per buffer*: a burst of keystrokes produces one
/// parse of the newest text, not one parse per key. Without that, a fast typist
/// outruns the parser and the backlog never drains. Per *buffer* and not
/// globally, because more than one document is now in flight — the live one
/// being typed into, and any parked one a revert has just rewritten — and a
/// single slot meant the second of those was thrown away for the first.
pub struct Worker {
    requests: crossbeam_channel::Sender<Request>,
    results: crossbeam_channel::Receiver<Done>,
}

/// One buffer snapshot to highlight, and how it differs from the last one.
pub struct Request {
    /// What the *buffer's* change count said when `text` was taken. Comes back
    /// with the spans so a caller can drop an answer that buffer has already
    /// moved past.
    ///
    /// The buffer's own counter and not the editor's revision, which is what
    /// this used to be: a revision is global, so a keystroke in the live buffer
    /// moved it and invalidated a parked buffer's perfectly good parse — which
    /// is the same "colourless parked buffer" bug from the other end.
    pub seen: u64,
    /// Which document this is: which tree to reuse, and which buffer the spans
    /// are about when they come back.
    pub buffer: BufferId,
    pub lang: String,
    pub text: String,
    /// Every edit between the *previous* request's text and this one, oldest
    /// first, or `None` for "assume nothing".
    pub edits: Option<Vec<Change>>,
}

/// A finished parse, and enough to tell whether it still describes its buffer.
pub struct Done {
    pub buffer: BufferId,
    /// [`Request::seen`], handed straight back.
    pub seen: u64,
    /// What it was parsed *as*. The other way a result goes stale, and the one
    /// a change count cannot see: `set-language` moves no text at all, so
    /// without this the old grammar's spans would land on the new mode's buffer
    /// for as long as it takes the re-request to come back.
    pub lang: String,
    pub spans: Vec<Span>,
}

/// Put `req` in the pending set, folding it into whatever was already waiting
/// for that buffer, newest at the back.
///
/// Dropping the older request drops its `text`, which is stale and unwanted.
/// It must **not** drop its `edits`: the tree the worker is holding predates
/// both, so the newer request's edits alone would describe a jump the tree
/// never took. A list with a hole in it is worse than no list, so a run that
/// cannot be joined end to end — a different language, or either side already
/// resigned — collapses to `None` and a full parse.
fn queue(pending: &mut Vec<Request>, mut req: Request) {
    if let Some(i) = pending.iter().position(|p| p.buffer == req.buffer) {
        let old = pending.remove(i);
        req.edits = match (old.edits, req.edits) {
            (Some(mut before), Some(after)) if old.lang == req.lang => {
                before.extend(after);
                Some(before)
            }
            _ => None,
        };
    }
    pending.push(req);
}

/// Spawn the highlighting thread. It exits when the [`Worker`] is dropped.
pub fn spawn_worker() -> Worker {
    let (req_tx, req_rx) = crossbeam_channel::unbounded::<Request>();
    let (res_tx, res_rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("zemacs-syntax".into())
        .spawn(move || {
            let mut session = Session::new();
            // At most one pending parse per buffer. ponytail: the ceiling is
            // therefore the number of open buffers — a whole directory
            // reverting at once is that many parses and no more, because a
            // second request for a file folds into the first rather than
            // queueing behind it. No hard cap below that, because capping would
            // need a rule for which buffer loses its colour and there is no
            // honest one; the upgrade if a thousand-buffer project ever appears
            // is to drop parked requests, never the live one.
            let mut pending: Vec<Request> = Vec::new();
            loop {
                if pending.is_empty() {
                    match req_rx.recv() {
                        Ok(req) => queue(&mut pending, req),
                        Err(_) => break, // the worker was dropped
                    }
                }
                while let Ok(req) = req_rx.try_recv() {
                    queue(&mut pending, req);
                }
                // Newest first, and that is the whole latency argument: the
                // newest request is the keystroke somebody is waiting on, and
                // the parked buffers behind it are files that changed on disk
                // while nobody was looking. Oldest-first would put a
                // directory's worth of reverts in front of the character just
                // typed. Continuous typing can starve the parked ones, which is
                // the right way round — they stay colourless while you are
                // busy, and are coloured the moment you pause.
                let req = pending.pop().expect("filled just above");
                let spans =
                    session.highlight(Some(req.buffer), &req.lang, &req.text, req.edits.as_deref());
                let done = Done {
                    buffer: req.buffer,
                    seen: req.seen,
                    lang: req.lang,
                    spans,
                };
                if res_tx.send(done).is_err() {
                    break;
                }
            }
        })
        .expect("failed to spawn syntax thread");
    Worker {
        requests: req_tx,
        results: res_rx,
    }
}

impl Worker {
    /// Queue a snapshot for highlighting. Never blocks.
    pub fn request(&self, req: Request) {
        let _ = self.requests.send(req);
    }

    /// One finished parse, or `None`. Call it until it answers `None`.
    ///
    /// It used to keep only the newest result and drop the rest, which was
    /// right when the live buffer was the only thing ever parsed and wrong the
    /// moment it was not: the live buffer finishing would throw away the parked
    /// buffer's colours on the way past. Two results for the *same* buffer
    /// still arrive in the order they were parsed, so a caller applying each in
    /// turn ends on the newest anyway.
    pub fn poll(&self) -> Option<Done> {
        self.results.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text a span actually covers, sliced by *chars* — the same way the
    /// renderer will. Asserting on this catches byte/char confusion; asserting
    /// on literal offsets would not.
    fn text_of(src: &str, span: &Span) -> String {
        src.chars().skip(span.start).take(span.end - span.start).collect()
    }

    fn first(src: &str, spans: &[Span], kind: HlKind) -> String {
        let span = spans
            .iter()
            .find(|s| s.kind == kind)
            .unwrap_or_else(|| panic!("no {:?} span in {spans:?}", kind));
        text_of(src, span)
    }

    /// The face covering the first occurrence of `needle`, or `None` where the
    /// renderer would leave body text. The language tests below are all "these
    /// two things are not the same colour", which is the question a flat
    /// highlighter fails and a "some spans came back" assertion never asks.
    fn kind_of(src: &str, spans: &[Span], needle: &str) -> Option<HlKind> {
        let byte = src
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is not in the source"));
        let at = src[..byte].chars().count();
        spans.iter().find(|s| s.start <= at && at < s.end).map(|s| s.kind)
    }

    /// Every span of `kind`, in order, as text.
    fn all(src: &str, spans: &[Span], kind: HlKind) -> Vec<String> {
        spans
            .iter()
            .filter(|s| s.kind == kind)
            .map(|s| text_of(src, s))
            .collect()
    }

    /// Every id we advertise must have a working config — a grammar whose query
    /// stops compiling should be caught here, not by silently losing color.
    #[test]
    fn every_advertised_language_builds() {
        assert!(languages().contains(&"rust") && languages().contains(&"lisp"));
        assert_eq!(languages().len(), LANGS.len());
        for lang in languages() {
            // org is the hand-rolled scanner, not a grammar; it has no config.
            assert!(config(lang).is_some() || *lang == "org", "{lang} failed to build");
        }
    }

    #[test]
    fn rust_keywords_and_strings() {
        let src = "fn main() { let x = \"hello\"; }";
        let spans = highlight("rust", src);
        assert_eq!(first(src, &spans, HlKind::Keyword), "fn");
        assert_eq!(first(src, &spans, HlKind::String), "\"hello\"");
        assert_eq!(first(src, &spans, HlKind::Function), "main");
    }

    #[test]
    fn lisp_comments_and_strings() {
        let src = "; greet the user\n(defun greet (name)\n  (format t \"hi ~a\" name))\n";
        let spans = highlight("lisp", src);
        assert_eq!(first(src, &spans, HlKind::Comment), "; greet the user\n");
        assert_eq!(first(src, &spans, HlKind::String), "\"hi ~a\"");
        assert_eq!(first(src, &spans, HlKind::Keyword), "defun");
        assert_eq!(first(src, &spans, HlKind::Function), "greet");
    }

    #[test]
    fn lisp_head_symbols_split_into_keywords_and_calls() {
        let src = "(let ((x 1)) (print x))";
        let spans = highlight("lisp", src);
        assert_eq!(first(src, &spans, HlKind::Keyword), "let");
        assert_eq!(first(src, &spans, HlKind::Function), "print");
        assert_eq!(first(src, &spans, HlKind::Number), "1");
    }

    /// Everything `python-ts-mode` marks that the grammar's own query does not.
    /// Before `python.scm` the decorator was a function call, the docstring was
    /// an ordinary string, `len` was one of your own functions, and the
    /// brackets and the parameters were body text.
    #[test]
    fn python_marks_what_the_grammars_own_query_left_flat() {
        let src = concat!(
            "\"\"\"Module blurb.\"\"\"\n",
            "@app.route\n",
            "def greet(name, count=2):\n",
            "    \"\"\"Say hello.\"\"\"\n",
            "    msg = \"plain\"\n",
            "    return len(msg) + count\n",
        );
        let spans = highlight("python", src);
        let k = |needle: &str| kind_of(src, &spans, needle);
        // the decorator, `@' and dotted path alike
        assert_eq!(k("@app"), Some(HlKind::Type));
        assert_eq!(k("route"), Some(HlKind::Type));
        assert_eq!(k("greet"), Some(HlKind::Function));
        assert_ne!(k("@app"), k("greet"), "a decorator is not a call");
        // a builtin is not one of yours
        assert_eq!(k("len"), Some(HlKind::Constant));
        assert_ne!(k("len"), k("greet"));
        // a docstring is not an ordinary string, in either of the two places
        // this file has one
        assert_eq!(k("Module blurb"), Some(HlKind::Comment));
        assert_eq!(k("Say hello"), Some(HlKind::Comment));
        assert_eq!(k("\"plain\""), Some(HlKind::String));
        // parameters, an assignment target, a number and the brackets
        assert_eq!(k("name"), Some(HlKind::Variable));
        assert_eq!(k("count"), Some(HlKind::Variable));
        assert_eq!(k("msg"), Some(HlKind::Variable));
        assert_eq!(k("2"), Some(HlKind::Number));
        assert_eq!(k("("), Some(HlKind::Punctuation));
    }

    /// The other half of what level 3 of `python-ts-mode` paints: `self`, the
    /// `__dunder__` names, annotations, and a class told apart from a function.
    #[test]
    fn python_reads_self_dunders_and_annotations_the_way_python_ts_mode_does() {
        let src = concat!(
            "class Widget:\n",
            "    \"\"\"A widget.\"\"\"\n",
            "    def __init__(self, size: float) -> None:\n",
            "        self.size = size\n",
            "        print(__name__)\n",
        );
        let spans = highlight("python", src);
        let k = |needle: &str| kind_of(src, &spans, needle);
        assert_eq!(k("Widget"), Some(HlKind::Type));
        assert_eq!(k("A widget"), Some(HlKind::Comment)); // the class docstring
        // a definition first and a dunder second, which is the order the
        // patterns are in: `def __init__` is still a definition
        assert_eq!(k("__init__"), Some(HlKind::Function));
        assert_ne!(k("Widget"), k("__init__"), "a class is not a function");
        assert_eq!(k("self"), Some(HlKind::Keyword));
        assert_eq!(k("size"), Some(HlKind::Variable));
        assert_eq!(k("float"), Some(HlKind::Type));
        assert_eq!(k("None"), Some(HlKind::Constant));
        assert_eq!(k("__name__"), Some(HlKind::Constant));
        assert_eq!(k("print"), Some(HlKind::Constant));
    }

    /// The blanket `(identifier) @variable` is gone on purpose: a name that is
    /// not something in particular gets no span at all and the renderer leaves
    /// it in the body colour. Nothing else checks that decision.
    #[test]
    fn a_plain_python_name_is_left_as_body_text() {
        let src = "import os\nwhere = os.getcwd()\n";
        let spans = highlight("python", src);
        assert_eq!(kind_of(src, &spans, "os\n"), None);
        assert_eq!(kind_of(src, &spans, "where"), Some(HlKind::Variable));
        assert_eq!(kind_of(src, &spans, "getcwd"), Some(HlKind::Function));
    }

    #[test]
    fn an_f_string_is_a_string_and_what_is_in_its_braces_is_not() {
        let src = "note = f\"total: {len(rows)}\"\n";
        let spans = highlight("python", src);
        let k = |needle: &str| kind_of(src, &spans, needle);
        assert_eq!(k("total:"), Some(HlKind::String));
        assert_eq!(k("{"), Some(HlKind::Punctuation));
        assert_eq!(k("len"), Some(HlKind::Constant));
        assert_eq!(k("rows"), Some(HlKind::Variable));
    }

    /// A JSON key is not the same thing as a string value — the stock query had
    /// its two patterns in the order that made it one — and a TOML boolean had
    /// no capture this crate answered to at all.
    #[test]
    fn json_keys_and_toml_booleans_are_not_the_colour_of_their_neighbours() {
        let json = "{\"name\": \"zemacs\", \"n\": 1}";
        let spans = highlight("json", json);
        assert_eq!(kind_of(json, &spans, "\"name\""), Some(HlKind::Type));
        assert_eq!(kind_of(json, &spans, "\"zemacs\""), Some(HlKind::String));
        let toml = "debug = true\nname = \"zemacs\"\n";
        let spans = highlight("toml", toml);
        assert_eq!(kind_of(toml, &spans, "true"), Some(HlKind::Constant));
    }

    /// `self` is a keyword in Rust and `this` is one in JavaScript. Both
    /// grammars call it `@variable.builtin`, which used to fold onto the face a
    /// plain identifier gets — the body colour, in most themes.
    #[test]
    fn self_and_this_are_keywords_rather_than_plain_names() {
        let rust = "impl T { fn f(&self) -> u8 { self.x } }";
        let spans = highlight("rust", rust);
        assert_eq!(kind_of(rust, &spans, "self)"), Some(HlKind::Keyword));
        let js = "class C { m() { return this.x; } }";
        let spans = highlight("javascript", js);
        assert_eq!(kind_of(js, &spans, "this"), Some(HlKind::Keyword));
    }

    /// The whole point of char offsets. Every span here sits after a multi-byte
    /// character, so byte offsets would slice the wrong text.
    #[test]
    fn non_ascii_offsets_are_chars_not_bytes() {
        let src = "let s = \"héllo ▸ wörld\"; // café";
        let spans = highlight("rust", src);
        assert_eq!(first(src, &spans, HlKind::String), "\"héllo ▸ wörld\"");
        assert_eq!(first(src, &spans, HlKind::Comment), "// café");
        // and the byte-offset answers really are different, so this test bites
        assert!(src.len() > src.chars().count());
    }

    /// The contract `runtime/modes/org-fold.lisp` reads: outermost first, so the
    /// innermost range covering a line is the *last* match and the top-level ones
    /// are those no earlier range covers. Both of its loops are one integer of
    /// state that this ordering is the whole justification for.
    #[test]
    fn fold_ranges_are_outermost_first_and_skip_the_root() {
        //          1              2                3     4  5              6
        let src = "fn one() {\n    let a = 1;\n    let b = 2;\n}\n\nfn two() {\n}\n";
        let r = fold_ranges("rust", src);
        assert_eq!(r.first(), Some(&(1, 4)), "the first function, outermost first");
        assert!(r.contains(&(6, 7)), "and the second one: {r:?}");
        // The root spans the file; folding it would hide everything and swallow
        // every other range a caller filtered for the outermost ones.
        assert!(!r.contains(&(1, 7)), "the root is not a fold: {r:?}");
        // Nested ranges are offered too — the caller picks. `one`'s block has the
        // same extent as `one`, which is why `fold-all` drops what it covers.
        assert!(r.iter().filter(|&&x| x == (1, 4)).count() >= 2, "{r:?}");
    }

    /// A one-line `//` comment is not a fold, and the reason it ever looked like
    /// one is [`end_row`]: tree-sitter-rust's `line_comment` eats its own newline
    /// and so ends at column 0 of the next row. Before this, `fold-all` on a file
    /// with a `//!` header folded each header line over the one below it.
    #[test]
    fn a_one_line_comment_is_not_a_two_line_node() {
        let src = "//! one\n//! two\n//! three\nfn f() {\n    // trailing\n}\n";
        let r = fold_ranges("rust", src);
        assert!(r.iter().all(|&(a, b)| a >= 4 && b >= 4), "comments folded: {r:?}");
        assert!(r.contains(&(4, 6)), "the function still folds: {r:?}");
        // A comment that really is several lines still is one.
        assert!(fold_ranges("rust", "/* a\n b */\nfn f() {}\n").contains(&(1, 2)));
    }

    /// Every way out of the reader answers "no folds" rather than failing, which
    /// is what lets Lisp treat the empty list as "nothing structural here".
    #[test]
    fn a_language_with_no_grammar_folds_nothing() {
        assert!(fold_ranges("cobol", "IDENTIFICATION DIVISION.\n").is_empty());
        // org has a hand-rolled highlighter and no tree at all.
        assert!(fold_ranges("org", "* one\nbody\n").is_empty());
        assert!(fold_ranges("rust", "").is_empty());
        // Nonsense in the right language: a parse with errors still answers.
        let _ = fold_ranges("rust", "fn ((( {{{ unterminated\n\n\n");
    }

    #[test]
    fn spans_are_sorted_and_disjoint() {
        let src = include_str!("lib.rs");
        for lang in ["rust", "lisp", "python", "json", "toml", "c", "javascript", "org"] {
            let spans = highlight(lang, src);
            for pair in spans.windows(2) {
                assert!(pair[0].start < pair[0].end, "{lang}: empty span");
                assert!(pair[0].end <= pair[1].start, "{lang}: overlap {pair:?}");
            }
            let n = src.chars().count();
            assert!(spans.iter().all(|s| s.end <= n), "{lang}: span past end");
        }
    }

    #[test]
    fn unknown_language_and_junk_input_are_survivable() {
        assert!(highlight("brainfuck", "+++[->+<]").is_empty());
        assert!(highlight("", "").is_empty());
        for lang in languages() {
            // unterminated everything, stray bytes, lone surrogatish text
            highlight(lang, "\"(({[<#|;'`,@\\\u{0}\u{feff}日本\n");
            highlight(lang, "");
            highlight(lang, "\u{e9}");
        }
    }

    // --- incremental parsing -----------------------------------------------

    /// Type `insert` at char offset `at`, and report what the session says the
    /// colours are afterwards. The one thing a caller must get right is the
    /// [`Change`], so it is computed here the way core computes it.
    fn typed(session: &mut Session, lang: &str, text: &str, at: usize, insert: &str) -> (String, Vec<Span>) {
        edited(session, lang, text, at, 0, insert)
    }

    /// The same with a deletion in front of it: delete `del` characters at
    /// `at`, then type `insert` where they were.
    fn edited(
        session: &mut Session,
        lang: &str,
        text: &str,
        at: usize,
        del: usize,
        insert: &str,
    ) -> (String, Vec<Span>) {
        let mut next: String = text.chars().take(at).collect();
        next.push_str(insert);
        next.extend(text.chars().skip(at + del));
        let change = Change {
            start: at,
            old_end: at + del,
            new_end: at + insert.chars().count(),
        };
        let spans = session.highlight(Some(1), lang, &next, Some(&[change]));
        (next, spans)
    }

    /// The whole contract of the exercise: a tree fed the edit must colour the
    /// text exactly as a tree built from scratch would. Anything less and the
    /// speed is bought with wrong colours, which is worse than slow ones.
    #[test]
    fn an_incremental_parse_agrees_with_a_full_one() {
        let mut session = Session::new();
        let mut src = include_str!("lib.rs").to_string();
        // Seed the session with the unedited file, so every edit below is a
        // reuse rather than a first parse.
        session.highlight(Some(1), "rust", &src, None);
        for (at, insert) in [
            (0, "// leading\n"),
            (300, "\"a string\""),
            (12, "\n"),
            (1000, "fn added() {}\n"),
            // an edit that opens a comment and therefore recolours what
            // follows it — the case a naive "reparse the changed line" gets
            // wrong and an `InputEdit` gets right
            (2000, "/*"),
            (2100, "*/"),
        ] {
            let at = at.min(src.chars().count());
            let (next, incremental) = typed(&mut session, "rust", &src, at, insert);
            assert_eq!(
                incremental,
                highlight("rust", &next),
                "inserting {insert:?} at {at} disagreed with a full parse"
            );
            src = next;
        }
    }

    /// The same contract for the *query*, and the only assertion that proves
    /// it: spans spliced together out of a re-queried range must come back
    /// **byte for byte** what a query over the whole tree would have produced.
    /// Anything weaker buys speed with colours that are wrong only after one
    /// particular edit, which is the bug nobody can reproduce.
    ///
    /// The shapes are the ones that recolour far more than they touch — a
    /// comment or a string opened near the top of a file swallows everything
    /// below it, and a closing brace deleted hands the rest of the parse to an
    /// error node. `at` is *found* rather than counted, so the test keeps
    /// meaning something as the file it reads changes underneath it.
    #[test]
    fn an_incremental_query_agrees_with_a_full_one() {
        let mut session = Session::new();
        let mut src = include_str!("lib.rs").to_string();
        session.highlight(Some(1), "rust", &src, None);
        // (landmark, characters into it, characters to delete, what to type)
        for (landmark, into, del, insert) in [
            ("fn kind_for", 3, 0, "x"),               // an ordinary mid-line insert
            ("fn config", 0, 0, "/*"),                // opens a comment...
            ("fn build_configs", 0, 0, "*/"),         // ...and closes it again
            ("fn to_char_offsets", 0, 0, "\""),       // opens a string...
            ("fn locate", 0, 0, "\""),                // ...and closes it again
            ("}\n\n/// A grammar", 0, 1, ""),         // deletes a closing brace
            ("fn dirty", 0, 0, "\n"),                 // whitespace between two items
            ("//! zemacs-syntax", 0, 0, "\n"),        // the very top of the file
            ("fn end_row", 0, 12, ""),                // deletes a whole name
        ] {
            let found = src.find(landmark).unwrap_or_else(|| panic!("{landmark:?} is gone"));
            let at = src[..found].chars().count() + into;
            let (next, incremental) = edited(&mut session, "rust", &src, at, del, insert);
            assert_eq!(
                incremental,
                highlight("rust", &next),
                "{del} deleted and {insert:?} typed at {landmark:?} disagreed with a full query"
            );
            src = next;
        }
        // The end of the file, where the last closing brace holds up everything
        // above it and there is no following node to widen the change to.
        let end = src.chars().count();
        let (next, incremental) = edited(&mut session, "rust", &src, end - 2, 1, "");
        assert_eq!(incremental, highlight("rust", &next), "the last brace deleted");
        let (appended, incremental) = typed(&mut session, "rust", &next, next.chars().count(), "\nfn t() {}\n");
        assert_eq!(incremental, highlight("rust", &appended), "typed past the end");

        // The other grammars, including the shapes where the whole top level is
        // a single item and there is nothing to narrow to: this file read as
        // JSON is one enormous error node.
        for lang in ["lisp", "python", "json", "toml", "c", "javascript"] {
            let mut session = Session::new();
            let mut src = include_str!("lib.rs").to_string();
            session.highlight(Some(1), lang, &src, None);
            for (at, del, insert) in [(0, 0, "\"("), (900, 1, ""), (400, 0, "*/\n#")] {
                let (next, incremental) = edited(&mut session, lang, &src, at, del, insert);
                assert_eq!(incremental, highlight(lang, &next), "{lang}: {insert:?} at {at}");
                src = next;
            }
        }
    }

    /// What the tree a session is holding would colour if the query had been
    /// run over all of it.
    ///
    /// This is what [`requery`] has to agree with, and it is a *different*
    /// question from "what would a fresh parse colour": tree-sitter's error
    /// recovery is path-dependent, so an edited tree and a from-scratch parse
    /// of the same broken text can genuinely differ, and did — see the fuzz
    /// below, whose junk input reaches that case regularly. Comparing against
    /// the session's own tree asks only what this crate is answerable for.
    fn whole_tree(session: &Session, lang: &str, text: &str) -> Vec<Span> {
        let config = config(lang).expect("a grammar");
        let parsed = session.parsed.last().expect("a parse to compare against");
        let mut cursor = QueryCursor::new();
        let mut out = spans(config, &mut cursor, &parsed.tree, text, 0..text.len());
        to_char_offsets(text, &mut out);
        out
    }

    /// The one generated test in the crate, and it earned its place by finding
    /// a real one: the error-recovery hole that the test above this now pins
    /// down deterministically. No table of edit shapes was going to reach that,
    /// and this is here for the next hole rather than for that one.
    ///
    /// Fixed seed, so a failure is reproducible rather than a thing that
    /// happened on somebody's machine once. The junk it makes is the point:
    /// every language here is fed a file written in another one, and half the
    /// edits leave a quote or a bracket hanging.
    ///
    /// It compares against [`whole_tree`] and not against a fresh parse, which
    /// is the only honest comparison at this level of junk — see there.
    #[test]
    fn a_generated_run_of_edits_never_disagrees_with_a_full_query() {
        let corpus = include_str!("lib.rs");
        let bits = [
            "/*", "*/", "\"", "'", "{", "}", "(", ")", ";", "#", "\n", "x", "//", "\\", "|#",
            "#|", "[", "]", ":", "*", "-", "$", "`", "\u{e9}", "def f():", "\"\"\"", "r#\"",
        ];
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rng = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as usize
        };
        for lang in ["rust", "python", "lisp", "json", "toml", "c", "javascript"] {
            for round in 0..24 {
                let mut session = Session::new();
                let n = 200 + rng() % 2000;
                let start = rng() % (corpus.len() - n - 1);
                let mut src: String = corpus.chars().skip(start).take(n).collect();
                session.highlight(Some(1), lang, &src, None);
                for step in 0..10 {
                    let len = src.chars().count();
                    if len < 4 {
                        break;
                    }
                    let at = rng() % len;
                    let del = if rng() % 3 == 0 { (rng() % 40).min(len - at) } else { 0 };
                    let insert = if rng() % 4 == 0 { "" } else { bits[rng() % bits.len()] };
                    let (next, incremental) = edited(&mut session, lang, &src, at, del, insert);
                    assert_eq!(
                        incremental,
                        whole_tree(&session, lang, &next),
                        "{lang} round {round} step {step}: {del} deleted and {insert:?} typed at {at}"
                    );
                    src = next;
                }
            }
        }
    }

    /// The case the fuzz found, shrunk to the sixty-nine characters that still
    /// show it, and the reason [`dirty`] does not believe the changed ranges
    /// around an error.
    ///
    /// Junk Python. Deleting the first line re-lexes the `from` near the end
    /// from an identifier into a keyword — a change tree-sitter makes and then
    /// does not report, because the two error subtrees over it agree on symbol,
    /// size, parse state and error cost, which is all the changed-range walk
    /// compares before skipping. In the file this was found in, the two were a
    /// thousand bytes apart.
    #[test]
    fn a_relex_inside_an_error_is_recoloured_even_though_nothing_reports_it() {
        let src = ") -> Option<(usize, usize)> {l(h=(M if o>{s for\ny,d///t d from d e e/";
        let mut session = Session::new();
        assert_eq!(kind_of(src, &session.highlight(Some(1), "python", src, None), "from"), None);
        let (next, incremental) = edited(&mut session, "python", src, 0, 29, "");
        // A keyword now, and stale colour is exactly what "still None" means.
        assert_eq!(kind_of(&next, &incremental, "from"), Some(HlKind::Keyword));
        assert_eq!(incremental, highlight("python", &next));
    }

    /// The edit that proves the changed ranges are not enough by themselves.
    /// `(module . (expression_statement (string) @string.documentation))` is
    /// anchored to the *first* statement of the file, so typing a statement
    /// above a docstring recolours the docstring — a node tree-sitter reuses
    /// byte for byte and never reports as changed. The widening in [`requery`]
    /// is the only thing standing between that and a stale colour.
    #[test]
    fn a_docstring_stops_being_one_when_a_statement_is_typed_above_it() {
        let mut session = Session::new();
        let src = "\"\"\"Module blurb.\"\"\"\nimport os\n\n\ndef f():\n    return os\n";
        let before = session.highlight(Some(1), "python", src, None);
        assert_eq!(kind_of(src, &before, "Module blurb"), Some(HlKind::Comment));
        let (next, incremental) = edited(&mut session, "python", src, 0, 0, "x = 1\n");
        // It is prose no longer, and that is the whole point of the test: if
        // this ever comes back `Comment` the edit has stopped biting.
        assert_eq!(kind_of(&next, &incremental, "Module blurb"), Some(HlKind::String));
        assert_eq!(incremental, highlight("python", &next));
    }

    /// Deletions and replacements, not only insertions — `old_end > start` is
    /// the half of the record that a byte/char mix-up destroys, and non-ASCII
    /// text is where it shows.
    #[test]
    fn an_incremental_parse_survives_deletions_and_non_ascii_text() {
        let mut session = Session::new();
        let src = "let s = \"héllo ▸ wörld\"; // café\nfn f() { g(); }\n";
        session.highlight(Some(2), "rust", src, None);
        // Delete the comment, whose `//` is nine characters past a run of
        // multi-byte ones: a record read as bytes would cut the wrong text.
        let comment_at = src.chars().position(|c| c == '/').unwrap();
        let next: String = src.chars().take(comment_at).collect();
        let change = Change {
            start: comment_at,
            old_end: src.chars().count(),
            new_end: comment_at,
        };
        let incremental = session.highlight(Some(2), "rust", &next, Some(&[change]));
        assert_eq!(incremental, highlight("rust", &next));
        assert_eq!(first(&next, &incremental, HlKind::String), "\"héllo ▸ wörld\"");
    }

    /// A tree may only be reused for the document it was built from. Handing it
    /// another buffer's text is the one way to get a *wrong* parse rather than
    /// a slow one, so the session must refuse — even though the caller passed
    /// edits that look perfectly well formed.
    #[test]
    fn a_tree_is_never_reused_for_another_buffer_or_another_language() {
        let mut session = Session::new();
        session.highlight(Some(1), "rust", "fn a() { let x = 1; }", None);
        let other = "fn b() { let y = \"two\"; }";
        let lie = [Change { start: 0, old_end: 0, new_end: 0 }];
        assert_eq!(
            session.highlight(Some(2), "rust", other, Some(&lie)),
            highlight("rust", other)
        );
        let lisp = "(defun f () \"two\")";
        assert_eq!(
            session.highlight(Some(2), "lisp", lisp, Some(&lie)),
            highlight("lisp", lisp)
        );
    }

    /// A run of edits arrives as a run — the worker coalesces requests and
    /// hands over everything since the tree it holds. Feeding tree-sitter the
    /// folded record has to land in the same place as feeding it each one.
    #[test]
    fn a_run_of_edits_lands_where_the_edits_themselves_would() {
        let src = "fn main() {\n    let x = 1;\n}\n";
        let (mut batched, mut one_at_a_time) = (Session::new(), Session::new());
        batched.highlight(Some(3), "rust", src, None);
        one_at_a_time.highlight(Some(3), "rust", src, None);

        // Two edits, applied in order: insert a line, then delete a word from
        // the line above it.
        let after_first = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        let first_change = Change { start: 26, old_end: 26, new_end: 41 };
        let after_second = "fn main() {\n    x = 1;\n    let y = 2;\n}\n";
        let second_change = Change { start: 16, old_end: 20, new_end: 16 };

        one_at_a_time.highlight(Some(3), "rust", after_first, Some(&[first_change]));
        let stepped =
            one_at_a_time.highlight(Some(3), "rust", after_second, Some(&[second_change]));
        let folded = batched.highlight(
            Some(3),
            "rust",
            after_second,
            Some(&[first_change, second_change]),
        );
        assert_eq!(folded, stepped);
        assert_eq!(folded, highlight("rust", after_second));
    }

    /// A record that does not describe the two texts is a caller's bug, and the
    /// crate's rule is that a bug costs colours rather than the editor. The
    /// shapes here are the ones that reach tree-sitter as an *invalid* edit —
    /// a start past the end of one side only — rather than merely a wide one.
    #[test]
    fn a_record_from_the_wrong_text_costs_colours_not_a_crash() {
        let mut session = Session::new();
        let long = "fn a() { let x = 1; let y = 2; let z = 3; }";
        session.highlight(Some(1), "rust", long, None);
        for lie in [
            Change { start: 40, old_end: 41, new_end: 41 },
            Change { start: 0, old_end: 9_999, new_end: 9_999 },
            Change { start: 9_999, old_end: 9_999, new_end: 0 },
        ] {
            let short = "fn a() {}";
            let spans = session.highlight(Some(1), "rust", short, Some(&[lie]));
            assert_eq!(first(short, &spans, HlKind::Keyword), "fn", "{lie:?}");
            session.highlight(Some(1), "rust", long, None);
        }
    }

    /// `locate` is where a character offset becomes the bytes and rows
    /// tree-sitter counts in. Getting it wrong is silent — the parse succeeds
    /// and the colours drift — so it is checked directly.
    #[test]
    fn character_offsets_become_bytes_and_rows() {
        let text = "café\n▸ x\nlast";
        // 0: start; 5: just past the newline after "café" (5 chars, 6 bytes);
        // 9: after "▸ x\n" — row 2, column 0.
        let [zero, after_first_line, third_line] = locate(text, [0, 5, 9]);
        assert_eq!(zero, (0, Point { row: 0, column: 0 }));
        assert_eq!(after_first_line, (6, Point { row: 1, column: 0 }));
        assert_eq!(third_line, (12, Point { row: 2, column: 0 }));
        // past the end clamps rather than panicking
        assert_eq!(locate(text, [999])[0].0, text.len());
    }

    #[test]
    fn language_for_path_maps_extensions() {
        let lang = |p: &str| language_for_path(Path::new(p));
        assert_eq!(lang("src/main.rs").as_deref(), Some("rust"));
        assert_eq!(lang("/tmp/init.lisp").as_deref(), Some("lisp"));
        assert_eq!(lang("zemacs.asd").as_deref(), Some("lisp"));
        assert_eq!(lang("init.el").as_deref(), Some("lisp"));
        assert_eq!(lang("Cargo.toml").as_deref(), Some("toml"));
        assert_eq!(lang("TODO.org").as_deref(), Some("org"));
        assert_eq!(lang("SHOUT.RS").as_deref(), Some("rust"));
        assert_eq!(lang("notes.txt"), None);
        assert_eq!(lang("Makefile"), None);
        assert_eq!(lang(""), None);
    }

    // --- org ---------------------------------------------------------------

    #[test]
    fn org_headings_are_leveled_and_whole_line() {
        let src = "* One\n** Two\n*** Three\n**** Four\nbody text\n";
        let spans = highlight("org", src);
        assert_eq!(all(src, &spans, HlKind::Heading1), ["* One"]);
        assert_eq!(all(src, &spans, HlKind::Heading2), ["** Two"]);
        // level 3 is the floor: deeper headings share it rather than fading out
        assert_eq!(all(src, &spans, HlKind::Heading3), ["*** Three", "**** Four"]);
        assert_eq!(spans.len(), 4, "body text should get no span: {spans:?}");
    }

    #[test]
    fn org_emphasis_paints_body_and_delimiters_separately() {
        let src = "*bold* /italic/ =verbatim= ~code~ _under_ +strike+\n";
        let spans = highlight("org", src);
        assert_eq!(all(src, &spans, HlKind::Bold), ["bold"]);
        // `_underline_` has no face of its own and borrows Italic
        assert_eq!(all(src, &spans, HlKind::Italic), ["italic", "under"]);
        assert_eq!(all(src, &spans, HlKind::Code), ["verbatim", "code"]);
        // ...and `+strike+` borrows Comment, the dimmest face there is
        assert_eq!(all(src, &spans, HlKind::Comment), ["strike"]);
        // the markers themselves, and nothing but the markers
        assert_eq!(all(src, &spans, HlKind::Markup).concat(), "**//==~~__++");
    }

    /// The trap that makes `*` hard: it is a heading at column 0, emphasis
    /// mid-line, and multiplication the rest of the time.
    #[test]
    fn org_arithmetic_is_not_bold() {
        let src = "2 * 3 * 4 = 24, and a *b* is not\n";
        let spans = highlight("org", src);
        assert_eq!(all(src, &spans, HlKind::Bold), ["b"]);
    }

    /// The matching trap for `/`: the slashes in a URL are preceded by `:` and
    /// by other slashes, neither of which may open emphasis.
    #[test]
    fn org_urls_are_not_italic() {
        let src = "see http://example.com/a/b/ then /really/ italic\n";
        let spans = highlight("org", src);
        assert_eq!(all(src, &spans, HlKind::Italic), ["really"]);
    }

    /// `#+` is a directive, `# ` is a comment. Confusing the two paints every
    /// keyword line grey.
    #[test]
    fn org_directives_and_comments_are_not_confused() {
        let src = "#+TITLE: x\n# a comment\n\
                   #+BEGIN_SRC rust\nlet y = *not markup*;\n#+end_src\n";
        let spans = highlight("org", src);
        assert_eq!(
            all(src, &spans, HlKind::Keyword),
            ["#+TITLE: x", "#+BEGIN_SRC rust", "#+end_src"]
        );
        assert_eq!(all(src, &spans, HlKind::Comment), ["# a comment"]);
        // block contents are left plain rather than read as prose
        assert!(!spans.iter().any(|s| s.kind == HlKind::Bold), "{spans:?}");
    }

    #[test]
    fn org_links_with_and_without_a_description() {
        let src = "[[https://x][label]] and [[bare]]\n";
        let spans = highlight("org", src);
        assert_eq!(all(src, &spans, HlKind::Link), ["https://x", "label", "bare"]);
        assert_eq!(
            all(src, &spans, HlKind::Markup),
            ["[[", "][", "]]", "[[", "]]"]
        );
    }

    #[test]
    fn org_todo_keywords_split_the_heading() {
        let src = "* TODO write tests\n** DONE ship it\n*** TODOS are just a word\n";
        let spans = highlight("org", src);
        assert_eq!(all(src, &spans, HlKind::Constant), ["TODO"]);
        assert_eq!(all(src, &spans, HlKind::Comment), ["DONE"]);
        assert_eq!(all(src, &spans, HlKind::Heading1), ["* ", " write tests"]);
        assert_eq!(all(src, &spans, HlKind::Heading3), ["*** TODOS are just a word"]);
    }

    /// A table rule is full of `+` and `-`; read as prose it becomes a
    /// strike-through run. Whole-line, like Emacs' own `org-table` face.
    #[test]
    fn org_table_rows_are_not_prose() {
        let src = "| a | b |\n|---+---|\n| c | d |\n";
        let spans = highlight("org", src);
        assert_eq!(
            all(src, &spans, HlKind::Punctuation),
            ["| a | b |", "|---+---|", "| c | d |"]
        );
        assert_eq!(spans.len(), 3, "{spans:?}");
    }

    #[test]
    fn org_list_bullets() {
        let src = "- one\n+ two\n1. three\n2) four\n  - nested *b*\nnot- a bullet\n";
        let spans = highlight("org", src);
        assert_eq!(
            all(src, &spans, HlKind::Punctuation),
            ["-", "+", "1.", "2)", "-"]
        );
        assert_eq!(all(src, &spans, HlKind::Bold), ["b"]);
    }

    /// A byte-indexed scanner slices these one or two characters short.
    #[test]
    fn org_non_ascii_offsets_are_chars_not_bytes() {
        let src = "* Überschrift\nein *café* wert\n";
        let spans = highlight("org", src);
        assert_eq!(first(src, &spans, HlKind::Heading1), "* Überschrift");
        assert_eq!(first(src, &spans, HlKind::Bold), "café");
        assert!(src.len() > src.chars().count());
    }

    #[test]
    fn org_checkboxes_borrow_the_todo_and_done_faces() {
        let src = "- [ ] open\n- [X] shut\n- [-] partly\n- [q] not a box\n";
        let spans = highlight("org", src);
        assert_eq!(all(src, &spans, HlKind::Constant), ["[ ]", "[-]"]);
        assert_eq!(all(src, &spans, HlKind::Comment), ["[X]"]);
    }

    // --- org structure ------------------------------------------------------

    /// The text each bullet marker actually covers, so a wrong offset shows up
    /// as the wrong character rather than as a number nobody can check.
    fn marker(src: &str, b: &Bullet) -> String {
        src.chars().skip(b.start).take(b.end - b.start).collect()
    }

    #[test]
    fn heading_bullets_carry_their_star_run_and_level() {
        let src = "* One\n** Two\n***** Deep\nnot a heading\n*bold* line\n";
        let b = bullets(src);
        let seen: Vec<_> = b.iter().map(|b| (b.kind, b.level, marker(src, b))).collect();
        assert_eq!(
            seen,
            [
                (BulletKind::Heading, 1, "*".into()),
                (BulletKind::Heading, 2, "**".into()),
                (BulletKind::Heading, 5, "*****".to_string()),
            ]
        );
    }

    /// A list bullet's level is its indent column: that is what nesting *is* in
    /// org, and what an org-modern renderer cycles glyphs on.
    #[test]
    fn list_bullets_report_where_they_are_and_how_deep() {
        let src = "- top\n  - nested\n+ also\n1. numbered\nnot- one\n";
        let b = bullets(src);
        let seen: Vec<_> = b.iter().map(|b| (b.kind, b.level, marker(src, b))).collect();
        assert_eq!(
            seen,
            [
                (BulletKind::List, 0, "-".into()),
                (BulletKind::List, 2, "-".into()),
                (BulletKind::List, 0, "+".to_string()),
            ]
        );
    }

    /// A `-` in a diff block and a `*` in a C comment are not org structure.
    #[test]
    fn bullets_inside_a_source_block_are_not_bullets() {
        let src = "- real\n#+begin_src diff\n- removed\n* not a heading\n#+end_src\n- real too\n";
        assert_eq!(bullets(src).len(), 2, "{:?}", bullets(src));
    }

    #[test]
    fn bullet_offsets_are_chars_not_bytes() {
        let src = "* Überschrift\n- café\n";
        let b = bullets(src);
        assert_eq!(b.iter().map(|b| marker(src, b)).collect::<Vec<_>>(), ["*", "-"]);
        assert!(src.len() > src.chars().count());
    }

    // --- org LaTeX fragments ------------------------------------------------

    fn fragments(src: &str) -> Vec<String> {
        latex_fragments(src).into_iter().map(|f| f.source).collect()
    }

    #[test]
    fn every_delimiter_org_uses_is_a_fragment_and_display_is_told_apart() {
        let src = "inline $E = mc^2$ and \\(a+b\\), display $$x$$ and \\[y\\]\n";
        let f = latex_fragments(src);
        assert_eq!(
            f.iter().map(|f| f.source.clone()).collect::<Vec<_>>(),
            ["$E = mc^2$", "\\(a+b\\)", "$$x$$", "\\[y\\]"]
        );
        assert_eq!(
            f.iter().map(|f| f.display).collect::<Vec<_>>(),
            [false, false, true, true]
        );
    }

    /// The rule everyone gets wrong. Org wants no whitespace just inside either
    /// `$`, which is exactly what stops prices, and only punctuation after the
    /// closer, which stops `$5-$10`.
    #[test]
    fn a_dollar_amount_in_prose_is_not_a_fragment() {
        for src in [
            "it cost $5 and $10 more\n",
            "$ x$ and $x $\n",
            "a $,x$ b\n",
            "price: $100.\n",
            "from $5-$10 each\n",
        ] {
            assert_eq!(fragments(src), Vec::<String>::new(), "{src:?}");
        }
    }

    #[test]
    fn any_environment_counts_not_just_equation() {
        let src = "\\begin{align}\na &= b\n\\end{align}\n\
                   mid\n\\begin{equation*}\nx\n\\end{equation*}\n";
        let f = latex_fragments(src);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f.iter().all(|f| f.display));
        assert!(f[0].source.ends_with("\\end{align}"), "{:?}", f[0].source);
        assert!(f[1].source.starts_with("\\begin{equation*}"), "{:?}", f[1].source);
    }

    #[test]
    fn an_environment_must_open_its_own_line() {
        assert_eq!(
            fragments("we write \\begin{align}a\\end{align} inline\n"),
            Vec::<String>::new()
        );
        assert_eq!(fragments("  \\begin{align}a\\end{align}\n").len(), 1);
    }

    #[test]
    fn a_dollar_inside_a_source_block_or_a_comment_is_not_math() {
        let src = "#+begin_src sh\necho $HOME and $x$\n#+end_src\n\
                   # $y$ in a comment\n#+TITLE: $z$\nreal $w$ here\n";
        assert_eq!(fragments(src), ["$w$"]);
    }

    #[test]
    fn an_unterminated_fragment_is_not_a_fragment() {
        for src in [
            "half $x of math\n",
            "$$never closed\n",
            "\\(open only\n",
            "\\begin{align}\nx\n",
            "\\[dangling\n",
        ] {
            assert_eq!(fragments(src), Vec::<String>::new(), "{src:?}");
        }
    }

    #[test]
    fn latex_fragment_offsets_are_chars_not_bytes() {
        let src = "café $x^2$ Überschrift\n";
        let f = latex_fragments(src);
        let sliced: String = src.chars().skip(f[0].start).take(f[0].end - f[0].start).collect();
        assert_eq!(sliced, "$x^2$");
        assert_eq!(sliced, f[0].source);
        assert!(src.len() > src.chars().count());
    }

    /// What `C-c r` asks: "is there a fragment where the cursor is?" — including
    /// just past the closing delimiter, which is where you land after typing it.
    #[test]
    fn latex_fragment_at_finds_the_one_under_the_cursor() {
        let src = "a $x$ b $$y$$ c\n"; // `$x$` is 2..5, `$$y$$` is 8..13
        let at = |pos| latex_fragment_at(src, pos).map(|f| f.source);
        assert_eq!(at(2).as_deref(), Some("$x$"));
        assert_eq!(at(3).as_deref(), Some("$x$"));
        assert_eq!(at(5).as_deref(), Some("$x$"));
        assert_eq!(at(9).as_deref(), Some("$$y$$"));
        assert_eq!(at(0), None);
        assert_eq!(at(14), None);
    }

    /// Same contract the spans have: in order, non-overlapping, in bounds, and
    /// each `source` really is the text at its own offsets.
    #[test]
    fn latex_fragments_are_sorted_disjoint_and_in_bounds() {
        let src = "#+TITLE: notes with $math$\n\
                   * A heading with $x_1$ in it\n\
                   - [ ] an item with \\(y\\) and a price of $20 too\n\
                   \\begin{align}\na &= b\n\\end{align}\n\
                   #+begin_src rust\nlet cost = \"$5\"; // $not$ math\n#+end_src\n\
                   trailing $$\\int_0^1 f$$ and café $\\alpha$\n";
        let f = latex_fragments(src);
        assert_eq!(
            fragments(src),
            [
                "$x_1$",
                "\\(y\\)",
                "\\begin{align}\na &= b\n\\end{align}",
                "$$\\int_0^1 f$$",
                "$\\alpha$",
            ]
        );
        for pair in f.windows(2) {
            assert!(pair[0].start < pair[0].end, "empty {pair:?}");
            assert!(pair[0].end <= pair[1].start, "overlap {pair:?}");
        }
        let n = src.chars().count();
        for frag in &f {
            assert!(frag.end <= n, "past end: {frag:?}");
            let sliced: String = src.chars().skip(frag.start).take(frag.end - frag.start).collect();
            assert_eq!(sliced, frag.source);
        }
    }

    /// Junk must not hang or panic either — these run the fragment scanner over
    /// unbalanced delimiters of every kind.
    #[test]
    fn org_structure_survives_junk() {
        for src in ["", "$", "$$", "$$$", "$$$$", "\\", "\\(", "\\[", "\\begin{", "\\begin{}",
                    "#+begin_src\n$", "\\begin{a}\n$$\n", "$\u{feff}$", "* \n- \n$$\n"] {
            latex_fragments(src);
            bullets(src);
            highlight("org", src);
        }
    }

    /// Everything at once: the contract is sorted, disjoint, in-bounds spans,
    /// and a heading that keeps its own line even when it contains markup.
    #[test]
    fn org_kitchen_sink_is_sorted_and_disjoint() {
        let src = "#+TITLE: Everything\n\
                   # a comment\n\
                   * TODO Head *with* markup\n\
                   ** Sub /italic/ and =verb=\n\
                   *** Deep\n\
                   - a list with ~code~ and [[https://x][a link]]\n\
                   1. numbered 2 * 3 * 4\n\
                   \n\
                   #+begin_src rust\n\
                   fn f() { /* *nope* */ }\n\
                   #+end_src\n\
                   \n\
                   plain http://example.com/a/b/ tail *bold* café Überschrift\n";
        let spans = highlight("org", src);
        for pair in spans.windows(2) {
            assert!(pair[0].start < pair[0].end, "empty span {pair:?}");
            assert!(pair[0].end <= pair[1].start, "overlap {pair:?}");
        }
        let n = src.chars().count();
        assert!(spans.iter().all(|s| s.end <= n), "span past end: {spans:?}");
        // the heading wins over the `*with*`, `/italic/` and `=verb=` inside it
        assert_eq!(all(src, &spans, HlKind::Bold), ["bold"]);
        assert_eq!(all(src, &spans, HlKind::Italic), Vec::<String>::new());
        assert_eq!(all(src, &spans, HlKind::Code), ["code"]);
        assert_eq!(all(src, &spans, HlKind::Link), ["https://x", "a link"]);
    }
}
