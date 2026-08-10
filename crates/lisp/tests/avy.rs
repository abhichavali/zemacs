//! avy, end to end — the second feature whose whole point is the *chain*.
//!
//! `which_key.rs` argues the case and it applies here unchanged: every link
//! below has a unit test somewhere and the feature is still dead if any one of
//! them is not wired to the next. So this drives the lot, against the shipped
//! `runtime/init.lisp` rather than a synthetic config, because the binding, the
//! module list and the load order are three of the links.
//!
//! 1. `SPC j c` reaches `avy-goto-char` — which needs `define-leader` in
//!    `init.lisp` *and* `modes/avy.lisp` in `*runtime-modules*`, since core runs
//!    an unrecognised action as `(avy-goto-char)` and a config that never loaded
//!    the file gets an undefined function;
//! 2. the image arms `grab-key`, which lands in `Editor::grab_key` — the one
//!    thing Lisp could not do for itself, and the only Rust this feature added;
//! 3. core hands the *next keystroke* over as `(avy-pick "b")` instead of
//!    dispatching it, which is the whole mechanism;
//! 4. the image finds the candidates in the visible region and puts one overlay
//!    per hit on the editor, each with a `display` of its label...
//! 5. ...and re-arms, so the key after that is a label;
//! 6. which moves point — to the *labelled* offset, not to the first hit.
//!
//! Then the path that matters more than any of them: **Esc cancels totally.** A
//! key that is no label still arrives here (that is what `grab-key` buys over a
//! transient keymap), so the labels come off and point has not moved. A
//! half-cleared screenful of labels is this feature's worst failure mode and is
//! the reason the cancel assertions below are on the count of overlays rather
//! than on a message.
//!
//! Deliberately a single `#[test]`, like every file beside it: `cl_boot`
//! initialises a process-wide image, so there is one `spawn` per test binary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Key, Mode, Shared};

const PATIENCE: Duration = Duration::from_secs(20);

/// Two lines chosen for their `b`s: four words start with one, at 6, 17, 24 and
/// 28, and `bee` at 24 is neither the first nor the last — so a jump landing
/// there cannot be a scan that stopped early or a fencepost.
const TEXT: &str = "alpha beta gamma\nbanana bee bravo\n";
const B_WORD_STARTS: [usize; 4] = [6, 17, 24, 28];

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

/// Press a key the way the application does: core turns it into commands, and a
/// Lisp call is the one effect core cannot have itself. Returns what core said,
/// so a test can assert on the command as well as on its result.
fn press(shared: &Shared, lisp: &zemacs_lisp::Lisp, key: Key) -> Vec<EditorCommand> {
    press_watching_grab(shared, lisp, key).0
}

/// `press`, and also what `grab_key` held *the instant `handle_key` returned* —
/// read under the same lock, before the form reaches the image.
///
/// That timing is the whole point rather than fussiness. `avy-pick' arms
/// `grab-key' a second time to read the label, so by the time an `eval' has been
/// answered "is the grab clear?" is a question about a *window* and not about a
/// state, and asking it afterwards is a race that passes alone and fails under a
/// loaded suite. Spending the grab is `handle_key''s own job, and this is the
/// only place it can be observed happening.
fn press_watching_grab(
    shared: &Shared,
    lisp: &zemacs_lisp::Lisp,
    key: Key,
) -> (Vec<EditorCommand>, Option<String>) {
    let (cmds, grabbed) = {
        let mut ed = shared.lock().unwrap();
        let cmds = ed.handle_key(key);
        (cmds, ed.grab_key.clone())
    };
    for cmd in &cmds {
        match cmd.clone() {
            EditorCommand::CallLisp(form) => lisp.eval(form),
            other => shared.lock().unwrap().apply(other),
        }
    }
    (cmds, grabbed)
}

/// The labels currently on the buffer, as `(start, display)`, in creation order.
/// Every overlay avy makes carries a `display`, so this is also the count.
fn labels(ed: &Editor) -> Vec<(usize, String)> {
    ed.buffer
        .overlays()
        .iter()
        .filter_map(|o| o.display.clone().map(|d| (o.start, d)))
        .collect()
}

/// `SPC j c`, then the character to search for — links 1 through 5, which every
/// half of this test needs before it can say anything.
fn arm(shared: &Shared, lisp: &zemacs_lisp::Lisp, search: char) {
    for key in [Key::Char(' '), Key::Char('j'), Key::Char('c')] {
        press(shared, lisp, key);
    }
    wait(shared, "avy to ask for the character", |ed| {
        (ed.grab_key.as_deref() == Some("avy-pick")).then_some(())
    });
    press(shared, lisp, Key::Char(search));
    wait(shared, "the labels to reach the editor", |ed| {
        (labels(ed).len() == B_WORD_STARTS.len()).then_some(())
    });
}

#[test]
fn a_typed_character_labels_the_screen_and_one_key_jumps() {
    let entry = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp"),
    )
    .expect("runtime/init.lisp must exist");

    let init = std::env::temp_dir().join("zemacs_test_avy_init.lisp");
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(message "avy test init loaded")
"#,
            entry.display().to_string()
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = Arc::new(zemacs_lisp::spawn(tx, shared.clone(), init));

    wait(&shared, "avy.lisp to load", |ed| {
        ed.messages.iter().any(|m| m == "avy test init loaded").then_some(())
    });
    // The leader bindings are `%do` commands and land asynchronously, so the
    // keystrokes below have to wait for them or they test an empty keymap.
    wait(&shared, "the leader bindings to arrive", |ed| {
        (ed.keymap.len() >= 3).then_some(())
    });

    {
        let mut ed = shared.lock().unwrap();
        ed.load(TEXT, Some(PathBuf::from("/tmp/zemacs_avy_test.txt")), None);
        ed.apply(EditorCommand::SetMode(Mode::Normal));
        ed.apply(EditorCommand::MoveTo(0));
    }

    // --- link 1: the shipped binding reaches the shipped module -------------
    for key in [Key::Char(' '), Key::Char('j')] {
        press(&shared, &lisp, key);
    }
    let cmds = press(&shared, &lisp, Key::Char('c'));
    assert!(
        cmds.iter().any(|c| matches!(c, EditorCommand::CallLisp(s) if s.contains("avy-goto-char"))),
        "`SPC j c' must reach avy; got {cmds:?}"
    );

    // --- link 2: the image asks for the next keystroke -----------------------
    wait(&shared, "avy to arm grab-key", |ed| {
        (ed.grab_key.as_deref() == Some("avy-pick")).then_some(())
    });

    // --- link 3: core hands that keystroke over instead of dispatching it ----
    //
    // `b` in Normal mode is *back a word*, and the assertion that it did not do
    // that is the assertion that this mechanism exists at all.
    let (cmds, grabbed) = press_watching_grab(&shared, &lisp, Key::Char('b'));
    assert_eq!(
        cmds,
        vec![EditorCommand::CallLisp("(avy-pick \"b\")".into())],
        "a grabbed key goes to the image whole, and nowhere else"
    );
    assert!(
        grabbed.is_none(),
        "and the grab is spent by the key that satisfied it"
    );

    // --- link 4: one overlay per candidate, each carrying its label ----------
    wait(&shared, "the labels to reach the editor", |ed| {
        (labels(ed).len() == B_WORD_STARTS.len()).then_some(())
    });
    let drawn = labels(&shared.lock().unwrap());
    assert_eq!(
        drawn.iter().map(|(at, _)| *at).collect::<Vec<_>>(),
        B_WORD_STARTS,
        "word starts only, in document order — `beta' is a hit and the `b' of \
         `alphabet' would not be"
    );
    assert_eq!(
        drawn.iter().map(|(_, d)| d.as_str()).collect::<Vec<_>>(),
        vec!["a", "s", "d", "f"],
        "`*avy-keys*' in order, one character each"
    );

    // --- link 5, then 6: the second key lands on the *labelled* one ----------
    wait(&shared, "avy to arm for a label", |ed| {
        (ed.grab_key.as_deref() == Some("avy-pick")).then_some(())
    });
    press(&shared, &lisp, Key::Char('d')); // the third label, `bee'
    wait(&shared, "point to reach the labelled offset", |ed| {
        (ed.buffer.cursor == B_WORD_STARTS[2]).then_some(())
    });
    assert!(
        labels(&shared.lock().unwrap()).is_empty(),
        "and the labels go with the jump"
    );

    // --- the path that matters most: Esc, and nothing left behind ------------
    //
    // Esc is not in `*avy-keys*` and could not be — this is the case a transient
    // keymap cannot express, since a key that is not a label would fall through
    // to the editor with the labels still on screen. It arrives here instead.
    arm(&shared, &lisp, 'b');
    let before = shared.lock().unwrap().buffer.cursor;
    let cmds = press(&shared, &lisp, Key::Esc);
    assert_eq!(
        cmds,
        vec![EditorCommand::CallLisp("(avy-pick \"<esc>\")".into())],
        "Esc reaches the image spelled the way `key-bindings' spells it"
    );
    wait(&shared, "the labels to come off", |ed| {
        labels(ed).is_empty().then_some(())
    });
    assert_eq!(
        shared.lock().unwrap().buffer.cursor,
        before,
        "a cancel leaves point exactly where it was"
    );
    assert!(
        shared.lock().unwrap().grab_key.is_none(),
        "...and does not go on holding the keyboard"
    );

    // A key that is a *letter* but not a label cancels the same way — the arm
    // above is not special-casing Esc, and `z' must not delete or repeat.
    arm(&shared, &lisp, 'b');
    let before = shared.lock().unwrap().buffer.cursor;
    press(&shared, &lisp, Key::Char('z'));
    wait(&shared, "an unknown label to cancel", |ed| {
        labels(ed).is_empty().then_some(())
    });
    assert_eq!(shared.lock().unwrap().buffer.cursor, before);

    // Nothing on screen to jump to is a cancel too, not a grab left armed: a
    // search that found nothing used to be the one way to be left waiting for a
    // label that does not exist.
    for key in [Key::Char(' '), Key::Char('j'), Key::Char('c')] {
        press(&shared, &lisp, key);
    }
    wait(&shared, "avy to ask for the character", |ed| {
        (ed.grab_key.as_deref() == Some("avy-pick")).then_some(())
    });
    press(&shared, &lisp, Key::Char('q')); // no word on screen starts with `q'
    wait(&shared, "avy to report an empty screen", |ed| {
        ed.status.contains("no q on screen").then_some(())
    });
    assert!(
        shared.lock().unwrap().grab_key.is_none(),
        "an empty search releases the keyboard rather than waiting for a label"
    );
    assert!(labels(&shared.lock().unwrap()).is_empty());
}
