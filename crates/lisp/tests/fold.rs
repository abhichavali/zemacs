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

    let init = std::env::temp_dir()
        .join(format!("zemacs_test_fold_init-{}.lisp", std::process::id()));
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

    // --- org's own three-state cycle ----------------------------------------
    //
    // `fold-dwim` above is the generic two-state toggle every mode gets. This is
    // the org-only one, and the whole reason it needs a test is that its three
    // states are told apart by *reading the buffer back* — so a state that is
    // misread does not fail loudly, it just refuses to advance. Both bugs this
    // caught were of exactly that kind.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.major_mode = "org-mode".into();
        ed.buffer.cursor = 0; // on `* one`
        ed.status.clear();
    }

    // SUBTREE -> FOLDED: everything under the headline goes.
    lisp.eval("(zemacs::org-cycle)".into());
    wait(&shared, "org-cycle to fold", |ed| {
        (hidden(ed) == vec![1, 2, 3]).then_some(())
    });

    // FOLDED -> CHILDREN: `** deep` comes back as a headline, its body does not,
    // and neither does the parent's own body. This is the state that does not
    // exist without the cycle, and asserting on `hidden` rather than on the fold
    // count is the point — several folds spell it, and how many is not the
    // contract.
    lisp.eval("(zemacs::org-cycle)".into());
    wait(&shared, "org-cycle to show children", |ed| {
        (hidden(ed) == vec![1, 3]).then_some(())
    });

    // CHILDREN -> SUBTREE. The regression: every child headline is the *start*
    // of its own fold here, so a state test built on `folded-p` reads CHILDREN
    // as FOLDED and this press loops back instead of opening.
    lisp.eval("(zemacs::org-cycle)".into());
    wait(&shared, "org-cycle to open", |ed| {
        (folds(ed) == 0 && hidden(ed).is_empty()).then_some(())
    });

    // A leaf headline has no middle state and cycles in two. `* two` owns only
    // `tail`; the second press used to be a type error on `(first NIL)`.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.cursor = ed.buffer.line_start(4);
        ed.status.clear();
    }
    lisp.eval("(zemacs::org-cycle)".into());
    wait(&shared, "the leaf subtree to fold", |ed| {
        (hidden(ed) == vec![5]).then_some(())
    });
    lisp.eval("(zemacs::org-cycle)".into());
    wait(&shared, "the leaf subtree to open again", |ed| {
        (folds(ed) == 0).then_some(())
    });

    // --- a heading with nothing under it is not foldable ---------------------
    //
    // Because a fold's *first* line stays drawn: a range that stops before the
    // next line's start hides no rows at all. It is still an overlay, though, and
    // the renderer marks every line a fold begins on — so both of these used to
    // answer TAB by hanging a `…` on your heading, promising text that was not
    // there and taking three presses to walk a cycle nothing moved in.
    //
    //   0 "* one"  1 ""  2 "* two"
    //
    // `* one` owns only the blank separator line, and `* two` is the heading you
    // have just typed at the end of the file.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer = Buffer::from_str("* one\n\n* two");
        ed.buffer.major_mode = "org-mode".into();
        ed.buffer.cursor = 0;
        ed.status.clear();
    }
    lisp.eval("(zemacs::org-cycle)".into());
    let status = wait(&shared, "org-cycle to decline a blank body", |ed| {
        (!ed.status.is_empty()).then(|| ed.status.clone())
    });
    assert!(status.contains("nothing foldable"), "got {status:?}");
    assert_eq!(folds(&shared.lock().unwrap()), 0, "no overlay, so no marker");
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.cursor = ed.buffer.line_start(2);
        ed.status.clear();
    }
    lisp.eval("(zemacs::org-cycle)".into());
    let status = wait(&shared, "org-cycle to decline the last heading", |ed| {
        (!ed.status.is_empty()).then(|| ed.status.clone())
    });
    assert!(status.contains("nothing foldable"), "got {status:?}");
    assert_eq!(folds(&shared.lock().unwrap()), 0);

    // --- CHILDREN of bare headlines is one fold, not three -------------------
    //
    // The same rule one level down, and the state it is most visible in: a list
    // of `** TODO` lines with nothing under them has nothing to hide, so the
    // middle state is the body fold alone. Asserting the *count* here rather
    // than only `hidden`, because the bug this catches hides nothing by
    // definition — it is two extra overlays, each drawing a `…` on a child that
    // has no body.
    //
    //   0 "* one"  1 "body"  2 "** a"  3 "** b"
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer = Buffer::from_str("* one\nbody\n** a\n** b\n");
        ed.buffer.major_mode = "org-mode".into();
        ed.buffer.cursor = 0;
        ed.status.clear();
    }
    lisp.eval("(zemacs::org-cycle)".into());
    wait(&shared, "the whole subtree to fold", |ed| {
        (hidden(ed) == vec![1, 2, 3]).then_some(())
    });
    lisp.eval("(zemacs::org-cycle)".into());
    wait(&shared, "the children to come back", |ed| {
        (hidden(ed) == vec![1] && folds(ed) == 1).then_some(())
    });
    // ...and the cycle still closes, which is the half a state test can lose:
    // with no child folds left to read, CHILDREN is told from FOLDED by the body
    // fold's extent alone.
    lisp.eval("(zemacs::org-cycle)".into());
    wait(&shared, "the subtree to open from CHILDREN", |ed| {
        (folds(ed) == 0).then_some(())
    });

    // --- blocks and drawers, org's other two foldable things ------------------
    //
    // Both have the shape a fold already wants — an opening line that stays drawn
    // and a run under it that goes — and neither is a subtree, so TAB on one used
    // to fold the whole heading it sits in and there was no way to collapse a
    // 200-line `#+begin_src` at all.
    //
    //   0 "* one"           1 ":PROPERTIES:"  2 ":ID: 42"    3 ":END:"
    //   4 "#+begin_src lisp" 5 "(+ 1 2)"      6 "#+end_src"  7 "* two"
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer = Buffer::from_str(
            "* one\n:PROPERTIES:\n:ID: 42\n:END:\n#+begin_src lisp\n(+ 1 2)\n#+end_src\n* two\n",
        );
        ed.buffer.major_mode = "org-mode".into();
        ed.buffer.cursor = ed.buffer.line_start(1); // on `:PROPERTIES:`
        ed.status.clear();
    }
    lisp.eval("(zemacs::org-cycle)".into());
    wait(&shared, "the drawer to fold", |ed| {
        (hidden(ed) == vec![2, 3]).then_some(())
    });
    // Two-state, like every other range: there is no CHILDREN inside a drawer.
    lisp.eval("(zemacs::org-cycle)".into());
    wait(&shared, "the drawer to open", |ed| (folds(ed) == 0).then_some(()));

    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.cursor = ed.buffer.line_start(4); // on `#+begin_src lisp`
        ed.status.clear();
    }
    lisp.eval("(zemacs::org-cycle)".into());
    wait(&shared, "the source block to fold", |ed| {
        (hidden(ed) == vec![5, 6]).then_some(())
    });
    lisp.eval("(zemacs::fold-open-all)".into());
    wait(&shared, "the block to open again", |ed| (folds(ed) == 0).then_some(()));

    // ...and the heading above them still cycles as a heading. The block reader
    // asks about the line under point only, so finding a `#+begin_src` five lines
    // down must not take TAB away from `* one`.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.cursor = 0;
        ed.status.clear();
    }
    lisp.eval("(zemacs::org-cycle)".into());
    wait(&shared, "the subtree over a block to fold whole", |ed| {
        (hidden(ed) == vec![1, 2, 3, 4, 5, 6]).then_some(())
    });
    lisp.eval("(zemacs::fold-open-all)".into());
    wait(&shared, "that subtree to open", |ed| (folds(ed) == 0).then_some(()));

    // --- code, with nobody having taught this mode anything ------------------
    //
    // The default the whole tree-sitter half exists for: `rust-mode` has no entry
    // in `*fold-subtree-functions*` and needs none. The ranges come out of the
    // same parse that colours the buffer, so a grammar in the build is a language
    // that folds.
    //
    //   0 "fn one() {"  1 "    let a = 1;"  2 "    let b = 2;"  3 "}"
    //   4 ""            5 "fn two() {"      6 "    println!();" 7 "}"
    const RUST: &str = "fn one() {\n    let a = 1;\n    let b = 2;\n}\n\nfn two() {\n    println!();\n}\n";
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer = Buffer::from_str(RUST);
        ed.buffer.major_mode = "rust-mode".into();
        // What `open_file` sets from the extension, and what the reader asks the
        // buffer for rather than making the caller name a grammar.
        ed.buffer.language = Some("rust".into());
        ed.buffer.cursor = 0;
        ed.status.clear();
    }
    lisp.eval("(zemacs::fold-dwim)".into());
    wait(&shared, "the function under point to fold", |ed| {
        (folds(ed) == 1).then_some(())
    });
    {
        let ed = shared.lock().unwrap();
        // The signature line stays and the body goes — the same shape org's
        // headline has, arrived at without a line of rust-specific policy.
        assert_eq!(hidden(&ed), vec![1, 2, 3], "the first function, not the second");
    }
    lisp.eval("(zemacs::fold-dwim)".into());
    wait(&shared, "the function to open", |ed| (folds(ed) == 0).then_some(()));

    // `fold-all` takes the *outermost* ranges only. Both functions, and neither
    // the `block` inside each — which is a node of its own with the same extent,
    // so a nested range that was not dropped would double every fold here.
    lisp.eval("(zemacs::fold-all)".into());
    wait(&shared, "both functions to fold", |ed| (folds(ed) == 2).then_some(()));
    assert_eq!(hidden(&shared.lock().unwrap()), vec![1, 2, 3, 6, 7]);
    lisp.eval("(zemacs::fold-open-all)".into());
    wait(&shared, "every fold to open", |ed| (folds(ed) == 0).then_some(()));

    // --- and a buffer with no grammar says so rather than guessing ------------
    //
    // Which is the boundary working from the other side: plain text has no
    // structure to read, so the reader answers with none and nothing is invented.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.language = None;
        ed.buffer.major_mode = "text-mode".into();
        ed.status.clear();
    }
    lisp.eval("(zemacs::fold-dwim)".into());
    let status = wait(&shared, "fold-dwim to decline", |ed| {
        (!ed.status.is_empty()).then(|| ed.status.clone())
    });
    assert!(status.contains("nothing foldable"), "got {status:?}");
    assert_eq!(folds(&shared.lock().unwrap()), 0);
}
