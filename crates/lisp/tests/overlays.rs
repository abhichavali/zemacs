//! Headless proof of the overlay API, end to end through the real Lisp image.
//!
//! What is under test is the *bridge*: `zemacs_core::overlay` has its own unit
//! tests for position adjustment, and this is the other half — that a handle
//! survives the trip out to Lisp and back, that a property Rust draws reaches
//! the editor, that a property Rust has never heard of reaches nothing but the
//! image, and that an overlay moves with the text when Lisp is the one editing.
//!
//! Deliberately a single `#[test]`, as in every file beside it: `cl_boot`
//! initialises a process-wide Lisp image, so there is exactly one `spawn` per
//! test binary and a new file is the only way to add one.

use std::path::PathBuf;
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
    wait(shared, what, |ed| {
        ed.messages.iter().any(|m| pred(m)).then_some(())
    });
}

/// Evaluate `form` and wait for its value, printed with `~a`, to be `want`.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"~a\" {form}))"));
    wait_message(shared, form, |m| m == want);
}

/// A known buffer, in Normal mode. A rewrite rather than a second `load`, for
/// the same reason `api.rs` does it: loading an already-open path is a switch.
fn load(shared: &Shared, text: &str) {
    let mut ed = shared.lock().unwrap();
    if ed.buffer.path.is_none() {
        ed.load("", Some(PathBuf::from("/tmp/zemacs_overlay_test.org")), None);
    }
    ed.apply(EditorCommand::SetMode(Mode::Insert));
    let n = ed.buffer.len_chars();
    ed.apply(EditorCommand::DeleteRange(0, n));
    ed.apply(EditorCommand::InsertText(text.into()));
    ed.apply(EditorCommand::MoveTo(0));
    ed.apply(EditorCommand::SetMode(Mode::Normal));
}

/// The overlays the editor actually holds, as `(id, start, end)`.
///
/// Takes the `Editor` rather than the `Shared`: `wait` runs its predicate with
/// the lock held, and a second `lock()` in there is a deadlock, not a wait.
fn overlays(ed: &Editor) -> Vec<(u64, usize, usize)> {
    ed.buffer
        .overlays()
        .iter()
        .map(|o| (o.id, o.start, o.end))
        .collect()
}

/// How many overlays the live buffer has, once it has that many.
fn wait_overlays(shared: &Shared, n: usize) {
    wait(shared, "the overlay count", |ed| {
        (overlays(ed).len() == n).then_some(())
    });
}

#[test]
fn overlays_reach_the_editor_and_move_with_the_text() {
    let init = std::env::temp_dir().join("zemacs_test_overlay_init.lisp");
    std::fs::write(&init, "(in-package :zemacs)\n(message \"overlay test init loaded\")\n").unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    wait_message(&shared, "the init file", |m| m == "overlay test init loaded");

    load(&shared, "alpha beta gamma\n");

    // --- a handle comes back, and it names a range ------------------------
    lisp.eval("(defparameter *ov* (make-overlay 6 10))".into());
    wait_overlays(&shared, 1);
    assert_eq!(overlays(&shared.lock().unwrap())[0], (1, 6, 10));
    says(&shared, &lisp, "(overlay-position *ov*)", "(6 . 10)");
    says(&shared, &lisp, "(list (overlay-start *ov*) (overlay-end *ov*))", "(6 10)");
    // Overlap, not containment — and `overlays-at` is the same question about
    // one position.
    says(&shared, &lisp, "(length (overlays-in 0 7))", "1");
    says(&shared, &lisp, "(overlays-in 0 6)", "NIL");
    says(&shared, &lisp, "(length (overlays-at 6))", "1");
    says(&shared, &lisp, "(overlays-at 5)", "NIL");
    // An empty range is no overlay at all, and says so rather than handing back
    // a handle that names nothing.
    says(&shared, &lisp, "(make-overlay 4 4)", "NIL");

    // --- the properties Rust draws ---------------------------------------
    //
    // A colour is a *face name* from `face-list`, so an overlay follows the
    // theme instead of carrying an RGB of its own.
    lisp.eval("(overlay-put *ov* 'face \"keyword\")".into());
    lisp.eval("(overlay-put *ov* 'display \"BETA\")".into());
    wait(&shared, "the face and display to land", |ed| {
        let o = ed.buffer.overlays().first()?;
        (o.face == Some(zemacs_core::HlKind::Keyword) && o.display.as_deref() == Some("BETA"))
            .then_some(())
    });
    // ...and NIL takes one off again.
    lisp.eval("(overlay-put *ov* 'face nil)".into());
    wait(&shared, "the face to clear", |ed| {
        ed.buffer.overlays().first()?.face.is_none().then_some(())
    });

    // --- the display attributes -------------------------------------------
    //
    // Type size, weight and slant, plus the two that are about the *line* an
    // overlay touches rather than about the cells it covers. All of it through
    // the same `overlay-put`, which is the point of putting it there: the
    // vocabulary grew and the verb did not.
    lisp.eval("(overlay-put *ov* 'scale 1.5)".into());
    lisp.eval("(overlay-put *ov* 'weight 'bold)".into());
    lisp.eval("(overlay-put *ov* 'slant 'italic)".into());
    lisp.eval("(overlay-put *ov* 'line-background \"code\")".into());
    lisp.eval("(overlay-put *ov* 'line-prefix \"| \")".into());
    wait(&shared, "the display attributes to land", |ed| {
        let o = ed.buffer.overlays().first()?;
        (o.scale == Some(150)
            && o.bold == Some(true)
            && o.italic == Some(true)
            && o.line_background == Some(zemacs_core::HlKind::Code)
            && o.line_prefix.as_deref() == Some("| "))
        .then_some(())
    });
    // A multiplier crosses the boundary as a percentage, because the write
    // envelope carries integers — and `overlay-get` still answers the float that
    // was put, because the plist is the image's and never went anywhere.
    says(&shared, &lisp, "(overlay-get *ov* 'scale)", "1.5");
    // `normal` is a claim and NIL is the absence of one, which is the whole
    // reason weight is three-valued: the first draws upright over an overlay
    // underneath, the second leaves whatever that overlay said alone.
    lisp.eval("(overlay-put *ov* 'weight 'normal)".into());
    wait(&shared, "an explicit upright weight", |ed| {
        (ed.buffer.overlays().first()?.bold == Some(false)).then_some(())
    });
    lisp.eval("(overlay-put *ov* 'weight nil)".into());
    lisp.eval("(overlay-put *ov* 'line-prefix nil)".into());
    wait(&shared, "the weight and prefix to clear", |ed| {
        let o = ed.buffer.overlays().first()?;
        (o.bold.is_none() && o.line_prefix.is_none()).then_some(())
    });
    // Body size is not a size, however it is spelled — so a mode may set the
    // scale of every heading level unconditionally and level 1 costs nothing.
    lisp.eval("(overlay-put *ov* 'scale 1)".into());
    wait(&shared, "a body-size scale to fold away", |ed| {
        ed.buffer.overlays().first()?.scale.is_none().then_some(())
    });

    // --- the properties only Lisp knows about ----------------------------
    //
    // The whole reason the plist lives in the image: none of this could survive
    // a trip through C as printed source, and none of it has to.
    lisp.eval("(overlay-put *ov* 'target (lambda () 42))".into());
    lisp.eval("(overlay-put *ov* 'diagnostic '(:severity :warning :code 7))".into());
    says(&shared, &lisp, "(funcall (overlay-get *ov* 'target))", "42");
    says(&shared, &lisp, "(getf (overlay-get *ov* 'diagnostic) :code)", "7");
    // Spelled either way, and from any package: the key is a keyword.
    says(&shared, &lisp, "(overlay-get *ov* :display)", "BETA");
    says(&shared, &lisp, "(overlay-get *ov* 'no-such-property)", "NIL");

    // --- it moves with the text ------------------------------------------
    //
    // The point of the exercise. Emacs' default insertion types: text typed at
    // the front lands inside, text typed at the back lands outside.
    lisp.eval("(insert-at 0 \"XX\")".into());
    says(&shared, &lisp, "(overlay-position *ov*)", "(8 . 12)");
    lisp.eval("(insert-at 8 \"..\")".into());
    says(&shared, &lisp, "(overlay-position *ov*)", "(8 . 14)");
    lisp.eval("(insert-at 14 \"!\")".into());
    says(&shared, &lisp, "(overlay-position *ov*)", "(8 . 14)");
    // A deletion straddling one end clips it to what survived...
    lisp.eval("(delete-region 12 20)".into());
    says(&shared, &lisp, "(overlay-position *ov*)", "(8 . 12)");
    // ...and one that swallows it takes it with it, rather than leaving a
    // zero-width overlay whose `display` string would still be drawn.
    lisp.eval("(delete-region 0 12)".into());
    says(&shared, &lisp, "(overlay-position *ov*)", "NIL");
    wait_overlays(&shared, 0);

    // --- deleting, and removing a range ----------------------------------
    load(&shared, "one two three four\n");
    lisp.eval("(defparameter *a* (make-overlay 0 3))".into());
    lisp.eval("(defparameter *b* (make-overlay 4 7))".into());
    lisp.eval("(defparameter *c* (make-overlay 8 13))".into());
    wait_overlays(&shared, 3);
    lisp.eval("(delete-overlay *b*)".into());
    wait_overlays(&shared, 2);
    // The plist goes with it, which is the half Rust cannot do.
    says(&shared, &lisp, "(overlay-get *b* 'anything)", "NIL");
    // `remove-overlays` is Emacs' blunt instrument: everything that *overlaps*.
    lisp.eval("(remove-overlays 2 9)".into());
    wait_overlays(&shared, 0);
    // ...and here is the leak the shim documents, in the flesh: `*ov*` above was
    // deleted by the *editor*, when an edit swallowed the text it was about, so
    // nothing ever pruned its plist. One entry, and the recipe for the rest is
    // the one in the shim's own comment.
    says(&shared, &lisp, "(hash-table-count *overlay-properties*)", "1");
    lisp.eval(
        "(maphash (lambda (id p) (declare (ignore p))
                    (unless (overlay-position id)
                      (remhash id *overlay-properties*)))
                  *overlay-properties*)"
            .into(),
    );
    says(&shared, &lisp, "(hash-table-count *overlay-properties*)", "0");

    // --- the LaTeX fragment reader ---------------------------------------
    //
    // The one reader answered outside `query.rs`, and what `org-latex-preview`
    // walks. No TeX is run here: rendering wants a toolchain a CI box need not
    // have, and the pipeline has its own tests in `crates/latex`.
    load(&shared, "Euler said $e^{i\\pi}+1=0$ and also\n\\[ a^2+b^2=c^2 \\]\n");
    says(&shared, &lisp, "(length (latex-fragments))", "2");
    says(&shared, &lisp, "(third (first (latex-fragments)))", "NIL"); // inline
    says(&shared, &lisp, "(third (second (latex-fragments)))", "T"); // display
    says(
        &shared,
        &lisp,
        "(let ((f (first (latex-fragments)))) (buffer-substring (first f) (second f)))",
        "$e^{i\\pi}+1=0$",
    );
    // A reader is a noun rather than a command, so it stays out of the M-x list
    // the same way every other one does.
    says(&shared, &lisp, "(if (member \"latex-fragments\" *readers* :test #'string=) t nil)", "T");
}
