//! Headless proof of the Common Lisp runtime: loading an `init.lisp` must turn
//! into the corresponding `EditorCommand`s on the channel, a Lisp error in the
//! init file must be caught and reported instead of killing the image, the
//! image must still answer `eval` afterwards, and the shipped config must
//! publish a usable `M-x` list.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide Lisp
//! image, so there is exactly one `spawn` per test binary.

use std::time::{Duration, Instant};

use zemacs_core::EditorCommand;

/// Drain the channel until `pred` matches or we give up.
fn wait_for(
    rx: &crossbeam_channel::Receiver<EditorCommand>,
    seen: &mut Vec<EditorCommand>,
    what: &str,
    pred: impl Fn(&EditorCommand) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if seen.iter().any(&pred) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; got {seen:#?}"
        );
        if let Ok(c) = rx.recv_timeout(Duration::from_millis(100)) {
            seen.push(c);
        }
    }
}

#[test]
fn init_lisp_drives_editor_commands() {
    let init = std::env::temp_dir().join("zemacs_test_init.lisp");
    std::fs::write(
        &init,
        r#"(in-package :zemacs)
(set-font-size 30)
(set-background 0.1 0.2 0.3)
(set-line-numbers nil)
(set-syntax-color "keyword" 1.0 0.0 0.0)
(set-completion-style "center")
(dashboard-item #\f "Find file" "find-file")
(dashboard-item "q" "Quit" "quit")
(define-key "normal" "SPC f f" "find-file")
(save-file)
(message (format nil "hello from ~a" (lisp-implementation-type)))
;; Everything above must have arrived before this blows up.
(this-function-does-not-exist 42)
(message "unreachable")
"#,
    )
    .unwrap();

    let (tx, rx) = crossbeam_channel::unbounded();
    let lisp = zemacs_lisp::spawn(tx, init);
    let mut seen = Vec::new();

    // --- settings ---
    wait_for(&rx, &mut seen, "SetFontSize", |c| {
        *c == EditorCommand::SetFontSize(30.0)
    });
    wait_for(&rx, &mut seen, "SetBackground", |c| {
        *c == EditorCommand::SetBackground([0.1, 0.2, 0.3])
    });
    // NIL is false, and only NIL: proves the C side tests for ECL_NIL.
    wait_for(&rx, &mut seen, "SetLineNumbers(false)", |c| {
        *c == EditorCommand::SetLineNumbers(false)
    });
    wait_for(&rx, &mut seen, "SetSyntaxColor", |c| {
        *c == EditorCommand::SetSyntaxColor("keyword".into(), [1.0, 0.0, 0.0])
    });
    // The name is passed through verbatim; core owns the aliases.
    wait_for(&rx, &mut seen, "SetCompletionStyle", |c| {
        *c == EditorCommand::SetCompletionStyle("center".into())
    });

    // --- dashboard: character and one-char-string keys both work ---
    wait_for(&rx, &mut seen, "AddDashboardItem(#\\f)", |c| {
        matches!(c, EditorCommand::AddDashboardItem { key: 'f', label, action }
                 if label == "Find file" && action == "find-file")
    });
    wait_for(&rx, &mut seen, "AddDashboardItem(\"q\")", |c| {
        matches!(c, EditorCommand::AddDashboardItem { key: 'q', .. })
    });

    // --- keymap ---
    wait_for(&rx, &mut seen, "BindKey", |c| {
        matches!(c, EditorCommand::BindKey { mode, keys, command }
                 if mode == "normal" && keys == "SPC f f" && command == "find-file")
    });

    // --- the &optional wrapper: (save-file) means "save in place" ---
    wait_for(&rx, &mut seen, "SaveFile(None)", |c| {
        *c == EditorCommand::SaveFile(None)
    });

    // --- it is genuinely Common Lisp, not a lookalike ---
    wait_for(&rx, &mut seen, "message from init", |c| {
        matches!(c, EditorCommand::Message(m) if m.contains("hello from ECL"))
    });

    // --- the error is caught in Lisp and reported, not fatal ---
    wait_for(&rx, &mut seen, "caught init error", |c| {
        matches!(c, EditorCommand::Message(m) if m.contains("init.lisp error"))
    });
    assert!(
        !seen.iter().any(|c| matches!(c, EditorCommand::Message(m) if m == "unreachable")),
        "load should have stopped at the failing form"
    );

    // --- and the image survived it ---
    lisp.eval(
        "(zemacs:message (format nil \"~a ~a\" (lisp-implementation-type) (+ 1 2)))".into(),
    );
    wait_for(&rx, &mut seen, "message from eval", |c| {
        matches!(c, EditorCommand::Message(m) if m.contains("ECL 3"))
    });

    // --- a bad eval is reported the same way, and does not stop the next one ---
    lisp.eval("(no-such-function)".into());
    wait_for(&rx, &mut seen, "caught eval error", |c| {
        matches!(c, EditorCommand::Message(m) if m.contains("lisp error"))
    });
    lisp.eval("(zemacs:set-tab-width 8)".into());
    wait_for(&rx, &mut seen, "SetTabWidth after error", |c| {
        *c == EditorCommand::SetTabWidth(8)
    });

    // --- the config we actually ship must load cleanly ---
    let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp");
    let mark = seen.len();
    lisp.eval(format!(
        "(load {:?} :verbose nil :print nil)",
        shipped.display().to_string()
    ));
    wait_for(&rx, &mut seen, "runtime/init.lisp banner", |c| {
        matches!(c, EditorCommand::Message(m) if m.contains("is driving the editor"))
    });
    assert!(
        !seen[mark..].iter().any(|c| matches!(c, EditorCommand::Message(m) if m.contains("lisp error"))),
        "runtime/init.lisp should load without errors; got {:#?}",
        &seen[mark..]
    );
    // The banner is unicode, so this also proves extended strings survive the
    // trip through the shim byte-exact.
    assert!(
        seen[mark..].iter().any(|c| matches!(c, EditorCommand::SetDashboardBanner(b)
                                            if b.contains("███████╗"))),
        "expected the unicode banner from runtime/init.lisp"
    );

    // --- what M-x offers ---
    // `refresh-commands` clears first so a reload cannot duplicate the list;
    // everything it publishes therefore has to come after that clear.
    let tail = &seen[mark..];
    let cleared = tail
        .iter()
        .position(|c| *c == EditorCommand::ClearCommands)
        .unwrap_or_else(|| panic!("expected ClearCommands from runtime/init.lisp; got {tail:#?}"));
    let names: Vec<&str> = tail[cleared..]
        .iter()
        .filter_map(|c| match c {
            EditorCommand::RegisterCommand(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        tail[..cleared]
            .iter()
            .all(|c| !matches!(c, EditorCommand::RegisterCommand(_))),
        "commands must be registered after the clear, not before; got {tail:#?}"
    );
    assert!(
        names.contains(&"text-scale-increase"),
        "a zero-argument command must be offered; got {names:?}"
    );
    // M-x calls `(name)`, so anything needing an argument would only ever error.
    // ECL reports no lambda list for the C primitives, which is why `set-font-size`
    // (a primitive taking one argument) stays out.
    assert!(
        !names.contains(&"set-font-size") && !names.contains(&"set-scale"),
        "commands taking arguments must not be offered; got {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with('%')),
        "internal helpers must not be offered; got {names:?}"
    );
    // ECL stores SYMBOL-NAME upcased; the list is what the user reads.
    assert!(
        names.iter().all(|n| **n == n.to_lowercase()),
        "command names must be lowercase; got {names:?}"
    );

    // A command it defines is callable by *bare* name — which is exactly what a
    // dashboard item or a keybinding sends: core emits `(lisp-version)`, with no
    // package prefix.
    //
    // Regression: LOAD restores `*package*` to CL-USER when it returns, so
    // unless `eval-string` binds `*package*` to ZEMACS around the READ, this
    // reads as an undefined CL-USER::LISP-VERSION and every binding in the
    // editor fails with "the function ... is not defined".
    let mark = seen.len();
    lisp.eval("(lisp-version)".into());
    wait_for(&rx, &mut seen, "bare user-defined command", |c| {
        matches!(c, EditorCommand::Message(m) if m.contains("ECL") && m.contains("symbols"))
    });
    // Host primitives resolve unprefixed too.
    lisp.eval("(set-tab-width 3)".into());
    wait_for(&rx, &mut seen, "bare primitive", |c| {
        *c == EditorCommand::SetTabWidth(3)
    });
    assert!(
        !seen[mark..]
            .iter()
            .any(|c| matches!(c, EditorCommand::Message(m) if m.contains("not defined"))),
        "bare command names must resolve in ZEMACS; got {:#?}",
        &seen[mark..]
    );

    // Text a Lisp command inserts is one user-level edit, so a Checkpoint has to
    // precede it — otherwise `u` skips back past it to whatever the user last
    // typed by hand.
    let mark = seen.len();
    let inserted = EditorCommand::InsertText("(lambda (x) x)".into());
    lisp.eval(r#"(zemacs:insert "(lambda (x) x)")"#.into());
    wait_for(&rx, &mut seen, "InsertText from Lisp", |c| *c == inserted);
    let tail = &seen[mark..];
    let at_checkpoint = tail.iter().position(|c| *c == EditorCommand::Checkpoint);
    let at_insert = tail.iter().position(|c| *c == inserted);
    assert!(
        matches!((at_checkpoint, at_insert), (Some(c), Some(i)) if c < i),
        "Checkpoint must precede Lisp-inserted text; got {tail:#?}"
    );
}
