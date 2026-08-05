//! Headless proof of `runtime/modes/org-frozen.lisp` — org rendered as a
//! printed page rather than edited as a manuscript.
//!
//! The mode used to draw that page with overlays on the cell grid, and this
//! file used to read overlays back. It draws a **scene** now — a tree of boxes
//! and paragraphs laid out in pixels by `crates/gui` — so what is read back is
//! `Buffer::scene`, which is the whole of what the editor holds about the page.
//!
//! Reading the tree rather than a screenshot is not a compromise. Everything
//! this mode decides is in the tree: whether a heading's *run* is larger (which
//! is the thing an `Overlay::scale` could never be, since scale is a property of
//! a line), whether a listing is one `Text` per line with Python's own spans
//! inside it, whether a table cell kept the emphasis the grid version flattened
//! into six literal characters. What is *not* in the tree — where the words
//! actually land — belongs to `crates/gui`'s own tests, which measure against a
//! fake font with no window open.
//!
//! Deliberately a single `#[test]`, as in every file beside it: `cl_boot`
//! initialises a process-wide Lisp image, so there is exactly one `spawn` per
//! test binary and a new file is the only way to add one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, HlKind, Mode, ReadOnly, Shared};
use zemacs_gui::{Length, Node, Run, Scene, Style};

const PATIENCE: Duration = Duration::from_secs(20);
static NTH: AtomicUsize = AtomicUsize::new(0);

fn wait<T>(shared: &Shared, what: &str, f: impl Fn(&Editor) -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(v) = f(&shared.lock().unwrap()) {
            return v;
        }
        if Instant::now() >= deadline {
            let ed = shared.lock().unwrap();
            let seen = &ed.messages[ed.messages.len().saturating_sub(8)..];
            panic!(
                "timed out waiting for {what}; status={:?} nodes={:?} last={seen:#?}",
                ed.status,
                ed.buffer.scene.as_ref().map(|s| s.len()),
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_message(shared: &Shared, what: &str, pred: impl Fn(&str) -> bool) {
    wait(shared, what, |ed| {
        ed.messages.iter().any(|m| pred(m)).then_some(())
    });
}

/// Evaluate `form` and wait for its value, printed with `~a`, to be `want`.
///
/// Numbered, because the log is cumulative: without the counter a second `NIL`
/// would be satisfied by the first one and the wait would prove nothing.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    let tag = NTH.fetch_add(1, Ordering::Relaxed);
    lisp.eval(format!("(message (format nil \"#{tag} ~a\" {form}))"));
    let want = format!("#{tag} {want}");
    wait_message(shared, form, |m| m == want);
}

/// Evaluate `form` and answer its value, printed with `~a`.
///
/// [`says`] with the expectation taken out, for the one question whose answer
/// this test cannot know in advance: whether the machine it is running on has a
/// working `latex`.
fn asks(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str) -> String {
    let tag = NTH.fetch_add(1, Ordering::Relaxed);
    lisp.eval(format!("(message (format nil \"#{tag} ~a\" {form}))"));
    let prefix = format!("#{tag} ");
    wait(shared, form, |ed| {
        ed.messages
            .iter()
            .find_map(|m| m.strip_prefix(&prefix).map(str::to_string))
    })
}

fn runtime(file: &str) -> String {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime").join(file))
        .unwrap_or_else(|e| panic!("runtime/{file} must exist: {e}"))
        .display()
        .to_string()
}

// --- reading a page --------------------------------------------------------
//
// Six helpers, all of them walks of the arena. A scene is an arena and a root
// rather than a tree of boxes, which is what lets a `Frame` name the node it
// came from — so "every text node" is a filter over `0..len` and nothing has to
// recurse.

/// Every `Text` node's runs, in the order the builder pushed them.
fn paragraphs(scene: &Scene) -> Vec<&[Run]> {
    (0..scene.len())
        .filter_map(|id| match scene.node(id) {
            Some(Node::Text { runs, .. }) => Some(runs.as_slice()),
            _ => None,
        })
        .collect()
}

/// What one paragraph puts on the page, its runs joined.
///
/// This is the *drawn* text and not the buffer's: `*bold*` is `bold` here, and
/// a `[[id:unit-2][the next unit]]` is `the next unit`. Which is the point —
/// every assertion below about what is on the page is an assertion about what
/// a reader sees, not about what the file says.
fn drawn(runs: &[Run]) -> String {
    runs.iter()
        .map(|r| match r {
            Run::Text { text, .. } => text.as_str(),
            Run::Image { .. } => "",
        })
        .collect()
}

/// The whole page as text, one paragraph per line.
fn page(scene: &Scene) -> String {
    paragraphs(scene)
        .iter()
        .map(|runs| drawn(runs))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The style of the first run whose text is exactly `text`.
fn style_of(scene: &Scene, text: &str) -> Style {
    for runs in paragraphs(scene) {
        for r in runs {
            if let Run::Text { text: t, style, .. } = r {
                if t == text {
                    return *style;
                }
            }
        }
    }
    panic!("no run reading {text:?} on the page:\n{}", page(scene));
}

/// Whether any run reads exactly `text`.
fn has_run(scene: &Scene, text: &str) -> bool {
    paragraphs(scene)
        .iter()
        .any(|runs| runs.iter().any(|r| matches!(r, Run::Text { text: t, .. } if t == text)))
}

/// Whether `needle` appears anywhere on the page.
///
/// Weaker than [`has_run`] and the right question wherever the *runs* are the
/// syntax highlighter's rather than this mode's: `def independent(...)` comes
/// back as half a dozen runs and which ones is Python's grammar's business.
fn has_text(scene: &Scene, needle: &str) -> bool {
    page(scene).contains(needle)
}

/// The blocks of the scene, as `(id, block)`.
fn blocks(scene: &Scene) -> Vec<(usize, &zemacs_gui::Block)> {
    (0..scene.len())
        .filter_map(|id| match scene.node(id) {
            Some(Node::Block(b)) => Some((id, b)),
            _ => None,
        })
        .collect()
}

/// A document with one of everything the mode has an opinion about.
///
/// Written to look like the thing it exists for — `docs/curriculum.org` — so
/// that what is asserted below is what a maths unit actually contains: a title,
/// a unit with an `:ID:` drawer, a table, a list, a quote, a rule, and a
/// programming problem whose answer is a Python block.
const UNIT: &str = "\
#+TITLE: Linear Algebra I
#+ZEMACS_CURRICULUM: 1

* Vectors and Spaces                                              :unit:linear:
:PROPERTIES:
:ID: unit-1
:END:

A basis is a *minimal* spanning set, and $\\dim V = n$ counts it. See
[[id:unit-2][the next unit]].

\\[
  \\dim(U + W) = \\dim U + \\dim W - \\dim(U \\cap W)
\\]

The subtraction is inclusion-exclusion.

| Space | Dimension |
|-------+-----------|
| R^2 | *two* |
| R^n | n |

- [ ] transcribe the proof
- a plain item
1. and an ordered one

-----

#+begin_quote
All models are wrong,
but some are useful.
#+end_quote

** Problem
:PROPERTIES:
:ZEMACS_PROBLEM: programming
:END:

#+begin_src python
def solve(a, b):

    return None
#+end_src
";

#[test]
fn frozen_org_builds_a_page_of_nodes_that_the_cell_grid_could_not_have_drawn() {
    let init = std::env::temp_dir().join("zemacs_test_org_frozen_init.lisp");
    std::fs::write(
        &init,
        format!(
            // The shipped config, not the handful of files this needs picked
            // out of it. `org-fold.lisp` calls `define-leader`, which
            // `init.lisp` defines — so loading the mode files directly, in any
            // order, fails on the first leader binding. Loading the real entry
            // point is also the more honest test: it is the order the editor
            // actually uses, and it is what puts `gui.lisp` — the builders this
            // whole mode is now written against — in front of `org-frozen.lisp`.
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(message "org-frozen test init loaded")
"#,
            runtime("init.lisp"),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);

    wait_message(&shared, "the runtime files to load", |m| {
        m == "org-frozen test init loaded"
    });
    let seen = shared.lock().unwrap().messages.clone();
    assert!(
        !seen.iter().any(|m| m.contains("error")),
        "org-frozen.lisp must load cleanly; got {seen:#?}"
    );

    {
        let mut ed = shared.lock().unwrap();
        ed.load(
            UNIT,
            Some(PathBuf::from("/tmp/zemacs_test_frozen_unit.org")),
            Some("org".into()),
        );
        ed.apply(EditorCommand::SetMode(Mode::Normal));
        ed.apply(EditorCommand::MoveTo(0));
    }

    // --- entering ------------------------------------------------------------
    //
    // The command *and* the hook, which is the two halves of what happens for
    // real: `(org-frozen-mode)` is what `M-x` and `SPC m z` reach and is what
    // tells the *editor* which mode the buffer is in, and `X-hook` is what the
    // application calls back a frame later and is what runs the body. Nothing
    // drains the application's queue in a headless image, so the second half is
    // called here.
    lisp.eval("(setf *org-latex-auto* nil)".into());
    lisp.eval("(progn (org-frozen-mode) (org-frozen-mode-hook))".into());
    wait(&shared, "a page to go up", |ed| {
        ed.buffer.scene.as_ref().map(|_| ())
    });
    says(&shared, &lisp, "(major-mode)", "org-frozen-mode");
    says(&shared, &lisp, "(if (derived-mode-p 'org-mode) t nil)", "T");
    // ...and then the page again with **nothing on the hook**.
    //
    // The shipped config is loaded whole, and `math.lisp` puts itself on
    // `*org-frozen-node-functions*` — which is the hook working, and is that
    // file's test to make. Everything asserted below is a claim about *this*
    // file, so the page it is asserted against is this file's alone; the hook
    // gets its own section further down, with a decorator written here.
    assert_eq!(
        asks(
            &shared,
            &lisp,
            r#"(progn (setf *org-frozen-node-functions* nil)
                      (org-frozen-refresh) "undecorated")"#
        ),
        "undecorated"
    );
    // A malformed page is a message and *not* an installed scene, so a parse
    // error would leave every assertion below reading the previous page. There
    // was no previous page, but saying so here names the failure.
    {
        let ed = shared.lock().unwrap();
        assert!(
            !ed.messages.iter().any(|m| m.starts_with("scene-set:")),
            "the page did not parse: {:#?}",
            ed.messages
        );
    }

    // --- read-only is real, twice over ---------------------------------------
    //
    // The mode claims it and installing a scene claims it again, from the other
    // direction: a scene is not an editing surface. Not "the edit keys are
    // unbound" either way — the guard is the single one in `Editor::apply`, so
    // every route into the text is refused.
    says(&shared, &lisp, "(buffer-read-only-p)", "CLAIMED");
    {
        let mut ed = shared.lock().unwrap();
        assert_eq!(ed.buffer.read_only(), ReadOnly::Claimed);
        let before = ed.buffer.text.to_string();

        ed.apply(EditorCommand::InsertText("nope".into()));
        ed.apply(EditorCommand::DeleteForward);
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::Undo);

        assert_eq!(ed.buffer.text.to_string(), before, "a frozen page was edited");
        assert_ne!(ed.mode, Mode::Insert, "a frozen page let us into Insert");
        assert!(!ed.buffer.modified);
    }

    // --- the machinery is not on the page ------------------------------------
    //
    // The strong form of the claim, and it got stronger with the port: these
    // strings are not hidden by a fold, not dimmed, not replaced by a glyph —
    // **no node was ever built for them**. Five functions used to exist to make
    // a grid pretend otherwise.
    {
        let ed = shared.lock().unwrap();
        let scene = ed.buffer.scene.as_ref().unwrap();
        let text = page(scene);
        for gone in [
            ":PROPERTIES:",
            ":END:",
            ":ID: unit-1",
            ":ZEMACS_PROBLEM: programming",
            "#+ZEMACS_CURRICULUM:",
            "#+begin_src",
            "#+end_src",
            "#+begin_quote",
            "#+end_quote",
            "#+TITLE:",
        ] {
            assert!(!text.contains(gone), "{gone:?} is still on the page:\n{text}");
        }
        // ...and the document itself is untouched. A renderer that got this by
        // deleting text would pass every assertion above and lose your file.
        assert_eq!(ed.buffer.text.to_string(), UNIT);
        // The prose either side of a hidden run is still there, which is what
        // stops "build nothing" from passing this test.
        assert!(text.contains("A basis is a"), "{text}");
        // Two source lines, one paragraph: a paragraph is a *flow*, so the
        // lines are joined and the font decides where the break falls. The
        // space between them is the assertion — joining without one is the
        // mistake that reads as `usefulbut`.
        assert!(
            text.contains("All models are wrong, but some are useful."),
            "a paragraph is one flow across its source lines:\n{text}"
        );
        assert!(text.contains("def solve(a, b):"), "{text}");
        // A tag is a filing decision and not something the page says.
        assert!(!text.contains(":unit:linear:"), "{text}");
    }

    // --- a heading is a larger *run*, not a larger line -----------------------
    //
    // The thing the grid could not do at all. `Overlay::scale` is a property of
    // a *line* wherever it lands — the widest scale claimed by any overlay
    // touching a line sets that line's cell — so org-modern sizes a heading by
    // putting `scale` on its stars and then needs a second overlay on the text
    // to embolden it, because weight is per-run and size is not. Here both are
    // keywords on the same run, and the bullet beside it is set *smaller* than
    // the words it announces, which nothing on a grid can be.
    {
        let ed = shared.lock().unwrap();
        let scene = ed.buffer.scene.as_ref().unwrap();
        let heading = style_of(scene, "Vectors and Spaces");
        assert_eq!(heading.size, 150, "a level-1 heading is 1.5x the body");
        assert!(heading.bold);
        assert_eq!(heading.face, HlKind::from_name("heading-1").map(|k| k.face_id()));

        let deeper = style_of(scene, "Problem");
        assert_eq!(deeper.size, 125, "a level-2 heading is 1.25x");

        // The bullet is its own run inside the same paragraph, at its own size.
        let bullet = style_of(scene, "\u{25cf} ");
        assert!(
            bullet.size < heading.size,
            "the bullet is smaller than the heading: {bullet:?} vs {heading:?}"
        );
        // ...and in the **coding** face, while the words beside it are prose.
        // Not an inconsistency: `*org-modern-stars*` is org-modern's table and
        // org-modern draws on the cell grid, so every glyph in it was chosen
        // against the coverage of the font the grid uses. A reading serif has
        // no reason to carry `●`, and asking one for it draws nothing at all —
        // which is what "org-modern's bullets do not work here" looked like.
        assert_eq!(bullet.family, zemacs_gui::Family::Mono);
        assert_eq!(heading.family, zemacs_gui::Family::Prose);

        // Body prose is body-sized, which is what makes the numbers above mean
        // something rather than being whatever everything is.
        assert_eq!(style_of(scene, "A basis is a ").size, 100);
    }

    // --- a source block is one Text node per line, spans intact ---------------
    //
    // `docs/gui.org` is explicit: *a code block is one `text` node per line*,
    // not one node with newlines in it, because a paragraph is a flow and there
    // are no hard breaks in one — every whitespace is a break opportunity and
    // nothing forces one. Per line is what a listing wants anyway, since each
    // line carries its own syntax runs.
    {
        let ed = shared.lock().unwrap();
        let scene = ed.buffer.scene.as_ref().unwrap();
        let band = HlKind::from_name("modeline").map(|k| k.face_id());
        // By what is *in* it, not by being the first block with that colour: a
        // mode decorating through `*org-frozen-node-functions*` is free to tint
        // a pill with the same face, and a test that assumed otherwise would
        // start failing on somebody else's change.
        let (_, listing) = blocks(scene)
            .into_iter()
            .find(|(_, b)| {
                b.children.iter().any(|&c| {
                    matches!(scene.node(c), Some(Node::Text { runs, .. })
                             if drawn(runs) == "def solve(a, b):")
                })
            })
            .expect("a source block gets a band the width of the page");
        assert_eq!(listing.background, band, "and the band is the code face");
        assert_eq!(
            listing.children.len(),
            3,
            "three body lines, three paragraphs — the blank one included"
        );
        assert!(listing.pad > 0, "and a gutter, which is padding and not a prefix");
        assert_eq!(listing.width, Length::Pct(100));

        let line_of = |n: usize| match scene.node(listing.children[n]) {
            Some(Node::Text { runs, .. }) => runs.clone(),
            other => panic!("child {n} of a listing is {other:?}, not a paragraph"),
        };
        assert_eq!(drawn(&line_of(0)), "def solve(a, b):");
        assert_eq!(drawn(&line_of(2)), "    return None");
        // A blank line keeps its row: a paragraph of one space, which is one
        // line and so one line's height. It laid out to *nothing* until
        // `crates/gui` learned to flush a paragraph that never met a word, and
        // a blank line between two functions closed the listing up.
        assert_eq!(drawn(&line_of(1)), " ");
        // ...and the whole listing is set in the **coding** face, which is not a
        // preference: a scene sets prose in a proportional one, and a column of
        // code lines up only if every character is the same width.
        for n in 0..3 {
            assert!(
                line_of(n).iter().all(|r| matches!(r, Run::Text { style, .. }
                                                   if style.family == zemacs_gui::Family::Mono)),
                "line {n} of a listing is not monospaced: {:?}",
                line_of(n)
            );
        }
        // Prose is not, which is what makes the line above mean something.
        assert_eq!(style_of(scene, "A basis is a ").family, zemacs_gui::Family::Prose);

        // ...and it is set **as Python**, in a buffer whose own language is org
        // and for which no tree-sitter grammar exists at all. `def` is a keyword
        // to Python's grammar and nothing whatever to org's, so a face here can
        // only have come from the nested parse — and it is a face on a *run*,
        // with no overlay made anywhere, which is the ceiling this port closed
        // rather than deferred.
        let faces = |n: usize| -> Vec<u8> {
            line_of(n)
                .iter()
                .filter_map(|r| match r {
                    Run::Text { style, .. } => style.face,
                    Run::Image { .. } => None,
                })
                .collect()
        };
        assert!(
            faces(0).contains(&HlKind::Keyword.face_id()),
            "`def` is a Python keyword: {:?}",
            line_of(0)
        );
        // `None` is a Python constant. Two different faces on two different
        // lines is what says these are real spans rather than one blanket
        // colour — and that the spans were cut back into lines correctly.
        assert!(
            faces(2).contains(&HlKind::Constant.face_id()),
            "`None` is a Python constant: {:?}",
            line_of(2)
        );
        // The ceiling this used to carry is *closed* rather than raised: it read
        // "one overlay per token run, and a long listing has a few hundred; and
        // `Overlays` is a `Vec` scanned linearly per drawn line". A span is a
        // run inside its line's paragraph now and no overlay is made for it.
        assert!(
            !ed.buffer
                .overlays()
                .iter()
                .any(|o| o.face == Some(HlKind::Keyword) || o.face == Some(HlKind::Constant)),
            "a scene makes no face overlay per token run"
        );
    }

    // --- a table cell keeps its emphasis --------------------------------------
    //
    // The gain, and it changes what a curriculum author may write.
    // `%org-modern-line` skips a `|` row whole — `|--+--|` would otherwise read
    // as a `+strike+` run — and the grid version of this mode then rebuilt each
    // row as one flat `display` string with the padding recomputed. So
    // `| *two* |` drew as the six literal characters `*two*`, in a mode whose
    // whole premise is that markup is drawn rather than typed. A cell is a
    // paragraph like any other now.
    {
        let ed = shared.lock().unwrap();
        let scene = ed.buffer.scene.as_ref().unwrap();
        assert!(has_run(scene, "two"), "the cell's emphasis is drawn:\n{}", page(scene));
        assert!(!page(scene).contains("*two*"), "...and its markers are not");
        assert!(style_of(scene, "two").bold);
        // The header is *weight*, not colour: the rule under it already says
        // where it ends, and a coloured header row reads as a highlight.
        assert!(style_of(scene, "Dimension").bold);
        assert!(!style_of(scene, "R^2").bold, "only the header is bold");

        // Columns line up because every row states the **same** width, which is
        // the one thing `docs/gui.org` insists on about tables: a column's width
        // is a constraint across siblings and no per-child `Length` says it, so
        // `Auto` would size each cell to its own content and column one would be
        // as wide as `Space` in one row and `R^2` in the next.
        let cells: Vec<Vec<Length>> = blocks(scene)
            .iter()
            .filter(|(_, b)| b.dir == zemacs_gui::Dir::Row && b.children.len() == 2)
            .map(|(_, b)| {
                b.children
                    .iter()
                    .filter_map(|&c| match scene.node(c) {
                        Some(Node::Block(cell)) => Some(cell.width),
                        _ => None,
                    })
                    .collect()
            })
            .filter(|row: &Vec<Length>| row.len() == 2)
            .filter(|row| matches!(row[0], Length::Pct(_)) && matches!(row[1], Length::Pct(_)))
            .collect();
        assert!(cells.len() >= 3, "three content rows of two cells: {cells:?}");
        assert!(
            cells.iter().all(|r| r == &cells[0]),
            "every row must state the same column widths: {cells:?}"
        );
        // A *percentage*, not a pixel count. Lisp cannot measure a font — that
        // is the Rust column of the boundary — so a pixel width would be a
        // character count times a guess, and a guess that came out low would
        // wrap every cell in the table. A share of the measure needs no guess.
        assert_eq!(
            cells[0].iter().filter_map(|l| match l {
                Length::Pct(p) => Some(*p as u32),
                _ => None,
            }).sum::<u32>(),
            100,
            "the columns share the measure exactly: {cells:?}"
        );
    }

    // --- a wrapped list item hangs under its own text -------------------------
    //
    // The second gain. `Overlay::line_prefix` is drawn in the left margin of
    // *every* row of every line it touches, first row included — right for a
    // source block and wrong for a list item, where the first row carries the
    // bullet and the continuations have to align with the text. So the marker
    // and the body are two children of a row, and the body is a box of its own.
    {
        let ed = shared.lock().unwrap();
        let scene = ed.buffer.scene.as_ref().unwrap();
        let rows: Vec<&zemacs_gui::Block> = blocks(scene)
            .into_iter()
            .map(|(_, b)| b)
            .filter(|b| b.dir == zemacs_gui::Dir::Row && b.children.len() == 2)
            .filter(|b| {
                matches!(scene.node(b.children[1]), Some(Node::Block(x)) if x.width == Length::Fill)
            })
            .collect();
        assert_eq!(rows.len(), 3, "three list items, each a marker and a body");
        for r in &rows {
            match scene.node(r.children[0]) {
                Some(Node::Block(marker)) => assert_eq!(
                    marker.width,
                    Length::Auto,
                    "a marker's box is as wide as the marker: a stated pixel \
                     width is a guess about a font, and a `1.` that did not fit \
                     one would wrap inside its own column"
                ),
                other => panic!("a list item's marker is {other:?}"),
            }
        }
        // Every substituted glyph is in the coding face, for the reason the
        // heading's bullet is: these are org-modern's tables, picked against the
        // grid's font, and a proportional face draws a blank for most of them.
        for glyph in ["\u{2022}", "1.", "\u{25a1} "] {
            let style = style_of(scene, glyph);
            assert_eq!(
                style.family,
                zemacs_gui::Family::Mono,
                "{glyph:?} is a stand-in for punctuation and needs the coding face"
            );
        }
        // An *ordered* list, which the grid declined entirely:
        // `*org-modern-list-bullets*` has no entry for `1.` with the reason
        // beside it — a number is not one character and has no glyph to become,
        // and a substitution had to be the width of what it replaced.
        assert!(has_run(scene, "1."), "an ordered marker:\n{}", page(scene));
        assert!(has_run(scene, "\u{2022}"), "and a bulleted one");
        // A checkbox is drawn, in the item's own flow rather than beside it.
        assert!(
            page(scene).contains("\u{25a1} transcribe the proof"),
            "the cookie is a glyph in the sentence:\n{}",
            page(scene)
        );
    }

    // --- a rule is a rect, and the title is centred ---------------------------
    //
    // `-----` used to be `─` repeated 72 times, with a parameter whose own
    // docstring explained that the image cannot know the pane's width. A scene's
    // lengths include a percentage of the parent's content box, so the parameter
    // now means what it says.
    {
        let ed = shared.lock().unwrap();
        let scene = ed.buffer.scene.as_ref().unwrap();
        let markup = HlKind::from_name("markup").map(|k| k.face_id());
        let rules: Vec<&Node> = (0..scene.len())
            .filter_map(|id| scene.node(id))
            .filter(|n| matches!(n, Node::Rect { fill, .. } if *fill == markup))
            .collect();
        assert!(
            rules.iter().any(|n| matches!(n, Node::Rect { w: Length::Pct(60), .. })),
            "the horizontal rule is a rect as wide as the measure says: {rules:?}"
        );
        assert!(
            rules
                .iter()
                .any(|n| matches!(n, Node::Rect { w: Length::Pct(100), .. })),
            "the table's rule row is a hairline the width of the table, not a run \
             of box characters: {rules:?}"
        );
        // Every rule is a hairline and not a band: heights are the *only* place
        // this mode states a pixel, and it states them through one function so
        // that a display's scale is one number rather than a dozen.
        assert!(
            rules.iter().all(|n| matches!(n, Node::Rect { h: Length::Px(p), .. } if *p <= 4)),
            "{rules:?}"
        );

        // The title is a run at twice the body, centred — and centring is a
        // third thing an overlay has no way to say, since it has no notion of
        // where a line's middle is.
        assert_eq!(style_of(scene, "Linear Algebra I").size, 200);
        assert!(style_of(scene, "Linear Algebra I").bold);
        let centred = (0..scene.len()).filter_map(|id| scene.node(id)).any(|n| {
            matches!(n, Node::Text { runs, align: zemacs_gui::Align::Center }
                     if drawn(runs) == "Linear Algebra I")
        });
        assert!(centred, "a title is centred");
    }

    // --- mathematics --------------------------------------------------------
    //
    // The construct the whole mode exists for: `examples/math/` has fourteen
    // inline fragments in 206 lines. Two shapes, and telling them apart is the
    // work — `latex-fragments` answers `(START END DISPLAY)` and the third
    // element, which every other caller in the tree drops with `(declare
    // (ignore display))`, is exactly the bit a scene needs.
    //
    // Asserted first with `*org-latex-auto*` off, which is the state a machine
    // with no TeX is in, because *the classification is right either way*: a
    // display equation gets a line of its own and an inline one is marked as
    // maths rather than left as prose with dollars in it.
    {
        let ed = shared.lock().unwrap();
        let scene = ed.buffer.scene.as_ref().unwrap();
        let code = HlKind::from_name("code").map(|k| k.face_id());

        // The inline fragment is its *own run* inside the sentence, not a
        // paragraph of its own and not swallowed by the prose around it.
        assert!(has_run(scene, "$\\dim V = n$"), "{}", page(scene));
        assert_eq!(style_of(scene, "$\\dim V = n$").face, code);
        let sentence = paragraphs(scene)
            .into_iter()
            .find(|runs| drawn(runs).starts_with("A basis is a"))
            .expect("the sentence is one paragraph");
        assert_eq!(
            drawn(sentence),
            "A basis is a minimal spanning set, and $\\dim V = n$ counts it. \
             See the next unit.",
            "the equation sits in the flow, in order, with the prose either side"
        );

        // The display equation claims its lines whole, so the paragraph before
        // it and the paragraph after it are two paragraphs and not one with a
        // hole in the middle.
        assert!(
            paragraphs(scene)
                .iter()
                .any(|r| drawn(r) == "The subtraction is inclusion-exclusion."),
            "{}",
            page(scene)
        );
        let display = blocks(scene)
            .into_iter()
            .find(|(_, b)| {
                b.align == zemacs_gui::Align::Center
                    && b.children.iter().any(|&c| {
                        matches!(scene.node(c), Some(Node::Text { runs, .. })
                                 if drawn(runs).contains("\\dim(U + W)"))
                    })
            })
            .expect("a display equation is centred in a block of its own");
        assert_eq!(display.1.width, Length::Pct(100));
    }

    // ...and now the real thing, if this machine can typeset. A cold render is
    // a few hundred milliseconds of shelling out to TeX per fragment, and
    // `latex-preview` keys its id on the source, the dpi and the foreground
    // colour — so the second page costs a hash lookup and the equations follow
    // `load-theme` and `C-+` for free.
    let tex = asks(
        &shared,
        &lisp,
        r#"(progn (setf *org-latex-auto* t) (org-frozen-refresh)
                  (if *org-latex-auto* "yes" "no"))"#,
    );
    if tex == "yes" {
        let ed = shared.lock().unwrap();
        let scene = ed.buffer.scene.as_ref().unwrap();
        // An **image run**: a bitmap in the middle of a sentence, wrapping like
        // a word and hanging `depth` pixels below the baseline. This is the
        // thing an earlier draft of `docs/gui.org` said a scene could not do,
        // and the one place the cell grid was ahead.
        let inline = paragraphs(scene)
            .into_iter()
            .find(|runs| drawn(runs).starts_with("A basis is a"))
            .expect("the sentence is still one paragraph");
        assert!(
            inline.iter().any(|r| matches!(r, Run::Image { .. })),
            "the inline fragment is a bitmap in the flow: {inline:?}"
        );
        assert!(!page(scene).contains("$"), "and its source is off the page");
        // Core filled the size and the depth in from its own image table, which
        // is why `(image-run ID)` is the whole form a document writes.
        let sized = inline.iter().any(
            |r| matches!(r, Run::Image { width, height, .. } if *width > 0 && *height > 0),
        );
        assert!(sized, "an image run with no size lays out to nothing: {inline:?}");

        // ...and the display equation is a block-level figure.
        assert!(
            (0..scene.len())
                .filter_map(|id| scene.node(id))
                .any(|n| matches!(n, Node::Image { width, height, .. } if *width > 0 && *height > 0)),
            "a display equation is an image node:\n{}",
            page(scene)
        );
    } else {
        eprintln!("no working `latex` here — the typeset half of the maths is not checked");
    }
    lisp.eval("(progn (setf *org-latex-auto* nil) (org-frozen-refresh))".into());
    wait(&shared, "the page back without its previews", |ed| {
        ed.buffer
            .scene
            .as_ref()
            .filter(|s| has_run(s, "$\\dim V = n$"))
            .map(|_| ())
    });

    // --- a link is clickable, which is new --------------------------------------
    //
    // There was no pointer path into a link at all before this: `RET` on one
    // needs a *point*, and a scene has none. A link's run carries a `:tag` — an
    // integer this image chose, handed back by Rust when a click lands inside —
    // and the closure it names is written where the widget is.
    {
        let ed = shared.lock().unwrap();
        let scene = ed.buffer.scene.as_ref().unwrap();
        let tag = paragraphs(scene)
            .iter()
            .flat_map(|runs| runs.iter())
            .find_map(|r| match r {
                Run::Text { text, tag, .. } if text == "the next unit" => Some(*tag),
                _ => None,
            })
            .expect("the link's description is on the page");
        assert!(tag.is_some(), "and it carries a tag");
        assert_eq!(
            style_of(scene, "the next unit").face,
            HlKind::from_name("link").map(|k| k.face_id())
        );
    }

    // --- the decoration hook ----------------------------------------------------
    //
    // The seam a mode built on top of this one uses, and the contract is the
    // docstring on `*org-frozen-node-functions*`: called with a plist, answering
    // a list of extra arguments spliced into that heading's `block` form. The
    // point of asserting it here is that neither half is a curriculum's — this
    // file knows nothing about problems, and `math.lisp` needs nothing added
    // here to make one clickable.
    lisp.eval(
        r#"(progn
             (defun %test-decorate (h)
               (when (and (eq (getf h :kind) :heading)
                          (member "unit" (getf h :tags) :test #'string=))
                 (list :tag (lambda () (message "clicked a unit"))
                       :border "link"
                       (text (run (format nil "level ~d at ~d"
                                          (getf h :level) (getf h :line))
                                  :face "comment")))))
             (pushnew '%test-decorate *org-frozen-node-functions*)
             (org-frozen-refresh))"#
            .into(),
    );
    wait(&shared, "the hook's node to reach the page", |ed| {
        ed.buffer
            .scene
            .as_ref()
            .filter(|s| has_run(s, "level 1 at 3"))
            .map(|_| ())
    });
    {
        let ed = shared.lock().unwrap();
        let scene = ed.buffer.scene.as_ref().unwrap();
        // The keywords land on the heading's own block: a tag makes the whole
        // box clickable, padding included, which is the one thing that stops a
        // pill from feeling broken.
        let decorated = blocks(scene)
            .into_iter()
            .find(|(_, b)| b.border == HlKind::from_name("link").map(|k| k.face_id()))
            .expect("the hook's :border reached the heading's block");
        assert!(decorated.1.tag.is_some(), "and its :tag");
        assert_eq!(decorated.1.children.len(), 2, "the heading, then the hook's node");
        // Only the heading it asked for. A hook that decorated every heading
        // would pass every assertion above.
        assert_eq!(
            blocks(scene).iter().filter(|(_, b)| b.tag.is_some()).count(),
            1
        );
    }
    // The tag is an integer and the closure is in the image; `%scene-click` is
    // what the editor calls with it, and it is the whole callback layer.
    let tag = {
        let ed = shared.lock().unwrap();
        let scene = ed.buffer.scene.as_ref().unwrap();
        blocks(scene).into_iter().find_map(|(_, b)| b.tag).unwrap()
    };
    lisp.eval(format!("(%scene-click {tag})"));
    wait_message(&shared, "the click to reach its closure", |m| {
        m == "clicked a unit"
    });
    lisp.eval("(setf *org-frozen-node-functions* nil)".into());

    // --- a program may still write, and the page follows it ---------------------
    //
    // A frozen buffer is read-only *to the keyboard* and is still written to.
    // The loop closes itself: a write moves the revision, the change hook fires,
    // the builder runs, and `scene-set` swaps the tree — carrying the scroll, so
    // a reader on page nine stays on page nine.
    says(
        &shared,
        &lisp,
        r#"(with-inhibited-read-only (buffer-read-only-p))"#,
        "NIL",
    );
    says(&shared, &lisp, "(buffer-read-only-p)", "CLAIMED");
    lisp.eval(
        r#"(progn
             (with-inhibited-read-only
               (replace-region (search-forward "R^n" 0) (+ 3 (search-forward "R^n" 0)) "R^k"))
             (org-frozen-refresh))"#
            .into(),
    );
    wait(&shared, "the page to follow the write", |ed| {
        ed.buffer
            .scene
            .as_ref()
            .filter(|s| has_run(s, "R^k"))
            .map(|_| ())
    });

    // --- leaving takes the page down and gives the document back ----------------
    //
    // Nothing was ever rewritten to draw the page, so leaving is a `(scene-set)`
    // with no argument. The read-only claim goes with it — both of them, the
    // one the mode made and the one the scene made, in the order that leaves the
    // buffer editable rather than frozen forever.
    lisp.eval("(org-frozen-toggle)".into());
    lisp.eval("(org-mode-hook)".into());
    says(&shared, &lisp, "(buffer-read-only-p)", "NIL");
    {
        let mut ed = shared.lock().unwrap();
        assert!(ed.buffer.scene.is_none(), "the page is down");
        // The document is what it was, bar the one word a *program* wrote into
        // it above: the page never rewrote a character to draw itself, so the
        // whole of leaving is dropping the page and the claim.
        assert_eq!(
            ed.buffer.text.to_string(),
            UNIT.replace("R^n", "R^k"),
            "the page rewrote the document"
        );
        assert_eq!(ed.buffer.read_only(), ReadOnly::No);
        // ...and the buffer takes an edit again.
        ed.apply(EditorCommand::MoveTo(0));
        ed.apply(EditorCommand::InsertText("x".into()));
        assert!(ed.buffer.text.to_string().starts_with('x'), "still frozen");
        assert!(
            ed.buffer.text.to_string().contains("A basis is a"),
            "and the rest of the document is still there"
        );
    }

    // --- the document this was built for ---------------------------------------
    //
    // `examples/math/linear-algebra.org` is a real curriculum written to
    // `docs/curriculum.org`'s spec — three units, five problems, LaTeX, a
    // figure, `:ZEMACS_*` drawers throughout. Rendering it is the assertion the
    // synthetic document above cannot make: that nothing here falls over on a
    // file somebody actually wrote.
    //
    // Its title carries an em dash, and that is the encoding proof. Lisp holds
    // buffer text as UTF-8 *bytes*, one character per byte, while the encoder
    // that carries a scene back to Rust encodes each *character* — so a string
    // that skipped `utf8-text` arrives twice-encoded and is drawn as the
    // Latin-1 reading of its own encoding. A scene makes that worse than the
    // grid did rather than better: every run is measured in a real font, so
    // three characters where the document has one is a wrong wrap and a wrong
    // column width as well as three wrong glyphs.
    let sample =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/math/linear-algebra.org");
    if let Ok(text) = std::fs::read_to_string(&sample) {
        {
            let mut ed = shared.lock().unwrap();
            ed.load(&text, Some(std::fs::canonicalize(&sample).unwrap()), Some("org".into()));
            ed.apply(EditorCommand::SetMode(Mode::Normal));
            ed.apply(EditorCommand::MoveTo(0));
        }
        // The LaTeX pass is `init.lisp`'s and is tested where it lives; here it
        // would be a `latex` subprocess per fragment on a document full of them,
        // to prove something this file does not do.
        lisp.eval("(setf *org-latex-auto* nil)".into());
        lisp.eval("(progn (org-frozen-mode) (org-frozen-mode-hook))".into());
        wait(&shared, "the curriculum's page", |ed| {
            ed.buffer
                .scene
                .as_ref()
                .filter(|s| has_text(s, "def independent"))
                .map(|_| ())
        });
        let ed = shared.lock().unwrap();
        let scene = ed.buffer.scene.as_ref().unwrap();
        assert!(
            has_run(scene, "Linear Algebra I \u{2014} Vectors, Maps, and Elimination"),
            "the em dash survived as one character:\n{}",
            page(scene).lines().take(4).collect::<Vec<_>>().join("\n")
        );
        let shown = page(scene);
        for gone in [":PROPERTIES:", ":END:", ":ZEMACS_STATUS:", "#+begin_src", "#+end_src"] {
            assert!(!shown.contains(gone), "{gone:?} is still on the curriculum's page");
        }
        assert!(shown.contains("Prove that every basis of"), "the prose survives:\n{shown}");
        // The Contents section is a list of links, every one of them clickable.
        assert!(has_run(scene, "1. Vectors and Spaces"));

        // --- a dash *and* an equation on the same line -----------------------
        //
        // The one that will actually happen, and the reason `utf8-text`
        // exists. `latex-fragments` answers **character** offsets into the
        // buffer; `line-string` answers UTF-8 **bytes**, one Lisp character per
        // byte. Line 116 of this file is
        //
        //     Prove the rank–nullity theorem: for $T : V \to W$ with $V$ ...
        //
        // and the en dash in front of the first fragment is three bytes and one
        // character. Cut the line at the byte index and every fragment on it
        // lands two characters late: the equation swallows the two characters
        // before it and gives back two of its own, and the page still looks
        // almost right — which is exactly why this is asserted on the *runs*
        // rather than on the page's text. The concatenation is the same string
        // either way; only where the boundaries fall differs.
        //
        // Read with `*org-latex-auto*` off on purpose. A fragment that will not
        // typeset leaves its **source** on the page — a missing equation is
        // worse than an ugly one — so the fallback run is the fragment verbatim
        // and is the sharpest possible statement of where the cut fell.
        {
            let dashed = paragraphs(scene)
                .into_iter()
                .find(|runs| drawn(runs).starts_with("Prove the rank"))
                .expect("the rank-nullity problem is one paragraph");
            let texts: Vec<&str> = dashed
                .iter()
                .filter_map(|r| match r {
                    Run::Text { text, .. } => Some(text.as_str()),
                    Run::Image { .. } => None,
                })
                .collect();
            assert!(
                texts.contains(&"Prove the rank\u{2013}nullity theorem: for "),
                "the prose before the equation ends where the `$` does, and the \
                 en dash in it is one character: {texts:?}"
            );
            assert!(
                texts.contains(&"$T : V \\to W$"),
                "the fragment is cut at its own characters, not two bytes late: \
                 {texts:?}"
            );
            // ...and the second fragment on the same line, which is where a
            // one-off error would have accumulated into a two-off one.
            assert!(texts.contains(&"$V$"), "{texts:?}");
            assert!(
                texts.contains(&" finite-dimensional,"),
                "and the prose after it starts where the equation stopped: {texts:?}"
            );
        }

        // Nothing was rewritten to draw any of it.
        assert_eq!(ed.buffer.text.to_string(), text);
    }

    // --- the pieces, without the editor ----------------------------------------
    //
    // Cheap to ask directly, and it pins each decision to a reason rather than
    // to a count in the document above.
    says(&shared, &lisp, r##"(%org-frozen-keyword "#+TITLE: x" "TITLE")"##, "x");
    says(&shared, &lisp, r#"(%org-frozen-keyword "  #+title:  x " "TITLE")"#, "x");
    says(&shared, &lisp, r##"(%org-frozen-keyword "#+AUTHOR: x" "TITLE")"##, "NIL");
    says(&shared, &lisp, r##"(car (%org-frozen-block-open "#+begin_src python :tangle a.py"))"##, "src");
    says(&shared, &lisp, r##"(cdr (%org-frozen-block-open "#+BEGIN_SRC Python"))"##, "python");
    says(&shared, &lisp, r##"(cdr (%org-frozen-block-open "#+begin_quote"))"##, "NIL");
    // Babel's names and tree-sitter's ids are two vocabularies; the table is the
    // disagreement, and a name no grammar answers to is simply not highlighted.
    says(&shared, &lisp, r#"(%org-frozen-language "emacs-lisp")"#, "lisp");
    says(&shared, &lisp, r#"(%org-frozen-language "sh")"#, "sh");
    says(&shared, &lisp, r#"(length (%org-frozen-highlight "sh" "echo hi"))"#, "0");
    // Org's own rule for a drawer — `:WORD:` alone on a line — applied rather
    // than a list of names, so `:LOGBOOK:` and a config's own drawer work. And
    // `:END:` excluded by name, because it closes one and would otherwise open a
    // second that never ends.
    says(&shared, &lisp, r#"(if (%org-frozen-drawer-p ":PROPERTIES:") t nil)"#, "T");
    says(&shared, &lisp, r#"(if (%org-frozen-drawer-p ":LOGBOOK:") t nil)"#, "T");
    says(&shared, &lisp, r#"(if (%org-frozen-drawer-p ":END:") t nil)"#, "NIL");
    says(&shared, &lisp, r#"(if (%org-frozen-drawer-p ":ID: unit-1") t nil)"#, "NIL");
    // Five dashes is org's own threshold, which is what keeps `---` in prose
    // from becoming a rule.
    says(&shared, &lisp, r#"(if (%org-frozen-rule-p "-----") t nil)"#, "T");
    says(&shared, &lisp, r#"(if (%org-frozen-rule-p "---") t nil)"#, "NIL");
    // A tag run is whitespace-preceded and takes its whitespace with it. The
    // function did not change and its contract did: the index is into whatever
    // it was handed, and the builder hands it a *decoded* line.
    says(&shared, &lisp, r#"(%org-frozen-tags "* Alpha   :a:b:")"#, "7");
    says(&shared, &lisp, r#"(%org-frozen-tags "* Alpha")"#, "NIL");
    says(&shared, &lisp, r#"(%org-frozen-tags "* Ratio a : b")"#, "NIL");
    says(&shared, &lisp, r#"(format nil "~{~a~^,~}" (%org-frozen-tag-list "  :a:b:"))"#, "a,b");
    // A rule row is org's `|---+---|` and its spellings; `| | |` is an empty
    // row and not a rule.
    says(&shared, &lisp, r#"(if (%org-frozen-rule-row-p "|---+---|") t nil)"#, "T");
    says(&shared, &lisp, r#"(if (%org-frozen-rule-row-p "| | |") t nil)"#, "NIL");
    // The block table answers flags now and not overlay property names — the
    // `case` survived the port and its vocabulary did not.
    says(&shared, &lisp, r#"(getf (%org-frozen-block "src") :highlight)"#, "T");
    says(&shared, &lisp, r#"(getf (%org-frozen-block "quote") :quote)"#, "T");
    says(&shared, &lisp, r#"(getf (%org-frozen-block "comment") :hide)"#, "T");
    says(&shared, &lisp, r#"(getf (%org-frozen-block "aside") :band)"#, "T");
    // An ordered list marker, which is a whole construct the grid declined.
    says(&shared, &lisp, r#"(second (%org-frozen-list-item "  1) go"))"#, "1.");
    says(&shared, &lisp, r#"(first (%org-frozen-list-item "  - go"))"#, "2");
    says(&shared, &lisp, r#"(if (%org-frozen-list-item "not a list") t nil)"#, "NIL");
    // Org's escaping comma, which used to need an overlay covering two
    // characters to draw one — a `display` of the empty string clears the
    // property rather than hiding anything — and is now a string.
    says(&shared, &lisp, r#"(%org-frozen-uncomma ",#+end_src")"#, "#+end_src");
    says(&shared, &lisp, r#"(%org-frozen-uncomma ", not markup")"#, ", not markup");

    // --- the decoder, which is what makes any of the above true of real text ----
    //
    // `utf8-text` is shared now — it is in `runtime/modes/modes.lisp`, and
    // `crates/lisp/tests/utf8.rs` is where it is proved byte for byte against a
    // known sample. What stays here is the pair of facts *this* mode depends
    // on: that it is reachable through a full `init.lisp` boot, and that a table
    // column is measured in the characters a cell spells rather than its bytes.
    //
    // Written with `code-char` and never with a literal, and that is not
    // fussiness: a non-ASCII literal in a form *evaluated from Rust* is the
    // second half of the same bug, so a test spelling the em dash here would be
    // asserting against mojibake it introduced itself.
    const EM_DASH: &str = "(coerce (list (code-char 226) (code-char 128) (code-char 148)) 'string)";
    says(&shared, &lisp, &format!("(length (utf8-text {EM_DASH}))"), "1");
    says(
        &shared,
        &lisp,
        &format!("(char-code (char (utf8-text {EM_DASH}) 0))"),
        "8212",
    );
    // ASCII is the identity, which is both the fast path and the common one.
    says(&shared, &lisp, r#"(utf8-text "abc")"#, "abc");
    // A truncated sequence is passed through one character at a time rather than
    // dropped: this is a renderer, and text it cannot make sense of should still
    // be on the page.
    says(&shared, &lisp, r#"(length (utf8-text (string (code-char 200))))"#, "1");
    // ...and that is what a column measures. A cell holding an em dash is *one*
    // character wide, so the column stated for it lines up; measuring the
    // undecoded bytes would have made it three and pulled every row after it.
    says(
        &shared,
        &lisp,
        &format!("(length (first (%org-frozen-cells (format nil \"| ~a | bb |\" {EM_DASH}))))"),
        "1",
    );
    says(&shared, &lisp, r#"(length (%org-frozen-cells "| a | bb |"))"#, "2");
    // A column's width is what its cells *draw*, not what they say — which is
    // the arithmetic the emphasis-in-a-cell gain depends on.
    says(&shared, &lisp, r#"(%org-frozen-drawn (%org-frozen-pieces "a *bold* c"))"#, "a bold c");
    // A paragraph is one flow across its source lines, and two lines set the
    // same way come out as **one run**, joined by the space that joined them.
    //
    // That is the mitigation for the thing you can see on the page when it is
    // not done: `crates/gui` measures a paragraph one whitespace-delimited
    // segment at a time and adds the widths up, while `crates/render` draws a
    // run's segments as a single string — so a run's drawn width is
    // `advance(whole)` and its recorded width is the sum of `advance(part)`,
    // and kerning makes the sum bigger while an italic's shear makes it
    // smaller. Inside a run that cannot show. At the boundary between two it
    // shows in full, as words overspaced or run together. Every boundary this
    // file does not create is one place that cannot happen.
    //
    // It takes a *chunk* per line and not a string, because by this point each
    // line has already been cut into pieces and its equations turned into
    // images.
    says(
        &shared,
        &lisp,
        r#"(%org-frozen-paragraph
             (list (%org-frozen-pieces "one") (%org-frozen-pieces "two")))"#,
        r#"(text (run "one two"))"#,
    );
    // ...and two pieces set *differently* stay two runs, which is what stops
    // the coalescing above from being "draw everything the same".
    says(
        &shared,
        &lisp,
        r#"(%org-frozen-paragraph (list (%org-frozen-pieces "a *b* c")))"#,
        r#"(text (run "a ") (run "b" :bold t :face 17) (run " c"))"#,
    );
    says(
        &shared,
        &lisp,
        r#"(%org-frozen-drawn (%org-frozen-pieces "see [[id:u][the unit]]"))"#,
        "see the unit",
    );
}
