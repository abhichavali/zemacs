//! Headless proof of `runtime/modes/math.lisp` — a maths curriculum as one org
//! file — and of the two things `org-modern.lisp` grew to serve it: following a
//! link, and drawing a figure.
//!
//! The schema is the *published contract*: two further parts of this feature are
//! written against `math-problems`, `math-set-status`, `math-response-insert`
//! and `math-src-block`, so what is asserted below is their shape and their
//! promises rather than any particular implementation of them. In particular:
//!
//! * `math-response-insert` is **one edit** — proved by undoing it and getting
//!   the original document back, since `replace-region` is the only primitive
//!   that makes a single checkpoint — and **idempotent**, proved by writing the
//!   same text twice and comparing the whole buffer.
//! * a status **round-trips** through the buffer, including from a problem that
//!   had no `:ZEMACS_STATUS:` line at all, which is the case a generated file
//!   will actually produce.
//!
//! The figure assertion is the only one that reaches Rust: `image-file` reads an
//! SVG off the disk, rasterises it and answers an id, and the proof that all of
//! that worked is an overlay in the editor carrying an `image`.
//!
//! Deliberately a single `#[test]`, as in every file beside it: `cl_boot`
//! initialises a process-wide Lisp image, so there is exactly one `spawn` per
//! test binary and a new file is the only way to add one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Mode, Shared};

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
            panic!("timed out waiting for {what}; status={:?} last={seen:#?}", ed.status);
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

/// Run `form` for its effect and wait until it has landed.
///
/// The trailing marker is the whole mechanism: Lisp runs on its own thread, so
/// an `eval` returns long before the edit it asked for reaches the editor, and
/// reading the buffer straight afterwards would race it.
fn does(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str) {
    let tag = NTH.fetch_add(1, Ordering::Relaxed);
    lisp.eval(format!("(progn {form} (message \"#{tag} done\"))"));
    let want = format!("#{tag} done");
    wait_message(shared, form, |m| m == want);
}

fn text(shared: &Shared) -> String {
    let ed = shared.lock().unwrap();
    ed.buffer.slice_string(0, ed.buffer.len_chars())
}

fn runtime(file: &str) -> String {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime").join(file))
        .unwrap_or_else(|e| panic!("runtime/{file} must exist: {e}"))
        .display()
        .to_string()
}

/// One small curriculum with one of everything the format has, and three things
/// that must *not* be mistaken for one: a `* Contents` heading with no `:ID:`
/// (and therefore not a unit), a problem with no `:ZEMACS_STATUS:` at all, and a
/// problem with no `*** Response` section to write into.
const CURRICULUM: &str = "\
#+TITLE: Linear Algebra I
#+ZEMACS_CURRICULUM: 1

* Contents
- [[id:unit-1][1. Vectors and Spaces]]
- [[id:unit-2][2. Matrix Multiplication]]

* Vectors and Spaces
:PROPERTIES:
:ID: unit-1
:END:

A basis is a linearly independent spanning set.

[[file:zemacs-test-fig.svg]]

** Problem
:PROPERTIES:
:ZEMACS_PROBLEM: written
:ZEMACS_STATUS: todo
:END:

Prove that every basis of a finite-dimensional space has the same cardinality.

*** Response

** Problem
:PROPERTIES:
:ZEMACS_PROBLEM: programming
:ZEMACS_PACKAGES: numpy sympy
:ZEMACS_STATUS: todo
:END:

Implement Gaussian elimination with partial pivoting.

#+begin_src python :tangle gaussian.py
import numpy as np

def solve(A, b):
    return None
#+end_src

* Matrix Multiplication
:PROPERTIES:
:ID: unit-2
:END:

Composition of linear maps.

** Problem
:PROPERTIES:
:ZEMACS_PROBLEM: written
:END:

Show that matrix multiplication is associative.
";

#[test]
fn a_curriculum_is_read_navigated_and_written_back() {
    let dir = std::env::temp_dir();
    let org = dir.join("zemacs_test_math.org");
    // The figure lives beside the .org file, because that is what
    // `[[file:...]]` means in a document and what `%org-expand-file` implements.
    std::fs::write(
        dir.join("zemacs-test-fig.svg"),
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="60" height="40">
              <rect width="60" height="40" fill="#4488cc"/>
            </svg>"##,
    )
    .unwrap();

    let init = dir.join("zemacs_test_math_init.lisp");
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(load {:?} :verbose nil :print nil)
(load {:?} :verbose nil :print nil)
(message "math test init loaded")
"#,
            runtime("modes/modes.lisp"),
            runtime("modes/org-modern.lisp"),
            runtime("modes/math.lisp"),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);

    wait_message(&shared, "the runtime files to load", |m| {
        m == "math test init loaded"
    });
    let seen = shared.lock().unwrap().messages.clone();
    assert!(
        !seen.iter().any(|m| m.contains("error")),
        "math.lisp must load cleanly; got {seen:#?}"
    );

    {
        let mut ed = shared.lock().unwrap();
        ed.load(CURRICULUM, Some(PathBuf::from(&org)), Some("org".into()));
        ed.apply(EditorCommand::SetMode(Mode::Normal));
        ed.apply(EditorCommand::MoveTo(0));
    }

    // --- the file says what it is --------------------------------------------
    says(&shared, &lisp, "(if (math-curriculum-p) t nil)", "T");

    // --- units ----------------------------------------------------------------
    //
    // Two, not three: `* Contents` carries no `:ID:` and is therefore not a
    // unit. That rule is what makes an unresolvable Contents link impossible to
    // write — losing the id loses the unit too.
    says(&shared, &lisp, "(length (math-units))", "2");
    says(&shared, &lisp, "(getf (first (math-units)) :id)", "unit-1");
    says(&shared, &lisp, "(getf (first (math-units)) :title)", "Vectors and Spaces");
    says(&shared, &lisp, "(getf (second (math-units)) :id)", "unit-2");

    // A unit's span covers its problems, which is what makes `math-problems`
    // able to take one as a filter at all.
    says(
        &shared,
        &lisp,
        r#"(progn (goto-char (search-forward "A basis is" 0))
                 (getf (math-unit-at-point) :id))"#,
        "unit-1",
    );
    // ...and NIL in the Contents section, which is the honest answer there.
    says(
        &shared,
        &lisp,
        r#"(progn (goto-char (search-forward "1. Vectors" 0)) (math-unit-at-point))"#,
        "NIL",
    );

    // --- problems -------------------------------------------------------------
    says(&shared, &lisp, "(length (math-problems))", "3");
    says(&shared, &lisp, "(length (math-problems (first (math-units))))", "2");
    says(&shared, &lisp, "(length (math-problems (second (math-units))))", "1");

    says(&shared, &lisp, "(getf (first (math-problems)) :kind)", "written");
    says(&shared, &lisp, "(getf (second (math-problems)) :kind)", "programming");
    says(&shared, &lisp, "(getf (first (math-problems)) :id)", "unit-1");
    // The owning unit is tracked forwards, so a problem in the *second* unit
    // must not still be reported under the first.
    says(&shared, &lisp, "(getf (third (math-problems)) :id)", "unit-2");

    // Packages: a list of strings, and empty rather than NIL-ish for a written
    // problem that never mentions any.
    says(
        &shared,
        &lisp,
        r#"(format nil "~{~a~^|~}" (getf (second (math-problems)) :packages))"#,
        "numpy|sympy",
    );
    says(&shared, &lisp, "(length (getf (first (math-problems)) :packages))", "0");

    // No `:ZEMACS_STATUS:` line at all still reads as `todo` — a generated file
    // is allowed to leave the default out.
    says(&shared, &lisp, "(getf (third (math-problems)) :status)", "todo");

    // A Response section that exists but is empty answers a span, and one that
    // does not exist answers NIL. Those are different questions and the capture
    // pass branches on the difference.
    says(&shared, &lisp, "(if (getf (first (math-problems)) :response-begin) t nil)", "T");
    says(&shared, &lisp, "(getf (third (math-problems)) :response-begin)", "NIL");
    says(&shared, &lisp, "(getf (third (math-problems)) :response-end)", "NIL");

    says(
        &shared,
        &lisp,
        r#"(progn (goto-char (search-forward "Prove that every basis" 0))
                 (getf (math-problem-at-point) :kind))"#,
        "written",
    );
    // Point in the preamble is inside no problem.
    says(&shared, &lisp, "(progn (goto-char 0) (math-problem-at-point))", "NIL");

    // --- the source block -----------------------------------------------------
    //
    // `:begin`/`:end` span the *body*, which is the promise a runner depends on:
    // the offsets and the text have to be the same thing, or replacing the range
    // would corrupt the block.
    says(
        &shared,
        &lisp,
        "(getf (math-src-block (second (math-problems))) :lang)",
        "python",
    );
    says(
        &shared,
        &lisp,
        "(getf (math-src-block (second (math-problems))) :tangle)",
        "gaussian.py",
    );
    says(
        &shared,
        &lisp,
        r#"(let ((b (math-src-block (second (math-problems)))))
             (if (string= (buffer-substring (getf b :begin) (getf b :end)) (getf b :body))
                 t nil))"#,
        "T",
    );
    says(
        &shared,
        &lisp,
        r#"(subseq (getf (math-src-block (second (math-problems))) :body) 0 12)"#,
        "import numpy",
    );
    // A written problem has none, and says so rather than answering the next
    // problem's.
    says(&shared, &lisp, "(math-src-block (first (math-problems)))", "NIL");

    // --- status round-trips ---------------------------------------------------
    does(&shared, &lisp, r#"(math-set-status (first (math-problems)) "done")"#);
    says(&shared, &lisp, "(getf (first (math-problems)) :status)", "done");
    // A symbol is accepted as well as a string — `'todo` is what a config writes.
    does(&shared, &lisp, "(math-set-status (first (math-problems)) 'todo)");
    says(&shared, &lisp, "(getf (first (math-problems)) :status)", "todo");

    // ...and into a drawer that has no status line yet, which is the case a
    // generated file actually produces. The other properties must survive it.
    does(&shared, &lisp, r#"(math-set-status (third (math-problems)) "done")"#);
    says(&shared, &lisp, "(getf (third (math-problems)) :status)", "done");
    says(&shared, &lisp, "(getf (third (math-problems)) :kind)", "written");
    says(&shared, &lisp, "(getf (third (math-problems)) :id)", "unit-2");
    says(&shared, &lisp, "(length (math-problems))", "3");

    // --- the response is one edit, and writing it twice changes nothing -------
    let before = text(&shared);
    does(
        &shared,
        &lisp,
        r#"(math-response-insert (first (math-problems)) "Any two bases are equinumerous.")"#,
    );
    let once = text(&shared);
    assert!(
        once.contains("Any two bases are equinumerous."),
        "the response lands in the document: {once}"
    );
    assert_ne!(before, once);

    // One `replace-region`, therefore one checkpoint, therefore one undo — which
    // is the only observable difference between an atomic write and a
    // `delete-region` followed by an `insert`.
    does(&shared, &lisp, "(undo)");
    assert_eq!(text(&shared), before, "the response insert is a single undo step");

    does(
        &shared,
        &lisp,
        r#"(math-response-insert (first (math-problems)) "Any two bases are equinumerous.")"#,
    );
    let again = text(&shared);
    assert_eq!(once, again, "redoing the same insert reproduces the same document");
    does(
        &shared,
        &lisp,
        r#"(math-response-insert (first (math-problems)) "Any two bases are equinumerous.")"#,
    );
    assert_eq!(
        again,
        text(&shared),
        "math-response-insert is idempotent: the same text twice is the same document"
    );

    // ...and it *creates* the section when a problem has none, one level deeper
    // than the problem itself.
    does(&shared, &lisp, r#"(math-response-insert (third (math-problems)) "AB times C.")"#);
    let created = text(&shared);
    assert!(
        created.contains("*** Response\nAB times C.\n"),
        "a missing Response section is created under the problem: {created}"
    );
    says(&shared, &lisp, "(if (getf (third (math-problems)) :response-begin) t nil)", "T");
    // ...and is then an ordinary Response like any other: writing again replaces
    // rather than appends.
    does(&shared, &lisp, r#"(math-response-insert (third (math-problems)) "AB times C.")"#);
    assert_eq!(created, text(&shared), "the created section is idempotent too");
    // The heading count has not moved: one Response was made, not two.
    says(&shared, &lisp, "(length (math-problems))", "3");

    // --- following a link -----------------------------------------------------
    //
    // The whole point of the Contents section: `RET` on `[[id:unit-2][...]]`
    // lands in that unit. `org-open-at-point` is `<ret>`'s command in org
    // buffers.
    says(
        &shared,
        &lisp,
        r#"(progn (goto-char (search-forward "[[id:unit-2]" 0))
                 (org-open-at-point)
                 (getf (math-unit-at-point) :id))"#,
        "unit-2",
    );
    // An id nobody has says so rather than jumping somewhere arbitrary.
    says(&shared, &lisp, r#"(%org-id-heading "no-such-unit")"#, "NIL");
    // The scheme parser, which is what keeps a *title* from being read as one:
    // `1. Vectors and Spaces` has a colon in it about as often as not.
    says(&shared, &lisp, r#"(car (%org-link-scheme "id:unit-1"))"#, "id");
    says(&shared, &lisp, r#"(%org-link-scheme "1. Vectors: a start")"#, "NIL");
    says(&shared, &lisp, r#"(cdr (%org-link-parts "[[id:unit-1][1. Vectors]]"))"#, "1. Vectors");
    says(&shared, &lisp, r#"(car (%org-link-parts "[[id:unit-1]]"))"#, "id:unit-1");

    // --- figures --------------------------------------------------------------
    //
    // Exactly one link in the document is a figure, and org-modern declines to
    // draw it as text — which is what stops two overlays claiming the same cells
    // and the renderer picking between them by age.
    says(&shared, &lisp, "(length (%org-image-links))", "1");
    says(&shared, &lisp, r#"(%org-modern-render :link "[[file:a.svg]]")"#, "NIL");
    says(
        &shared,
        &lisp,
        r#"(getf (%org-modern-render :link "[[id:unit-1][1. Vectors]]") 'display)"#,
        "1. Vectors",
    );
    // An `https:` image is *not* a figure: nothing here fetches, and a broken
    // image is worse than a URL you can read.
    says(&shared, &lisp, r#"(%org-image-target "https://x/a.png")"#, "NIL");
    says(&shared, &lisp, r#"(%org-image-target "file:fig-1.svg")"#, "fig-1.svg");

    // ...and the whole path, through Rust: `image-file` reads the SVG beside the
    // .org file, rasterises it with resvg and answers an id, and the proof is an
    // overlay in the editor carrying it.
    does(&shared, &lisp, "(org-inline-images)");
    let drawn = wait(&shared, "the figure overlay", |ed| {
        ed.buffer
            .overlays()
            .iter()
            .find(|o| o.image.is_some())
            .map(|o| (o.start, o.end, o.image.unwrap()))
    });
    assert!(drawn.2 > 0, "a figure overlay names a real image id: {drawn:?}");
    assert!(
        shared.lock().unwrap().image(drawn.2).is_some(),
        "and the editor is holding its pixels"
    );
    // The overlay covers the link text and nothing else, so the line's
    // indentation stays where it was.
    assert_eq!(
        shared.lock().unwrap().buffer.slice_string(drawn.0, drawn.1),
        "[[file:zemacs-test-fig.svg]]"
    );

    // Clearing takes its own overlays off and leaves everybody else's — the
    // `:org-image` mark doing the same job `:latex` and `:org-modern` do.
    does(&shared, &lisp, "(org-inline-images-clear)");
    wait(&shared, "the figure to clear", |ed| {
        ed.buffer.overlays().iter().all(|o| o.image.is_none()).then_some(())
    });
}
