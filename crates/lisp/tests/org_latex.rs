//! Headless proof that a preview gets out of the way of the cursor.
//!
//! The bug this pins down is the one that made editing an equation feel like the
//! editor had stopped listening. An overlay carrying an image *substitutes* for
//! the characters under it — `zemacs_core::display` gives them no cell of their
//! own — so with a preview still hanging on `$x^2$`, every character typed
//! inside it landed in the buffer and appeared nowhere on screen.
//!
//! `org-latex-preview-new` already declined to *re-render* the fragment point
//! was inside, which is why the bug looked like it was about typing rather than
//! about moving: nothing ever took the existing image *off*.
//!
//! No `latex` binary is involved. The overlay is made by hand with the `:latex`
//! mark the module puts on its own, which is exactly what the reveal keys off —
//! so this runs the real logic on a machine with no TeX, which is most of them.
//!
//! Deliberately a single `#[test]`, as in every file beside it: `cl_boot`
//! initialises a process-wide Lisp image, so there is one `spawn` per test
//! binary and a new file is the only way to add another.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Mode, Shared};

const PATIENCE: Duration = Duration::from_secs(20);

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
    wait(shared, what, |ed| ed.messages.iter().any(|m| pred(m)).then_some(()));
}

/// Evaluate `form` and wait for its value, printed with `~a`, to be `want`.
///
/// `tag` is not decoration. `messages` *accumulates*, and every assertion below
/// is about a count or a NIL — so an untagged `says` would match the identical
/// message some earlier step already left in the list and pass without the form
/// ever being evaluated. One unique tag per call site is what makes each of
/// these a question about the editor as it stands now.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, tag: &str, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"{tag}=~a\" {form}))"));
    let want = format!("{tag}={want}");
    wait_message(shared, &format!("{form} => {want}"), |m| m == want);
}

fn runtime(file: &str) -> String {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime").join(file))
        .unwrap_or_else(|e| panic!("runtime/{file} must exist: {e}"))
        .display()
        .to_string()
}

#[test]
fn a_preview_gets_out_of_the_way_of_the_cursor_and_comes_back_after() {
    let init = std::env::temp_dir()
        .join(format!("zemacs_test_org_latex_init-{}.lisp", std::process::id()));
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(load {:?} :verbose nil :print nil)
(message "org-latex test init loaded")
"#,
            runtime("modes/modes.lisp"),
            runtime("modes/org-latex.lisp"),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    wait_message(&shared, "the init file", |m| m == "org-latex test init loaded");

    // `$x^2$` is chars 7..12 on line 1, with text either side of it so that
    // "point is outside the fragment" has somewhere on the same line to mean.
    {
        let mut ed = shared.lock().unwrap();
        ed.load("", Some(PathBuf::from("/tmp/zemacs_org_latex_test.org")), None);
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::InsertText("before\n$x^2$ tail\nafter\n".into()));
        ed.apply(EditorCommand::SetMode(Mode::Normal));
    }
    lisp.eval("(org-mode)".into());
    wait(&shared, "org-mode", |ed| (ed.buffer.major_mode == "org-mode").then_some(()));

    // A preview, as `%org-latex-draw` would have left one: the `:latex` mark is
    // what says "this overlay is the module's own", and it is the whole of what
    // the reveal looks for.
    lisp.eval(
        "(defparameter *ov* (make-overlay 7 12))\
         (overlay-put *ov* :latex t)\
         (overlay-put *ov* 'display \"[x2]\")"
            .into(),
    );
    says(&shared, &lisp, "made", "(length (org-latex-previews (point-min) (point-max)))", "1");

    // --- point outside it: the preview stays -------------------------------
    //
    // The hook runs on *every* movement, so the case that must not fire is the
    // one worth pinning first — a reveal that triggered from anywhere would
    // simply un-preview the buffer as you scrolled through it.
    lisp.eval("(goto-char 0)".into());
    lisp.eval("(org-latex-maybe-preview)".into());
    says(&shared, &lisp, "outside-count", "(length (org-latex-previews (point-min) (point-max)))", "1");
    says(&shared, &lisp, "outside-armed", "*org-latex-edited-line*", "NIL");
    says(&shared, &lisp, "outside-walk", "*org-latex-revealed*", "NIL");

    // ...including just past its closing `$', which is a position `overlays-in'
    // does not call an overlap and the fragment reader deliberately does.
    lisp.eval("(goto-char 13)".into());
    lisp.eval("(org-latex-maybe-preview)".into());
    says(&shared, &lisp, "past-close", "(length (org-latex-previews (point-min) (point-max)))", "1");

    // --- point inside it: the source comes back ----------------------------
    lisp.eval("(goto-char 9)".into());
    lisp.eval("(org-latex-maybe-preview)".into());
    says(&shared, &lisp, "inside-gone", "(org-latex-previews (point-min) (point-max))", "NIL");
    // ...and *where it was* is remembered, which is how the image comes back
    // rather than the equation staying spelled out for the rest of the session.
    //
    // The range and not the line, and that distinction is the whole of the
    // second bug this file now pins. Arming the *line* meant `$a$ and $b$` —
    // two equations, one line — left the first one spelled out while you walked
    // over to the second, because the line test says point never left. Which is
    // exactly the "previews sometimes" it looked like from the outside.
    says(&shared, &lisp, "inside-armed", "*org-latex-revealed*", "(7 . 12)");
    // Still inside, so nothing is owed yet.
    says(&shared, &lisp, "inside-walked", "(%org-latex-walked-off-p)", "NIL");

    // --- off the fragment but *on the same line*: the render is owed --------
    //
    // ` tail` is chars 12..17 of the buffer, on the same line as the equation.
    // Before the range was remembered this position looked identical to being
    // inside it, and nothing was ever re-typeset until you pressed `j`.
    lisp.eval("(goto-char 15)".into());
    says(&shared, &lisp, "same-line-walked", "(%org-latex-walked-off-p)", "T");
    lisp.eval("(org-latex-maybe-preview)".into());
    says(&shared, &lisp, "same-line-cleared", "*org-latex-revealed*", "NIL");

    // --- ...and an edit takes the range back down --------------------------
    //
    // Typing inside a revealed fragment moves its end, so the offsets stop
    // meaning anything and the *line* flag owns the case again. Without this,
    // one character past the remembered end reads as "walked out" and re-runs
    // LaTeX in the middle of a word.
    lisp.eval(
        "(defparameter *ov* (make-overlay 7 12))\
         (overlay-put *ov* :latex t)\
         (goto-char 9)\
         (org-latex-reveal-at-point)"
            .into(),
    );
    says(&shared, &lisp, "edit-armed", "*org-latex-revealed*", "(7 . 12)");
    lisp.eval("(org-latex-note-change)".into());
    says(&shared, &lisp, "edit-dropped", "*org-latex-revealed*", "NIL");
    says(&shared, &lisp, "edit-line", "*org-latex-edited-line*", "2");

    // --- the closing delimiter counts as inside ----------------------------
    //
    // `overlays-in' does not count touching at a boundary as overlapping, so
    // without the one-character slack the reveal missed the very position you
    // are left on by typing the closing `$' — the keystroke that finishes an
    // equation is the one that would have hidden it under its own image.
    lisp.eval(
        "(defparameter *ov* (make-overlay 7 12))\
         (overlay-put *ov* :latex t)\
         (setf *org-latex-edited-line* nil)"
            .into(),
    );
    says(&shared, &lisp, "remade", "(length (org-latex-previews (point-min) (point-max)))", "1");
    lisp.eval("(goto-char 12)".into());
    lisp.eval("(org-latex-maybe-preview)".into());
    says(&shared, &lisp, "on-close-gone", "(org-latex-previews (point-min) (point-max))", "NIL");

    // --- and it is org-mode's rule, not the editor's -----------------------
    //
    // The reveal costs an `overlays-in' per movement, so it must not be running
    // in every buffer in the editor. Same overlay, same position, a mode that is
    // not org: the preview stays exactly where it is.
    lisp.eval(
        "(defparameter *ov* (make-overlay 7 12))\
         (overlay-put *ov* :latex t)\
         (text-mode)\
         (goto-char 9)\
         (org-latex-maybe-preview)"
            .into(),
    );
    says(&shared, &lisp, "not-org", "(length (org-latex-previews (point-min) (point-max)))", "1");

    // --- an inline preview is not a fold ------------------------------------
    //
    // `%org-latex-draw` sets `:fold` so the blank rows a multi-line
    // `\begin{align}` leaves behind stop existing. It used to set it on every
    // preview, on the argument that a one-line fragment has no line after its
    // first and so nothing to fold — which is true and was not the whole story:
    // a fold is what the renderer draws its `…` for, so every line holding an
    // inline `$x^2$` grew an ellipsis on the end claiming there was more text
    // behind it.
    //
    // No TeX on the machine, so `latex-preview` is stood in for: it is an
    // ordinary Lisp function now that it takes a size, and the rule under test
    // is about the *source*, not the pixels.
    //
    // The memo goes with it: on a machine with no TeX every step above has been
    // a real `latex-preview` call that failed, and `*org-latex-failed*` now
    // holds `$x^2$` so that it is never asked twice. Forgetting it here is what
    // `org-latex-preview` does for the same reason — the answer has changed.
    lisp.eval(
        "(mapc #'delete-overlay (org-latex-previews (point-min) (point-max)))\
         (setf *org-latex-failed* nil)\
         (defun latex-preview (source &optional scale)\
           (declare (ignore source scale)) 4242)"
            .into(),
    );
    // `$x^2$` is chars 7..12, all on one line.
    lisp.eval("(%org-latex-draw 7 12)".into());
    says(&shared, &lisp, "fold-inline",
         "(overlay-get (first (org-latex-previews 7 12)) :fold)", "NIL");
    // ...and a range spanning a newline is the case the flag exists for.
    lisp.eval("(mapc #'delete-overlay (org-latex-previews (point-min) (point-max)))\
               (%org-latex-draw 0 18)".into());
    says(&shared, &lisp, "fold-display",
         "(and (overlay-get (first (org-latex-previews 0 18)) :fold) t)", "T");
    lisp.eval("(mapc #'delete-overlay (org-latex-previews (point-min) (point-max)))".into());

    // --- how big to set it -------------------------------------------------
    //
    // A fragment is typeset at the size of the text around it, which is two
    // independent questions: what line it is on, and whether it is display math.
    // `%org-latex-scale` is a pure function of the buffer text and an offset, so
    // it is asked directly rather than through a render — there is no `latex` on
    // most machines and the answer here is policy, not pixels.
    //
    // org-modern is not loaded in this init, so the heading list is unbound and
    // every heading answers body size. That is the fallback being tested as much
    // as the arithmetic: a config that switched org-modern off must still get
    // equations, not an `unbound-variable` per fragment.
    // Real newlines, not `\n` — a backslash in a Lisp string escapes the next
    // character literally, so `"a\nb"` is `anb` and every offset below would be
    // on one very long heading.
    let text = "\"* head $a$\nbody $b$\n\\\\[ c \\\\]\n**bold** $d$\n\"";
    says(&shared, &lisp, "scale-nomodern", &format!("(%org-latex-scale {text} 8 nil)"), "1");
    // With the list bound, a level-1 heading's equation is the heading's size.
    lisp.eval("(defparameter *org-modern-heading-scale* '(1.5 1.25))".into());
    says(&shared, &lisp, "scale-h1", &format!("(%org-latex-scale {text} 8 nil)"), "1.5");
    // ...and one in the body is not.
    says(&shared, &lisp, "scale-body", &format!("(%org-latex-scale {text} 18 nil)"), "1");
    // Display math gets its own line and is set larger for it.
    says(&shared, &lisp, "scale-display",
         &format!("(%org-latex-scale {text} 18 t)"), "1.25");
    // `**bold**` at the start of a line is emphasis, not a level-2 heading:
    // stars only make a heading when a space follows them. Without that test
    // every equation in a paragraph opening with emphasis came out oversized.
    says(&shared, &lisp, "scale-emphasis",
         &format!("(%org-latex-scale {text} 38 nil)"), "1");

    // --- the fragment point is sitting in is *owed* a render ----------------
    //
    // `org-latex-preview-new` declines to typeset the fragment point is inside,
    // and for a long time declining was all it did. Nothing was left saying the
    // fragment was owed anything, so walking back out of it changed no line and
    // revealed no overlay — neither trigger in `org-latex-maybe-preview` fired
    // again and the equation stayed spelled out for the rest of the session.
    //
    // Which is the state you are in the moment you open a file with point in an
    // equation, and again every time you edit inside a `\begin{align}` and step
    // down a line still inside it.
    lisp.eval("(org-mode)".into());
    wait(&shared, "org-mode again", |ed| (ed.buffer.major_mode == "org-mode").then_some(()));
    lisp.eval(
        "(setf *org-latex-auto* t *org-latex-edited-line* nil *org-latex-revealed* nil)\
         (goto-char 9)\
         (org-latex-preview-new)"
            .into(),
    );
    says(&shared, &lisp, "at-point-undrawn", "(org-latex-previews 7 12)", "NIL");
    says(&shared, &lisp, "at-point-armed", "*org-latex-revealed*", "(7 . 12)");
    // ...and walking off it draws it, through the ordinary hook and nothing else.
    lisp.eval("(goto-char 15)(org-latex-maybe-preview)".into());
    says(&shared, &lisp, "at-point-drawn", "(length (org-latex-previews 7 12))", "1");

    // --- ...and `C-c R' keeps meaning "show me the source" ------------------
    //
    // Both flags mean "something is owed a render", and the moment you clear is
    // exactly when one of them is set: you are inside the equation you walked
    // into, or on the line you were typing on. Without the disarm the next
    // cursor movement drew the whole buffer again and the command looked as
    // though it had never run.
    lisp.eval(
        "(setf *org-latex-edited-line* 2 *org-latex-revealed* '(7 . 12))\
         (org-latex-preview-clear)"
            .into(),
    );
    says(&shared, &lisp, "clear-line", "*org-latex-edited-line*", "NIL");
    says(&shared, &lisp, "clear-walk", "*org-latex-revealed*", "NIL");

    // --- one equation LaTeX refuses is not the document's problem -----------
    //
    // `$ok$ and $bad$`: chars 0..4 and 9..14, point on the `n` between them.
    // The bad one is *second* on purpose — the render walks back to front, so it
    // is reached first, and it used to end the pass there and switch
    // `*org-latex-auto*` off. One typo therefore left every equation above it
    // spelled out, and said so with a message blaming the machine.
    {
        let mut ed = shared.lock().unwrap();
        ed.load("", Some(PathBuf::from("/tmp/zemacs_org_latex_refused.org")), None);
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::InsertText("$ok$ and $bad$\n".into()));
        ed.apply(EditorCommand::SetMode(Mode::Normal));
    }
    lisp.eval("(org-mode)".into());
    wait(&shared, "org-mode once more", |ed| (ed.buffer.major_mode == "org-mode").then_some(()));
    // A stand-in that refuses one source and counts every time it is asked —
    // the count is the whole of "and never asked twice".
    lisp.eval(
        "(defparameter *tries* 0)\
         (defun latex-preview (source &optional scale)\
           (declare (ignore scale))\
           (incf *tries*)\
           (unless (search \"bad\" source) 4242))\
         (setf *org-latex-failed* nil *org-latex-auto* t)\
         (setf *org-latex-edited-line* nil *org-latex-revealed* nil)\
         (goto-char 6)\
         (org-latex-preview-new)"
            .into(),
    );
    says(&shared, &lisp, "refused-drew-rest",
         "(length (org-latex-previews (point-min) (point-max)))", "1");
    says(&shared, &lisp, "refused-still-auto", "*org-latex-auto*", "T");
    says(&shared, &lisp, "refused-remembered", "(length *org-latex-failed*)", "1");
    // Twice through, once asked: the drawn one is skipped for having an overlay
    // and the refused one for being in the memo, so the second pass runs no
    // LaTeX at all. Without the memo this is where a bad fragment starts
    // costing a latex run every time you leave a line.
    lisp.eval("(org-latex-preview-new)".into());
    says(&shared, &lisp, "refused-not-retried", "*tries*", "2");

    // Asking by hand forgets the refusals, because that is what asking by hand
    // means — the typo you corrected and the TeX you installed are both `try it
    // again'. Both fragments go back through, so the count is four.
    lisp.eval("(goto-char 6)(org-latex-preview)".into());
    says(&shared, &lisp, "byhand-retries", "*tries*", "4");
    says(&shared, &lisp, "byhand-auto", "*org-latex-auto*", "T");

    // ...and a buffer with no equations in it has told you nothing about your
    // machine, so it must not switch anything off. `latex-fragments` is stood
    // in for last of all, since nothing after it would find anything.
    lisp.eval("(defun latex-fragments () nil)(setf *org-latex-auto* t)(org-latex-preview)".into());
    says(&shared, &lisp, "empty-auto", "*org-latex-auto*", "T");
}
