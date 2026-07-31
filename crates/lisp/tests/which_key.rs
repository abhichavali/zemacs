//! which-key, end to end — the one feature that spans every layer at once.
//!
//! Four links, and nothing short of running all of them proves the feature:
//!
//! 1. **core** notices that a prefix is pending and emits
//!    `CallLisp("(when (fboundp 'which-key) (which-key \"SPC\"))")` — the one
//!    fact Lisp cannot read for itself, since a half-typed sequence lives in
//!    `Pending` and nowhere else;
//! 2. the **app** hands that form to the image, which is one arm of `dispatch`;
//! 3. the **image** answers with a `message`, which lands in `Editor::status`
//!    and is what the modeline draws;
//! 4. and with one `(which-key-row ...)` per continuation, which lands in
//!    `Editor::which_key` and is what the renderer draws as a grid above the
//!    status line — plus the other half of that, which runs no Lisp at all:
//!    core empties the panel again the moment nothing is pending.
//!
//! Each link has a unit test somewhere and the feature was still dead, which is
//! the entire argument for this file: a chain is not tested by testing links.
//!
//! Deliberately a single `#[test]`, like every other file here — `cl_boot`
//! initialises a process-wide image, so there is one `spawn` per test binary.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Key, Mode, Shared};

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

/// Press `key` the way the application does: core turns it into commands, and
/// the only one this feature produces is a Lisp call. Returns what core said, so
/// the test can assert on link 1 as well as on the result.
fn press(shared: &Shared, lisp: &zemacs_lisp::Lisp, key: Key) -> Vec<EditorCommand> {
    let cmds = shared.lock().unwrap().handle_key(key);
    for cmd in &cmds {
        // The `dispatch` arm, spelled the same way: everything else goes to
        // `apply`, and a Lisp call is one of the three effects core cannot have.
        match cmd.clone() {
            EditorCommand::CallLisp(form) => lisp.eval(form),
            other => shared.lock().unwrap().apply(other),
        }
    }
    cmds
}

#[test]
fn a_pending_prefix_reaches_which_key_and_lands_in_the_status_line() {
    // The *shipped* config, not a synthetic one: which-key is loaded out of the
    // `dolist` at the foot of `init.lisp`, its rows come from the leader
    // bindings defined above that, and `%which-key-maps` asks the live buffer
    // which keymaps apply. Every one of those is a thing only the real config
    // exercises, which is why a test with three hand-written `define-key` calls
    // passed while the feature was dead in the editor.
    let entry = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp"),
    )
    .expect("runtime/init.lisp must exist");

    let init = std::env::temp_dir().join("zemacs_test_which_key_init.lisp");
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(message "which-key test init loaded")
"#,
            entry.display().to_string()
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = Arc::new(zemacs_lisp::spawn(tx, shared.clone(), init));

    wait(&shared, "which-key.lisp to load", |ed| {
        ed.messages
            .iter()
            .any(|m| m == "which-key test init loaded")
            .then_some(())
    });
    // The bindings are `%do` commands and land on the editor asynchronously, so
    // the keystroke below has to wait for them or it tests an empty keymap.
    wait(&shared, "the leader bindings to arrive", |ed| {
        (ed.keymap.len() >= 3).then_some(())
    });

    shared.lock().unwrap().mode = Mode::Normal;

    // --- link 1: core sees a pending prefix ---------------------------------
    let cmds = press(&shared, &lisp, Key::Char(' '));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, EditorCommand::CallLisp(s) if s.contains("which-key"))),
        "SPC with a leader binding must ask the image about the prefix; got {cmds:?}"
    );

    // --- links 2 and 3: the image answers, and the answer is on the modeline --
    let status = wait(&shared, "which-key to report the leader map", |ed| {
        ed.status.contains("SPC-").then(|| ed.status.clone())
    });
    assert!(status.contains("f +file"), "a prefix is named: {status:?}");
    assert!(status.contains("w +window"), "a group is named: {status:?}");
    assert!(
        shared.lock().unwrap().status_line().contains("SPC-"),
        "and the modeline is what draws it"
    );

    // A second key extends the sequence rather than ending it, so which-key
    // narrows instead of going quiet — this is the row that made the panel
    // worth having.
    let cmds = press(&shared, &lisp, Key::Char('f'));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, EditorCommand::CallLisp(s) if s.contains("SPC f"))),
        "a longer prefix is still pending; got {cmds:?}"
    );
    let status = wait(&shared, "which-key to narrow to the file map", |ed| {
        ed.status.contains("SPC f-").then(|| ed.status.clone())
    });
    assert!(status.contains("f find-file"), "narrowed: {status:?}");
    assert!(status.contains("s save-file"), "narrowed: {status:?}");

    // --- link 4: the panel ---------------------------------------------------
    //
    // The same answer on a surface with more than one row, which is a fourth
    // link and not a redraw of the third: the rows leave the image one `%do` at
    // a time, land in `Editor::which_key`, and the renderer draws them in a grid
    // above the status line. The `message` above has already been applied by the
    // time the status line reads "SPC f-", and the rows were emitted before it —
    // one Lisp thread, one queue — so this needs no second wait.
    let panel = shared.lock().unwrap().which_key.clone();
    assert!(
        panel.iter().any(|r| r.starts_with("f ") && r.contains("find-file")),
        "the panel carries the rows the status line summarised: {panel:?}"
    );
    assert!(
        panel.iter().all(|r| r.split(' ').next().is_some_and(|k| !k.is_empty())),
        "every row is `KEY LABEL`, which is what the renderer splits on: {panel:?}"
    );

    // ...and it must not outlive the sequence it describes. Esc resolves the
    // pending prefix, and core retires the panel *without asking the image* —
    // which is the only way it can work, since nothing calls Lisp on the key
    // that ends a sequence.
    press(&shared, &lisp, Key::Esc);
    assert!(
        shared.lock().unwrap().which_key.is_empty(),
        "a cancelled prefix takes its panel with it"
    );

    // --- the bug this file was written for ----------------------------------
    //
    // The editor *starts* on the dashboard, and dired and magit behave the same
    // way: they layer over Normal, so core keeps waiting for the rest of a
    // leader sequence there. which-key was asked, looked only in the mode's own
    // map, found nothing under `SPC', and said nothing — so the feature was
    // silent in the one buffer everybody sees first.
    for mode in [Mode::Dashboard, Mode::Dired, Mode::Magit] {
        // Esc first: a half-typed sequence is still pending from above, and
        // `SPC f SPC` is not a prefix of anything.
        press(&shared, &lisp, Key::Esc);
        {
            let mut ed = shared.lock().unwrap();
            ed.mode = mode;
            ed.status.clear();
        }
        let cmds = press(&shared, &lisp, Key::Char(' '));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::CallLisp(s) if s.contains("which-key"))),
            "{mode:?}: core layers over Normal, so SPC is still a pending prefix; got {cmds:?}"
        );
        let status = wait(&shared, &format!("which-key in {mode:?}"), |ed| {
            (!ed.status.is_empty()).then(|| ed.status.clone())
        });
        assert!(
            status.contains("SPC-") && status.contains("f +file"),
            "{mode:?}: the leader map is in reach and must be reported: {status:?}"
        );
        assert!(
            !shared.lock().unwrap().which_key.is_empty(),
            "{mode:?}: and the panel is filled there too, not only the line"
        );
    }
}
