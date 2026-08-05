//! `runtime/modes/show-paren.lisp`, end to end.
//!
//! The feature is Lisp over one primitive, so the thing worth proving is the
//! seam: `matching-bracket` crosses the shim as an integer or NIL, the mover
//! hangs off `*point-moved-functions*`, and what comes out the far end is two
//! overlays on the two characters of the pair — the ones the renderer will
//! draw. Asserting on the editor's own overlay list rather than on pixels is
//! the same line every other overlay test here draws.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Mode, Shared};

const PATIENCE: Duration = Duration::from_secs(20);

fn wait<T>(shared: &Shared, what: &str, f: impl Fn(&Editor) -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(v) = f(&shared.lock().unwrap()) {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_message(shared: &Shared, what: &str, pred: impl Fn(&str) -> bool) {
    wait(shared, what, |ed| {
        ed.messages.iter().any(|m| pred(m)).then_some(())
    });
}

fn runtime(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .expect("the runtime directory ships with the source")
}

/// A known buffer, in Normal mode, off the dashboard — which is generated and
/// therefore read-only, so `apply` refuses every edit aimed at it. Same shape
/// as `overlays.rs`, and for the same reason.
fn load(shared: &Shared, text: &str) {
    let mut ed = shared.lock().unwrap();
    if ed.buffer.path.is_none() {
        ed.load("", Some(std::path::PathBuf::from("/tmp/zemacs_show_paren.rs")), None);
    }
    ed.apply(EditorCommand::SetMode(Mode::Insert));
    let n = ed.buffer.len_chars();
    ed.apply(EditorCommand::DeleteRange(0, n));
    ed.apply(EditorCommand::InsertText(text.into()));
    ed.apply(EditorCommand::MoveTo(0));
    ed.apply(EditorCommand::SetMode(Mode::Normal));
}

/// Every overlay on the live buffer, as `(start, end)`, in creation order.
fn spans(ed: &Editor) -> Vec<(usize, usize)> {
    ed.buffer.overlays().iter().map(|o| (o.start, o.end)).collect()
}

/// Put point at `at` and run the mover the way the application does.
fn point_at(shared: &Shared, lisp: &zemacs_lisp::Lisp, at: usize) {
    shared.lock().unwrap().apply(EditorCommand::MoveTo(at));
    lisp.eval(
        "(let ((h (find-symbol \"POINT-MOVED-HOOK\" :zemacs))) \
           (when (and h (fboundp h)) (funcall h)))"
            .into(),
    );
}

/// Wait until the highlight *is* `want`.
///
/// On the spans and not on their count, which is what a first draft did and
/// what made this test pass by accident: moving from one pair to another leaves
/// the count at two throughout, so a count-watcher returns the *previous*
/// pair before the mover has run.
fn expect(shared: &Shared, want: &[(usize, usize)]) {
    wait(shared, &format!("the highlight to be {want:?}"), |ed| {
        let mut s = spans(ed);
        s.sort();
        (s == want).then_some(())
    });
}

#[test]
fn the_bracket_at_point_and_its_partner_are_both_highlighted() {
    let init = std::env::temp_dir().join("zemacs_test_show_paren_init.lisp");
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             ;; `modes.lisp' declares `point-moved-hook'; this file hangs off it.\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (message \"show-paren test init loaded\")\n",
            runtime("modes/modes.lisp"),
            runtime("modes/show-paren.lisp"),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init.clone());
    wait_message(&shared, "the init file", |m| {
        m == "show-paren test init loaded"
    });

    load(&shared, "f(a, g(b), c)");
    // f(a, g(b), c)
    // 0123456789...

    // On the opening paren: it and its partner, both one character wide.
    point_at(&shared, &lisp, 1);
    expect(&shared, &[(1, 2), (12, 13)]);

    // On the closing paren, the same pair — the highlight does not care which
    // end you approached it from.
    point_at(&shared, &lisp, 12);
    expect(&shared, &[(1, 2), (12, 13)]);

    // The inner pair, from its opener. Nesting is counted, so this is `g(b)`
    // and not `g(b), c)`.
    point_at(&shared, &lisp, 6);
    expect(&shared, &[(6, 7), (8, 9)]);

    // *Just past* a closing paren, which is where point sits the instant you
    // finish typing one — the case the whole feature exists for.
    point_at(&shared, &lisp, 9);
    expect(&shared, &[(6, 7), (8, 9)]);

    // Nowhere near a bracket: the last pair comes off rather than lingering.
    point_at(&shared, &lisp, 3);
    expect(&shared, &[]);

    // Unbalanced text answers NIL rather than guessing, and draws nothing.
    load(&shared, "(a");
    point_at(&shared, &lisp, 0);
    expect(&shared, &[]);

    let _ = std::fs::remove_file(&init);
}
