//! Code folding, end to end — and the point of the file is *which* end.
//!
//! Rust owns one fact and one only: an overlay carrying `fold` makes the lines
//! after its first stop occupying rows, so the renderer skips them and `j` steps
//! over them. Everything about what a *subtree* is — that a headline is stars at
//! column 0 followed by a space, that it runs until the next headline of the
//! same or shallower level — is Lisp, in `runtime/modes/org-fold.lisp`, and can
//! be replaced by a config without a rebuild.
//!
//! So this drives the shipped `init.lisp` and asserts on core's state: five
//! links, and a unit test of any one of them would have passed while the feature
//! was dead.
//!
//! 1. Lisp works out the range (`org-subtree-at-point`, pure Lisp over readers);
//! 2. `(fold-region beg end)` makes an overlay and `(overlay-put ov 'fold t)`;
//! 3. that is `%do "overlay-fold"`, one arm of `command_for`;
//! 4. core's `fold_hiding` then hides the lines — which the renderer reads to
//!    skip a row, and `step_line` reads to skip a `j`;
//! 5. and `fold-dwim` a second time takes it all back out.
//!
//! Deliberately a single `#[test]`, like every other file here — `cl_boot`
//! initialises a process-wide image, so there is one `spawn` per test binary.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zemacs_core::{fold_hiding, Buffer, Editor, Key, Mode, Shared};

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

/// How many folds the live buffer has. The whole observable effect of the Lisp
/// half, read the way the renderer reads it.
fn folds(ed: &Editor) -> usize {
    ed.buffer.overlays().iter().filter(|o| o.fold).count()
}

/// Buffer lines the renderer would skip, by line index.
fn hidden(ed: &Editor) -> Vec<usize> {
    (0..ed.buffer.len_lines())
        .filter(|&l| fold_hiding(ed.buffer.overlays(), ed.buffer.line_start(l)).is_some())
        .collect()
}

#[test]
fn an_org_subtree_folds_from_lisp_and_stops_occupying_rows() {
    let entry = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp"),
    )
    .expect("runtime/init.lisp must exist");

    let init = std::env::temp_dir().join("zemacs_test_fold_init.lisp");
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(message "fold test init loaded")
"#,
            entry.display().to_string()
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = Arc::new(zemacs_lisp::spawn(tx, shared.clone(), init));

    wait(&shared, "org-fold.lisp to load", |ed| {
        ed.messages
            .iter()
            .any(|m| m == "fold test init loaded")
            .then_some(())
    });

    // An org buffer with two top-level subtrees, one of them nested. Lines:
    //   0 "* one"  1 "body"  2 "** deep"  3 "under"  4 "* two"  5 "tail"
    {
        let mut ed = shared.lock().unwrap();
        ed.mode = Mode::Normal;
        ed.buffer = Buffer::from_str("* one\nbody\n** deep\nunder\n* two\ntail");
        ed.buffer.major_mode = "org-mode".into();
        ed.buffer.cursor = 0;
        ed.status.clear();
    }

    // --- fold the subtree under point ---------------------------------------
    lisp.eval("(zemacs::fold-dwim)".into());
    wait(&shared, "the subtree under point to fold", |ed| {
        (folds(ed) == 1).then_some(())
    });
    {
        let ed = shared.lock().unwrap();
        // The headline stays: it is the thing you fold *from*, and hiding it
        // would leave nothing to unfold with. Its subtree runs to the next
        // headline of the same or shallower level, so `** deep` goes with it and
        // `* two` does not — which is org's rule, decided in Lisp.
        assert_eq!(hidden(&ed), vec![1, 2, 3], "one subtree, not the next");
    }

    // --- and `j` steps over what the renderer skipped ------------------------
    {
        let mut ed = shared.lock().unwrap();
        for cmd in ed.handle_key(Key::Char('j')) {
            ed.apply(cmd);
        }
        assert_eq!(
            ed.buffer.cursor_line_col(),
            (4, 0),
            "j lands on the next drawn line, not inside the fold"
        );
        // ...and back, over three hidden lines in one press.
        for cmd in ed.handle_key(Key::Char('k')) {
            ed.apply(cmd);
        }
        assert_eq!(ed.buffer.cursor_line_col(), (0, 0));
    }

    // --- the same key opens it again ----------------------------------------
    lisp.eval("(zemacs::fold-dwim)".into());
    wait(&shared, "the subtree to open", |ed| (folds(ed) == 0).then_some(()));
    assert!(hidden(&shared.lock().unwrap()).is_empty());

    // --- fold all, and open all ---------------------------------------------
    lisp.eval("(zemacs::fold-all)".into());
    wait(&shared, "both top-level subtrees to fold", |ed| {
        (folds(ed) == 2).then_some(())
    });
    {
        let ed = shared.lock().unwrap();
        // Only the two headlines are left drawn — org's `overview'.
        assert_eq!(hidden(&ed), vec![1, 2, 3, 5]);
    }
    lisp.eval("(zemacs::fold-open-all)".into());
    wait(&shared, "every fold to open", |ed| (folds(ed) == 0).then_some(()));

    // --- a buffer whose mode has no policy says so rather than guessing -------
    //
    // Which is the boundary working: Rust has no opinion about what is foldable,
    // so a mode nobody taught folds nothing, and teaching it is one entry in
    // `*fold-subtree-functions*`.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.major_mode = "rust-mode".into();
        ed.status.clear();
    }
    lisp.eval("(zemacs::fold-dwim)".into());
    let status = wait(&shared, "fold-dwim to decline", |ed| {
        (!ed.status.is_empty()).then(|| ed.status.clone())
    });
    assert!(status.contains("nothing foldable"), "got {status:?}");
    assert_eq!(folds(&shared.lock().unwrap()), 0);
}
