//! Headless proof of `runtime/modes/org-frozen.lisp` — org rendered as a
//! printed page rather than edited as a manuscript.
//!
//! Everything this mode does ends up as an overlay on a real [`Editor`], so the
//! only honest check is the one `org_modern.rs` makes: boot the real image, put
//! a document in front of it, and read back what the editor actually holds. The
//! *interesting* assertions here are the two that no amount of Lisp could fake —
//! that a line has genuinely stopped occupying a row ([`fold_hiding`], which is
//! the same predicate the renderer and `j` ask), and that a `#+begin_src python`
//! body carries Python's own highlight spans inside a buffer whose language is
//! org.
//!
//! Deliberately a single `#[test]`, as in every file beside it: `cl_boot`
//! initialises a process-wide Lisp image, so there is exactly one `spawn` per
//! test binary and a new file is the only way to add one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use zemacs_core::{fold_hiding, Editor, EditorCommand, HlKind, Mode, ReadOnly, Shared};

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
                "timed out waiting for {what}; status={:?} overlays={} last={seen:#?}",
                ed.status,
                ed.buffer.overlays().len(),
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

fn runtime(file: &str) -> String {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime").join(file))
        .unwrap_or_else(|e| panic!("runtime/{file} must exist: {e}"))
        .display()
        .to_string()
}

/// The 0-based buffer lines the renderer would not draw.
///
/// `fold_hiding` and not "some overlay carries fold": that is the predicate the
/// draw loop and `j` both ask, so a line this reports is a line that has
/// genuinely stopped existing on screen. Asking any other way would prove
/// something about the overlay rather than about the page.
fn hidden_lines(ed: &Editor) -> Vec<usize> {
    (0..ed.buffer.len_lines())
        .filter(|&l| fold_hiding(ed.buffer.overlays(), ed.buffer.line_start(l)).is_some())
        .collect()
}

/// The text of the lines that are still drawn, joined — the page, as far as the
/// renderer is concerned, before substitutions.
fn drawn(ed: &Editor) -> String {
    (0..ed.buffer.len_lines())
        .filter(|&l| fold_hiding(ed.buffer.overlays(), ed.buffer.line_start(l)).is_none())
        .map(|l| {
            let (s, e) = (ed.buffer.line_start(l), ed.buffer.line_end(l));
            ed.buffer.slice_string(s, e)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The display string of the overlay starting at `at`, or `None`.
fn display_at(ed: &Editor, at: usize) -> Option<String> {
    ed.buffer
        .overlays()
        .iter()
        .find(|o| o.start == at && o.display.is_some())
        .and_then(|o| o.display.clone())
}

/// Every overlay `display` string in the buffer, in creation order.
fn displays(ed: &Editor) -> Vec<String> {
    ed.buffer
        .overlays()
        .iter()
        .filter_map(|o| o.display.clone())
        .collect()
}

/// Char offset of the first occurrence of `needle`.
fn at(ed: &Editor, needle: &str) -> usize {
    let text = ed.buffer.text.to_string();
    let byte = text.find(needle).unwrap_or_else(|| panic!("{needle:?} is in the document"));
    text[..byte].chars().count()
}

/// Char offset of the start of the first line that is exactly `line`.
///
/// Needed wherever the substring would be ambiguous, and the case that bit is
/// the honest one: `-----` is also a substring of the table's `|-------+---|`
/// rule row, which comes first in the document.
fn line_at(ed: &Editor, line: &str) -> usize {
    (0..ed.buffer.len_lines())
        .find(|&l| {
            let (s, e) = (ed.buffer.line_start(l), ed.buffer.line_end(l));
            ed.buffer.slice_string(s, e) == line
        })
        .map(|l| ed.buffer.line_start(l))
        .unwrap_or_else(|| panic!("a line reading {line:?} is in the document"))
}

/// Faces claimed by overlays covering `pos`, most recently made last.
fn faces_at(ed: &Editor, pos: usize) -> Vec<HlKind> {
    ed.buffer
        .overlays()
        .iter()
        .filter(|o| o.start <= pos && pos < o.end)
        .filter_map(|o| o.face)
        .collect()
}

/// A document with one of everything the mode has an opinion about.
///
/// Written to look like the thing it exists for — `docs/curriculum.org` — so
/// that what is asserted below is what a maths unit actually contains: a title,
/// a unit with an `:ID:` drawer, a table, a quote, a rule, and a programming
/// problem whose answer is a Python block.
const UNIT: &str = "\
#+TITLE: Linear Algebra I
#+ZEMACS_CURRICULUM: 1

* Vectors and Spaces                                              :unit:linear:
:PROPERTIES:
:ID: unit-1
:END:

A basis is a *minimal* spanning set. See [[id:unit-2][the next unit]].

| Space | Dimension |
|-------+-----------|
| R^2 | 2 |
| R^n | n |

-----

#+begin_quote
All models are wrong.
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
fn frozen_org_hides_its_machinery_typesets_its_page_and_refuses_the_keyboard() {
    let init = std::env::temp_dir().join("zemacs_test_org_frozen_init.lisp");
    std::fs::write(
        &init,
        format!(
            // The shipped config, not the four files this needs picked out of
            // it. `org-fold.lisp` calls `define-leader`, which `init.lisp`
            // defines — so loading the mode files directly, in any order, fails
            // on the first leader binding. Loading the real entry point is also
            // the more honest test: it is the order the editor actually uses,
            // and `fold.rs` and `math.rs` both do it this way.
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
    //
    // Both are needed and the first is the one that is easy to leave out: every
    // `derived-mode-p` in the runtime asks the *editor*, so a hook run without
    // it leaves `org-modern-appear` believing this is an ordinary org buffer.
    // The whole installation is org-mode's body running first — org-modern, the
    // previews, the figures — and this mode's on top of it.
    lisp.eval("(progn (org-frozen-mode) (org-frozen-mode-hook))".into());
    wait_message(&shared, "the page to be drawn", |m| m.contains("block"));
    says(&shared, &lisp, "(major-mode)", "org-frozen-mode");
    says(&shared, &lisp, "(if (derived-mode-p 'org-mode) t nil)", "T");

    // --- read-only is real ---------------------------------------------------
    //
    // Not "the edit keys are unbound": the guard is the single one in
    // `Editor::apply`, so *every* route into the text is refused — a keystroke,
    // an `:s` substitution, an `insert` from Lisp — and adding a new command
    // that mutates the document cannot accidentally get past it.
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
        assert!(ed.status.contains("read-only"), "status: {}", ed.status);
        assert!(!ed.buffer.modified);
    }
    // ...and `i` is refused before it becomes a mode change, so the modeline
    // never claims INSERT on a document that will not take a character. The
    // message names *freezing* rather than denying the file exists, which is the
    // difference between this and `*magit*`.
    {
        let mut ed = shared.lock().unwrap();
        let out = ed.handle_key(zemacs_core::Key::Char('i'));
        assert!(
            matches!(&out[..], [EditorCommand::Message(m)] if m.contains("read-only")),
            "`i` on a frozen page: {out:?}"
        );
    }
    // The cursor still moves, though — that is the whole difference between a
    // read-only buffer and a disabled one.
    {
        let mut ed = shared.lock().unwrap();
        ed.apply(EditorCommand::MoveTo(0));
        let out = ed.handle_key(zemacs_core::Key::Char('j'));
        for cmd in out {
            ed.apply(cmd);
        }
        assert!(ed.buffer.cursor > 0, "`j` moved nowhere on a frozen page");
    }

    // --- the machinery is not on the page ------------------------------------
    //
    // The strong form of the claim: these strings are not dimmed, not replaced
    // by a glyph — the lines they are on do not occupy a row, so `j` steps over
    // them too. Asked of the lines the *renderer* would draw.
    {
        let ed = shared.lock().unwrap();
        let page = drawn(&ed);
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
        ] {
            assert!(!page.contains(gone), "{gone:?} is still on the page:\n{page}");
        }
        // ...and the document itself is untouched. A renderer that got this by
        // deleting text would pass every assertion above and lose your file.
        assert_eq!(ed.buffer.text.to_string(), UNIT);
        // The prose either side of a hidden run is still there, which is what
        // stops "hide everything" from passing this test.
        assert!(page.contains("A basis is a"));
        assert!(page.contains("All models are wrong."));
        assert!(page.contains("def solve(a, b):"));
        // Consecutive machinery lines are one fold rather than one each: the
        // four-line drawer under the unit heading is hidden by a single overlay.
        assert!(hidden_lines(&ed).len() >= 9, "{:?}", hidden_lines(&ed));
    }

    // --- the title is typeset, not hidden ------------------------------------
    //
    // `#+TITLE:` is the one keyword that cannot be folded — a fold hides the
    // lines *after* its own, so line 0 is out of reach of every fold there can
    // ever be — and turning that constraint into the better answer is the point:
    // it is drawn as a title, at twice the body size.
    {
        let ed = shared.lock().unwrap();
        assert_eq!(display_at(&ed, 0).as_deref(), Some("Linear Algebra I"));
        let title = ed.buffer.overlays().iter().find(|o| o.start == 0).unwrap();
        assert_eq!(title.scale, Some(200), "a title is twice the body");
        assert_eq!(title.bold, Some(true));
    }

    // --- tags come off a heading ---------------------------------------------
    //
    // A tag is a filing decision and not something the page says. One space
    // rather than nothing, because a `display` of the empty string *clears* the
    // property — and at the end of a line one space and no space look alike.
    {
        let ed = shared.lock().unwrap();
        let tags = at(&ed, ":unit:linear:");
        let ov = ed
            .buffer
            .overlays()
            .iter()
            .find(|o| o.start < tags && tags < o.end && o.display.as_deref() == Some(" "));
        assert!(ov.is_some(), "the heading's tags are still drawn");
    }

    // --- a source block is a printed listing ---------------------------------
    //
    // Three claims, and the third is the one that needed a new primitive.
    {
        let ed = shared.lock().unwrap();
        let body = at(&ed, "def solve");

        // A band the width of the *pane*, which `background` cannot be: a
        // background paints the cells a range covers, so a block drawn with one
        // is a stripe as ragged as its own code.
        let band = ed
            .buffer
            .overlays()
            .iter()
            .find(|o| o.start <= body && body < o.end && o.line_background.is_some())
            .expect("a source block gets a band");
        assert_eq!(band.line_prefix.as_deref(), Some("  "), "and a gutter");

        // ...and it is highlighted **as Python**, in a buffer whose own language
        // is org and for which no tree-sitter grammar exists at all. `def` is a
        // keyword to Python's grammar and nothing whatever to org's, so a face
        // here can only have come from the nested parse.
        let kw = at(&ed, "def solve");
        assert!(
            faces_at(&ed, kw).contains(&HlKind::Keyword),
            "`def` is a Python keyword: {:?}",
            faces_at(&ed, kw)
        );
        // `None` is a Python constant. Two different faces inside one block is
        // what says these are real spans rather than one blanket colour.
        let none = at(&ed, "return None") + "return ".len();
        assert!(
            faces_at(&ed, none).contains(&HlKind::Constant),
            "`None` is a Python constant: {:?}",
            faces_at(&ed, none)
        );
    }

    // --- a quote is set apart, and a rule is a rule --------------------------
    {
        let ed = shared.lock().unwrap();
        let quote = at(&ed, "All models");
        let ov = ed
            .buffer
            .overlays()
            .iter()
            .find(|o| o.start <= quote && quote < o.end && o.line_prefix.is_some())
            .expect("a quote block gets a bar");
        assert_eq!(ov.italic, Some(true));
        assert_eq!(ov.face, Some(HlKind::Comment));

        // `-----` becomes one unbroken rule rather than five dashes.
        let rule = line_at(&ed, "-----");
        let drawn = display_at(&ed, rule).expect("a horizontal rule is drawn");
        assert!(drawn.chars().all(|c| c == '─'), "{drawn:?}");
        assert_eq!(drawn.chars().count(), 72);
    }

    // --- a table is aligned under a rule -------------------------------------
    //
    // Each row is one substitution carrying its own cells, repadded. The columns
    // are `Space` (5) and `Dimension` (9), so every row draws as 5 + 2 + its
    // last cell — and the rule spans exactly that.
    {
        let ed = shared.lock().unwrap();
        let head = line_at(&ed, "| Space | Dimension |");
        assert_eq!(display_at(&ed, head).as_deref(), Some("Space  Dimension"));
        let row = line_at(&ed, "| R^2 | 2 |");
        assert_eq!(
            display_at(&ed, row).as_deref(),
            Some("R^2    2"),
            "a narrow cell is padded to its column"
        );
        // The header is *weight*, not colour: the rule under it already says
        // where it ends.
        let bold = ed.buffer.overlays().iter().find(|o| o.start == head).unwrap();
        assert_eq!(bold.bold, Some(true));
        let body = ed.buffer.overlays().iter().find(|o| o.start == row).unwrap();
        assert_ne!(body.bold, Some(true), "only the header is bold");

        let rule = line_at(&ed, "|-------+-----------|");
        let drawn = display_at(&ed, rule).expect("the rule row is drawn as a rule");
        assert_eq!(drawn, "─".repeat(5 + 2 + 9));
    }

    // --- org-modern is still underneath, and never reveals --------------------
    //
    // Frozen mode is org-modern *plus more*: the bullets, the emphasis and the
    // link descriptions are that file's and are not redone here. What is
    // switched off is the one behaviour a renderer must not have.
    {
        let ed = shared.lock().unwrap();
        let all = displays(&ed);
        assert!(all.iter().any(|d| d == "◉"), "org-modern's bullet: {all:?}");
        assert!(all.iter().any(|d| d == "minimal"), "emphasis: {all:?}");
        assert!(all.iter().any(|d| d == "the next unit"), "a link: {all:?}");
    }
    // Put the cursor inside `*minimal*` and fire the hook that would normally
    // reveal it. In an ordinary org buffer this shows the asterisks; here it
    // must not, and the guard is a list org-modern owns rather than a test for
    // this mode written into that file.
    lisp.eval(
        r#"(progn (goto-char (1+ (search-forward "*minimal*" 0))) (org-modern-appear))"#.into(),
    );
    says(&shared, &lisp, "(if *org-modern-revealed* t nil)", "NIL");
    {
        let ed = shared.lock().unwrap();
        let emph = at(&ed, "*minimal*");
        assert_eq!(
            display_at(&ed, emph).as_deref(),
            Some("minimal"),
            "a frozen page revealed its markup under the cursor"
        );
    }

    // --- TAB still folds, and puts the machinery back ------------------------
    //
    // The interaction this mode had to solve: a fold is an overlay, org's own
    // cycle deletes every fold it finds in the subtree, and the drawers are
    // held shut by folds. `org-frozen-cycle` takes its own off, lets `org-cycle`
    // see a buffer folded only by itself, and puts them back.
    let before = shared.lock().unwrap().buffer.overlays().len();
    lisp.eval(
        r#"(progn (goto-char (search-forward "* Vectors" 0)) (org-frozen-cycle))"#.into(),
    );
    wait_message(&shared, "the subtree to fold", |m| m == "folded");
    {
        let ed = shared.lock().unwrap();
        let page = drawn(&ed);
        assert!(page.contains("Vectors and Spaces"), "the headline stays");
        assert!(!page.contains("A basis is a"), "its body is folded away");
    }
    // Open it again: the drawer is *still* hidden, which is the assertion the
    // whole of `org-frozen-cycle` exists for. Without it, one TAB permanently
    // reveals `:PROPERTIES:`.
    lisp.eval("(org-frozen-cycle)".into());
    lisp.eval("(org-frozen-cycle)".into());
    wait(&shared, "the subtree to open again", |ed| {
        drawn(ed).contains("A basis is a").then_some(())
    });
    {
        let ed = shared.lock().unwrap();
        let page = drawn(&ed);
        assert!(!page.contains(":PROPERTIES:"), "TAB revealed the drawer:\n{page}");
        assert!(!page.contains("#+begin_src"), "TAB revealed a delimiter");
        // ...and it did not stack a second copy of everything either.
        assert!(
            ed.buffer.overlays().len() <= before + 2,
            "cycling leaked overlays: {} -> {}",
            before,
            ed.buffer.overlays().len()
        );
    }

    // --- a program may still write, the keyboard may not ---------------------
    //
    // The maths curriculum is displayed frozen *and* is the buffer a transcribed
    // handwritten answer is written into. `with-inhibited-read-only` lifts a
    // claim a mode made — and only that: a generated buffer answers `:generated`
    // and is left alone, because its text is a rendering of state and an edit
    // there would be thrown away on the next refresh.
    says(
        &shared,
        &lisp,
        r#"(with-inhibited-read-only (buffer-read-only-p))"#,
        "NIL",
    );
    // The claim comes back — UNWIND-PROTECT, so it comes back through a throw
    // too, which is the failure this must not have.
    says(&shared, &lisp, "(buffer-read-only-p)", "CLAIMED");
    says(
        &shared,
        &lisp,
        r#"(ignore-errors (with-inhibited-read-only (error "boom")))"#,
        "NIL",
    );
    says(&shared, &lisp, "(buffer-read-only-p)", "CLAIMED");
    // ...and a write inside one lands.
    lisp.eval(
        r#"(with-inhibited-read-only
             (replace-region (search-forward "R^n" 0) (+ 3 (search-forward "R^n" 0)) "R^k"))"#
            .into(),
    );
    wait(&shared, "the Lisp write to land", |ed| {
        ed.buffer.text.to_string().contains("R^k").then_some(())
    });

    // --- leaving puts everything back ----------------------------------------
    //
    // The document was never modified to make the page, so leaving is a matter
    // of dropping overlays — exactly its own, so org-modern's bullets and the
    // LaTeX previews survive. The read-only claim goes with them.
    lisp.eval("(org-frozen-toggle)".into());
    lisp.eval("(org-mode-hook)".into());
    says(&shared, &lisp, "(buffer-read-only-p)", "NIL");
    {
        let mut ed = shared.lock().unwrap();
        let page = drawn(&ed);
        assert!(page.contains(":PROPERTIES:"), "the drawer is back:\n{page}");
        assert!(page.contains("#+begin_src python"), "the delimiter is back");
        assert_eq!(hidden_lines(&ed), Vec::<usize>::new(), "nothing is folded");
        // org-modern's own substitutions were never frozen mode's to remove.
        assert!(displays(&ed).iter().any(|d| d == "◉"), "the bullets survive");
        // ...and the buffer takes an edit again.
        ed.apply(EditorCommand::MoveTo(0));
        ed.apply(EditorCommand::InsertText("x".into()));
        assert!(ed.buffer.text.to_string().starts_with('x'), "still frozen");
    }

    // --- the document this was built for -------------------------------------
    //
    // `examples/math/linear-algebra.org` is a real curriculum written to
    // `docs/curriculum.org`'s spec — three units, five problems, LaTeX, a
    // figure, `:ZEMACS_*' drawers throughout. Rendering it is the assertion the
    // synthetic document above cannot make: that nothing here falls over on a
    // file somebody actually wrote.
    //
    // Its title carries an em dash, which is the *encoding* proof and the one
    // failure mode of this file that no other assertion reaches. A display
    // string is drawn correctly right up until buffer text is joined to a
    // literal — see `%org-frozen-text` — and a title is buffer text handed
    // straight through, so a wrong answer here would be `Ã¢ÂÂ` rather than a
    // panic somewhere obvious.
    let sample = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/math/linear-algebra.org");
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
        // Waited on the *last* thing the render does, which is the blocks —
        // `org-frozen-refresh` folds, then typesets the headers, then the
        // tables, then the listings. Waiting on the title would return while the
        // pass that highlights this block was still to run, and the assertion
        // below would be a coin toss.
        wait(&shared, "the curriculum's listing to be highlighted", |ed| {
            faces_at(ed, at(ed, "def independent"))
                .contains(&HlKind::Keyword)
                .then_some(())
        });
        let ed = shared.lock().unwrap();
        // The em dash survives — three bytes in the buffer, one character on the
        // page. This is the encoding proof and the one failure mode of this file
        // no other assertion reaches: a wrong answer here is mojibake, not a
        // panic somewhere obvious.
        assert_eq!(
            display_at(&ed, 0).as_deref(),
            Some("Linear Algebra I \u{2014} Vectors, Maps, and Elimination")
        );
        let page = drawn(&ed);
        for gone in [":PROPERTIES:", ":END:", ":ZEMACS_STATUS:", "#+begin_src", "#+end_src"] {
            assert!(!page.contains(gone), "{gone:?} is still on the curriculum's page");
        }
        assert!(page.contains("Prove that every basis of"), "the prose survives:\n{page}");
    }

    // --- the pieces, without the editor --------------------------------------
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
    says(&shared, &lisp, r#"(length (highlight "sh" "echo hi"))"#, "0");
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
    // A tag run is whitespace-preceded and takes its whitespace with it, so the
    // substitution leaves one invisible cell instead of a ragged gap.
    says(&shared, &lisp, r#"(%org-frozen-tags "* Alpha   :a:b:")"#, "7");
    says(&shared, &lisp, r#"(%org-frozen-tags "* Alpha")"#, "NIL");
    says(&shared, &lisp, r#"(%org-frozen-tags "* Ratio a : b")"#, "NIL");
    // A rule row is org's `|---+---|` and its spellings; `| | |` is an empty
    // row and not a rule.
    says(&shared, &lisp, r#"(if (%org-frozen-rule-row-p "|---+---|") t nil)"#, "T");
    says(&shared, &lisp, r#"(if (%org-frozen-rule-row-p "| | |") t nil)"#, "NIL");
    // A block whose kind nothing knows is still *some* kind of set-apart text,
    // and gets a band rather than being drawn as prose.
    says(&shared, &lisp, r#"(getf (%org-frozen-block "src") :highlight)"#, "T");
    says(&shared, &lisp, r#"(getf (%org-frozen-block "comment") :hide)"#, "T");
    says(&shared, &lisp, r#"(getf (%org-frozen-block "aside") 'line-background)"#, "modeline");

    // --- the decoder, which is what makes any of the above true of real text --
    //
    // Written with `code-char` and never with a literal, and that is not
    // fussiness: a non-ASCII literal in a form *evaluated from Rust* is the
    // second half of the same bug — `TODO.org` has it under `:bug:` — so a test
    // spelling `—` here would be asserting against mojibake it introduced
    // itself. Byte values are ASCII to write and mean exactly one thing.
    const EM_DASH: &str = "(coerce (list (code-char 226) (code-char 128) (code-char 148)) 'string)";
    // Three bytes in, one character out, and it is the right one.
    says(&shared, &lisp, &format!("(length (%org-frozen-text {EM_DASH}))"), "1");
    says(
        &shared,
        &lisp,
        &format!("(char-code (char (%org-frozen-text {EM_DASH}) 0))"),
        "8212",
    );
    // ASCII is the identity, which is both the fast path and the common one.
    says(&shared, &lisp, r#"(%org-frozen-text "abc")"#, "abc");
    // A truncated sequence is passed through one character at a time rather than
    // dropped: this is a renderer, and text it cannot make sense of should still
    // be on the page.
    says(&shared, &lisp, r#"(length (%org-frozen-text (string (code-char 200))))"#, "1");
    // ...and that is what a column measures. A cell holding an em dash is *one*
    // column wide, so the table beside it lines up; measuring the undecoded
    // bytes would have made it three and pulled every row after it left.
    says(
        &shared,
        &lisp,
        &format!("(length (first (%org-frozen-cells (format nil \"| ~a | bb |\" {EM_DASH}))))"),
        "1",
    );
    says(&shared, &lisp, r#"(length (%org-frozen-cells "| a | bb |"))"#, "2");
    // A narrow cell is padded to its column and the last one never is, so a row
    // ends where its text does.
    says(&shared, &lisp, r#"(%org-frozen-row '("a" "bb") '(3 2))"#, "a    bb");
}
