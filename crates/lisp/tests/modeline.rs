//! The modeline the editor actually ships, which is `default-modeline` in
//! `runtime/library.lisp`.
//!
//! `crates/core/src/modeline.rs` used to *be* the strip: a function that pushed
//! a pill, then two spaces, then the buffer name, in those colours, in that
//! order. What is left there is the expander — the `%` codes and the rule that
//! drops a segment whose codes all came back empty — and its unit tests cover
//! that. What they cannot cover is whether the shipped config still asks for a
//! modeline anybody would want, because the shipped config is Lisp.
//!
//! So this boots the real image against a real [`Editor`] and reads the strip
//! back the way the renderer does. It is the test that fails when someone
//! deletes a line from `default-modeline` without meaning to, and the test that
//! proves the migration did not quietly lose the mode indicator.
//!
//! It lives in `library.lisp` rather than in `init.lisp` on purpose: `init.lisp`
//! is the file a user's own copy shadows, so a config written before any of this
//! existed would otherwise boot into the bare fallback core keeps for a headless
//! session. The last assertion here is the other half of that — a config *can*
//! replace the lot.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::time::{Duration, Instant};

use zemacs_core::modeline::{self, Face};
use zemacs_core::{Editor, HlKind, Mode, Shared};

const PATIENCE: Duration = Duration::from_secs(30);

fn wait<T>(shared: &Shared, what: &str, f: impl Fn(&Editor) -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(v) = f(&shared.lock().unwrap()) {
            return v;
        }
        if Instant::now() >= deadline {
            let ed = shared.lock().unwrap();
            panic!(
                "timed out waiting for {what}\nmessages:\n  {}",
                ed.messages
                    .iter()
                    .rev()
                    .take(15)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n  ")
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn runtime(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .expect("the runtime directory ships with the source")
}

/// Both halves of the strip, joined, the way the renderer walks them.
fn strip(ed: &Editor, active: bool) -> String {
    let (left, right) = modeline::segments(ed, &ed.buffer, active);
    left.iter()
        .chain(right.iter())
        .map(|s| s.text.clone())
        .collect()
}

#[test]
fn the_image_builds_the_strip_and_the_editor_only_expands_it() {
    let init = std::env::temp_dir().join(format!(
        "zemacs_test_modeline_init-{}.lisp",
        std::process::id()
    ));
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (message \"modeline test init loaded\")\n",
            runtime("modes/modes.lisp"),
            runtime("init.lisp"),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init.clone());
    wait(&shared, "the init file", |ed| {
        ed.messages
            .iter()
            .any(|m| m == "modeline test init loaded")
            .then_some(())
    });

    // --- what init.lisp asked for --------------------------------------------

    let format = wait(&shared, "a modeline from the image", |ed| {
        (ed.modeline.left.len() > 2).then(|| ed.modeline.clone())
    });
    // The pill leads, in the colour of the state, filled. That is the one
    // segment the strip would be worse without — see init.lisp.
    assert_eq!(format.left[0].template, " %m ");
    assert_eq!(format.left[0].face, Face::Mode);
    assert!(format.left[0].filled && format.left[0].bold);
    // The buffer's name, bold, next to it.
    assert!(format.left[1].template.contains("%b"), "{:?}", format.left[1]);
    assert!(format.left[1].bold);
    // ...and the position bookends the right edge.
    let last = format.right.last().expect("a right group");
    assert!(last.template.contains("%p"), "{last:?}");
    assert_eq!(last.face, Face::Named(HlKind::Accent));
    assert!(last.filled);

    // A face name from `face-list` survived the trip as the face it names —
    // which is the thing the integer packing in `command_for` could get wrong
    // without any test noticing, since a wrong face is still a colour.
    assert!(
        format.right.iter().any(|s| s.face == Face::Named(HlKind::Type)),
        "the major mode should be drawn in `type`: {:#?}",
        format.right
    );

    // --- and what it expands to ----------------------------------------------

    {
        let mut ed = shared.lock().unwrap();
        ed.load("hello\nworld", None, None);
        ed.mode = Mode::Insert;
        ed.buffer.modified = true;
        ed.status = "saved".into();
    }
    let ed = shared.lock().unwrap();
    let active = strip(&ed, true);
    assert!(active.contains("INSERT"), "{active}");
    assert!(active.contains('●'), "unsaved work is marked: {active}");
    assert!(active.contains("saved"), "the message rides here: {active}");
    assert!(active.contains("1:1"), "{active}");

    // The inactive pane keeps its own buffer's facts and drops every field that
    // describes the focused window — including the separators those fields
    // carried, which is the drop-empty rule doing its job over a real format.
    let idle = strip(&ed, false);
    assert!(idle.contains('●'), "{idle}");
    assert!(!idle.contains("INSERT"), "{idle}");
    assert!(!idle.contains("saved"), "{idle}");
    drop(ed);

    // --- and that a config can replace the lot -------------------------------
    //
    // The whole point of the migration: this is one line in an init file, and it
    // used to be a rebuild.
    lisp.eval("(progn (clear-modeline) (modeline-segment :left \"[%b]\"))".into());
    wait(&shared, "the replaced strip", |ed| {
        (ed.modeline.left.len() == 1 && ed.modeline.right.is_empty()).then_some(())
    });
    let ed = shared.lock().unwrap();
    assert_eq!(strip(&ed, true), format!("[{}]", ed.buffer.name()));
    drop(ed);

    let _ = std::fs::remove_file(&init);
}
