//! zemacs-syntax — tree-sitter highlighting, flattened into [`zemacs_core::Span`].
//!
//! The whole crate is three functions ([`language_for_path`], [`highlight`],
//! [`languages`]) over one idea: run a grammar's `highlights.scm` over a parse
//! tree, keep the innermost capture covering each byte, and convert byte
//! offsets to char offsets, which is what the rope and the renderer index by.
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
//! instead. With no injection and no locals query — which is what this crate
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
/// `HighlightConfiguration::configure` does the prefix work for us: it matches
/// a query's capture name against this list by dot-separated parts, longest
/// match wins. So `function.macro` and `function.builtin` both land on
/// `function`, `punctuation.bracket` on `punctuation`, and so on. Capture names
/// absent from this list (`embedded`, `spell`, ...) simply get no highlight.
const CAPTURES: &[(&str, HlKind)] = &[
    ("attribute", HlKind::Constant),
    ("comment", HlKind::Comment),
    ("constant", HlKind::Constant),
    ("constructor", HlKind::Type),
    ("delimiter", HlKind::Punctuation),
    ("escape", HlKind::String),
    ("function", HlKind::Function),
    ("keyword", HlKind::Keyword),
    ("label", HlKind::Constant),
    ("number", HlKind::Number),
    ("operator", HlKind::Operator),
    ("property", HlKind::Variable),
    ("punctuation", HlKind::Punctuation),
    ("string", HlKind::String),
    ("type", HlKind::Type),
    ("variable", HlKind::Variable),
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
fn spans(config: &Config, cursor: &mut QueryCursor, tree: &Tree, text: &str) -> Vec<Span> {
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
        match out.last_mut() {
            // Nesting splits a run at every boundary; glue the pieces that
            // ended up the same colour back together.
            Some(last) if last.kind == kind && last.end == *at => last.end = to,
            _ => out.push(Span { start: *at, end: to, kind }),
        }
    }
    *at = to;
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
    add(
        "python",
        tree_sitter_python::LANGUAGE.into(),
        tree_sitter_python::HIGHLIGHTS_QUERY,
    );
    add(
        "json",
        tree_sitter_json::LANGUAGE.into(),
        tree_sitter_json::HIGHLIGHTS_QUERY,
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
}

/// A parser that remembers, so a keystroke costs a reparse of what changed
/// rather than of the file.
///
/// One document at a time, and that is deliberate: only the *live* buffer is
/// ever highlighted — a parked one keeps the spans it had — so a second tree
/// would only ever earn its memory across a buffer switch, and a switch already
/// throws nothing away. The first parse after a switch is the full parse that
/// used to happen on every keystroke.
///
/// ponytail: switching back and forth between two large files therefore
/// reparses each time. The ceiling is a pair of files big enough for that to be
/// felt, which is the same few hundred KB every other ceiling in this crate
/// sits at; the upgrade is a small LRU of [`Parsed`] keyed by [`BufferId`],
/// which is a `HashMap` and an eviction rule and no new idea.
pub struct Session {
    parser: Parser,
    cursor: QueryCursor,
    parsed: Option<Parsed>,
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
            parsed: None,
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
        if lang == "org" {
            self.parsed = None; // hand-rolled scanner, no tree to keep
            return org::highlight(text);
        }
        let Some(config) = config(lang) else {
            self.parsed = None;
            return Vec::new();
        };
        if self.parser.set_language(&config.language).is_err() {
            self.parsed = None;
            return Vec::new();
        }
        // A tree may only be reused for the same document in the same
        // language, and only when the caller can say what happened in between.
        // Anything else is a different text, and handing tree-sitter an old
        // tree that does not describe it is the one way to get a *wrong* parse
        // rather than a slow one.
        let old = match self.parsed.take() {
            Some(mut old) if Some(old.buffer) == buffer && old.lang == lang => {
                edits.map(|edits| {
                    // One `InputEdit` for the whole run rather than one each:
                    // the intermediate texts are gone, and only the two ends
                    // are here to convert offsets against. A wider edit than
                    // strictly happened costs a wider reparse and nothing else.
                    if let Some(change) = Change::coalesce(edits) {
                        old.tree.edit(&input_edit(&old.text, text, change));
                    }
                    old.tree
                })
            }
            _ => None,
        };
        let Some(tree) = self.parser.parse(text, old.as_ref()) else {
            return Vec::new();
        };
        // ponytail: the *query* still runs over the whole tree, so this saves
        // the parse and not the colouring. The parse is the part that grows
        // superlinearly and the part that was measured; re-running the query
        // only over `tree.changed_ranges(&old)` means keeping the previous
        // spans and splicing the new ones into them, which is a second
        // stateful thing to get wrong for a smaller win.
        let mut out = spans(config, &mut self.cursor, &tree, text);
        to_char_offsets(text, &mut out);
        if let Some(buffer) = buffer {
            self.parsed = Some(Parsed {
                buffer,
                lang: lang.to_string(),
                text: text.to_string(),
                tree,
            });
        }
        out
    }
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
/// The queue is *coalescing*: a burst of keystrokes produces one parse of the
/// newest text, not one parse per key. Without that, a fast typist outruns the
/// parser and the backlog never drains.
pub struct Worker {
    requests: crossbeam_channel::Sender<Request>,
    results: crossbeam_channel::Receiver<(u64, Vec<Span>)>,
}

/// One buffer snapshot to highlight, and how it differs from the last one.
pub struct Request {
    /// What the editor's revision counter said when `text` was taken. Comes
    /// back with the spans so a caller can drop an answer the buffer has
    /// already moved past.
    pub revision: u64,
    /// Which document this is. The worker keeps one tree, and this is how it
    /// knows the tree is not about some other file.
    pub buffer: BufferId,
    pub lang: String,
    pub text: String,
    /// Every edit between the *previous* request's text and this one, oldest
    /// first, or `None` for "assume nothing".
    pub edits: Option<Vec<Change>>,
}

/// Fold a request that is about to be dropped for a newer one into it.
///
/// Dropping the older request drops its `text`, which is stale and unwanted.
/// It must **not** drop its `edits`: the tree the worker is holding predates
/// both, so the newer request's edits alone would describe a jump the tree
/// never took. A list with a hole in it is worse than no list, so a run that
/// cannot be joined end to end — a different buffer, a different language, or
/// either side already resigned — collapses to `None` and a full parse.
fn coalesce_requests(old: Request, mut new: Request) -> Request {
    new.edits = match (old.edits, new.edits) {
        (Some(mut before), Some(after))
            if old.buffer == new.buffer && old.lang == new.lang =>
        {
            before.extend(after);
            Some(before)
        }
        _ => None,
    };
    new
}

/// Spawn the highlighting thread. It exits when the [`Worker`] is dropped.
pub fn spawn_worker() -> Worker {
    let (req_tx, req_rx) = crossbeam_channel::unbounded::<Request>();
    let (res_tx, res_rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("zemacs-syntax".into())
        .spawn(move || {
            let mut session = Session::new();
            while let Ok(mut req) = req_rx.recv() {
                // Everything queued behind the newest request is already stale.
                while let Ok(newer) = req_rx.try_recv() {
                    req = coalesce_requests(req, newer);
                }
                let spans = session.highlight(
                    Some(req.buffer),
                    &req.lang,
                    &req.text,
                    req.edits.as_deref(),
                );
                if res_tx.send((req.revision, spans)).is_err() {
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

    /// The most recent finished result, or `None`. Older results waiting behind
    /// it are dropped — the caller only ever wants the newest.
    pub fn poll(&self) -> Option<(u64, Vec<Span>)> {
        let mut latest = self.results.try_recv().ok()?;
        while let Ok(newer) = self.results.try_recv() {
            latest = newer;
        }
        Some(latest)
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
        let mut next: String = text.chars().take(at).collect();
        next.push_str(insert);
        next.extend(text.chars().skip(at));
        let change = Change {
            start: at,
            old_end: at,
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
