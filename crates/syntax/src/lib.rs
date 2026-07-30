//! zemacs-syntax — tree-sitter highlighting, flattened into [`zemacs_core::Span`].
//!
//! The whole crate is three functions ([`language_for_path`], [`highlight`],
//! [`languages`]) over one idea: `tree-sitter-highlight` already knows how to
//! run a grammar's `highlights.scm` and stream a stack of nested highlight
//! events, so we only have to (a) tell it which capture names we recognize,
//! (b) flatten its event stream to the innermost highlight per byte run, and
//! (c) convert byte offsets to char offsets, which is what the rope and the
//! renderer index by.
//!
//! The exception is org, which has no grammar we can use (see [`org`]) and is
//! scanned by hand, a line at a time, straight into char offsets.
//!
//! Design rules:
//!
//! * **Never fail loudly.** An unknown language, a query that won't compile, a
//!   grammar that panics on garbage input — all of it degrades to "no spans",
//!   i.e. plain text. Bad highlighting must never take the editor down.
//! * **Build each `HighlightConfiguration` once.** Compiling a query is
//!   milliseconds; `highlight` runs on every buffer revision.
//! * A grammar that will not compile against our `tree-sitter` version is
//!   dropped rather than pinning the workspace backwards.
//!
//! ponytail: full reparse of the whole buffer per revision. The ceiling is
//! roughly "files you can still scroll comfortably" — a few hundred KB. Past
//! that, keep the `Tree` per buffer and feed it to `Parser::parse` as the old
//! tree, and only re-run the query over the changed ranges.

mod org;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};
use zemacs_core::{HlKind, Span};

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
pub fn highlight(lang: &str, text: &str) -> Vec<Span> {
    if lang == "org" {
        return org::highlight(text); // hand-rolled, already in char offsets
    }
    let Some(config) = config(lang) else {
        return Vec::new();
    };
    // A `Highlighter` owns a `Parser`; one per thread, reused across calls.
    thread_local! {
        static HIGHLIGHTER: RefCell<Highlighter> = RefCell::new(Highlighter::new());
    }
    let mut spans = HIGHLIGHTER.with(|h| byte_spans(&mut h.borrow_mut(), config, text));
    to_char_offsets(text, &mut spans);
    spans
}

/// Flatten the highlight event stream into sorted, non-overlapping byte spans.
fn byte_spans(hl: &mut Highlighter, config: &HighlightConfiguration, text: &str) -> Vec<Span> {
    let Ok(events) = hl.highlight(config, text.as_bytes(), None, |_| None) else {
        return Vec::new();
    };
    let mut out: Vec<Span> = Vec::new();
    // `Source` events are already in order and disjoint; the top of the stack is
    // the innermost (most recent) highlight covering them.
    let mut stack: Vec<HlKind> = Vec::new();
    for event in events {
        match event {
            Ok(HighlightEvent::HighlightStart(h)) => stack.push(CAPTURES[h.0].1),
            Ok(HighlightEvent::HighlightEnd) => {
                stack.pop();
            }
            Ok(HighlightEvent::Source { start, end }) => {
                let (Some(&kind), true) = (stack.last(), start < end) else {
                    continue;
                };
                match out.last_mut() {
                    // The stream splits runs at every nesting change; glue the
                    // pieces that ended up the same color back together.
                    Some(last) if last.kind == kind && last.end == start => last.end = end,
                    _ => out.push(Span { start, end, kind }),
                }
            }
            Err(_) => return Vec::new(),
        }
    }
    out
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

/// Every supported language's configuration, built once on first use.
///
/// Building all of them together (rather than one `OnceLock` per language) costs
/// a few milliseconds once and saves a pile of machinery. A grammar whose query
/// fails to compile is simply left out of the map.
fn config(lang: &str) -> Option<&'static HighlightConfiguration> {
    static CACHE: OnceLock<HashMap<&'static str, HighlightConfiguration>> = OnceLock::new();
    CACHE.get_or_init(build_configs).get(lang)
}

fn build_configs() -> HashMap<&'static str, HighlightConfiguration> {
    let names: Vec<&str> = CAPTURES.iter().map(|(n, _)| *n).collect();
    let mut map = HashMap::new();
    let mut add = |id: &'static str, language: tree_sitter::Language, query: &str| {
        // Injections and locals are deliberately empty: no embedded-language
        // highlighting, no local-variable resolution. Lexical color only.
        if let Ok(mut config) = HighlightConfiguration::new(language, id, query, "", "") {
            config.configure(&names);
            map.insert(id, config);
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

// --- background highlighting ---------------------------------------------

/// A highlighting thread.
///
/// Highlighting is a full reparse, which is the one job in the editor whose
/// cost grows with the file and which the user must never wait on. So it runs
/// off the UI thread: [`Worker::request`] hands over a snapshot and returns
/// immediately, [`Worker::poll`] picks up whatever has finished.
///
/// The queue is *coalescing*: a burst of keystrokes produces one parse of the
/// newest text, not one parse per key. Without that, a fast typist outruns the
/// parser and the backlog never drains.
pub struct Worker {
    requests: crossbeam_channel::Sender<Request>,
    results: crossbeam_channel::Receiver<(u64, Vec<Span>)>,
}

struct Request {
    revision: u64,
    lang: String,
    text: String,
}

/// Spawn the highlighting thread. It exits when the [`Worker`] is dropped.
pub fn spawn_worker() -> Worker {
    let (req_tx, req_rx) = crossbeam_channel::unbounded::<Request>();
    let (res_tx, res_rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("zemacs-syntax".into())
        .spawn(move || {
            while let Ok(mut req) = req_rx.recv() {
                // Everything queued behind the newest request is already stale.
                while let Ok(newer) = req_rx.try_recv() {
                    req = newer;
                }
                if res_tx.send((req.revision, highlight(&req.lang, &req.text))).is_err() {
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
    pub fn request(&self, revision: u64, lang: &str, text: String) {
        let _ = self.requests.send(Request {
            revision,
            lang: lang.to_string(),
            text,
        });
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
