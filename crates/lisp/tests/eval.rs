//! Headless proof of the Common Lisp runtime: loading an `init.lisp` must move
//! the editor it shares with the host, a Lisp error in the init file must be
//! caught and reported instead of killing the image, the image must still
//! answer `eval` afterwards, and the shipped config must publish a usable `M-x`
//! list.
//!
//! There are two observation points, because there are two paths out of the
//! image. Most commands are applied to the shared [`Editor`] on the spot, so
//! they are checked by looking at it. The handful that need the app layer
//! (`OpenFile`, `SaveFile`, …) still arrive on the channel — see
//! `EditorCommand::needs_app`.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide Lisp
//! image, so there is exactly one `spawn` per test binary.

use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Mode, Shared};

const PATIENCE: Duration = Duration::from_secs(20);

/// Poll the shared editor until `f` answers, or give up.
///
/// Everything here is asynchronous by construction — that is the point of the
/// design — so every assertion about editor state is a wait rather than a read.
fn wait<T>(shared: &Shared, what: &str, f: impl Fn(&Editor) -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(v) = f(&shared.lock().unwrap()) {
            return v;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; status={:?}",
            shared.lock().unwrap().status
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Wait for a message matching `pred` to have been produced at any point.
fn wait_message(shared: &Shared, what: &str, pred: impl Fn(&str) -> bool) {
    wait(shared, what, |ed| {
        ed.messages.iter().any(|m| pred(m)).then_some(())
    })
}

/// How many messages have been produced, for "nothing after this point said X"
/// assertions.
fn mark(shared: &Shared) -> usize {
    shared.lock().unwrap().messages.len()
}

fn messages_since(shared: &Shared, from: usize) -> Vec<String> {
    shared.lock().unwrap().messages[from..].to_vec()
}

/// Drain the channel until `pred` matches or we give up. Only the commands the
/// app has to carry out ever come this way.
fn wait_for(
    rx: &crossbeam_channel::Receiver<EditorCommand>,
    seen: &mut Vec<EditorCommand>,
    what: &str,
    pred: impl Fn(&EditorCommand) -> bool,
) {
    let deadline = Instant::now() + PATIENCE;
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
(set-modeline-relief -3)
(set-modeline-pad 12)
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
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    let mut seen = Vec::new();

    // --- settings ---
    wait(&shared, "SetFontSize", |ed| {
        (ed.settings.font_size == 30.0).then_some(())
    });
    wait(&shared, "SetBackground", |ed| {
        (ed.settings.background == [0.1, 0.2, 0.3]).then_some(())
    });
    // NIL is false, and only NIL: proves the C side tests for ECL_NIL.
    wait(&shared, "SetLineNumbers(false)", |ed| {
        (!ed.settings.line_numbers).then_some(())
    });
    wait(&shared, "SetSyntaxColor", |ed| {
        (ed.theme.color(zemacs_core::HlKind::Keyword, [0.0; 3]) == [1.0, 0.0, 0.0]).then_some(())
    });
    wait(&shared, "SetCompletionStyle", |ed| {
        (ed.settings.completion_style == zemacs_core::CompletionStyle::Center).then_some(())
    });
    // Relief is signed and the sign is the whole feature — negative sinks the
    // modeline instead of raising it. Nothing between Lisp and core may take an
    // absolute value or clamp at zero.
    wait(&shared, "SetModelineRelief(-3)", |ed| {
        (ed.settings.modeline_relief == -3).then_some(())
    });
    wait(&shared, "SetModelinePad", |ed| {
        (ed.settings.modeline_pad == 12).then_some(())
    });

    // --- dashboard: character and one-char-string keys both work ---
    wait(&shared, "dashboard items", |ed| {
        let items = &ed.dashboard.items;
        (items.iter().any(|i| {
            i.key == 'f' && i.label == "Find file" && i.action == "find-file"
        }) && items.iter().any(|i| i.key == 'q'))
        .then_some(())
    });

    // --- keymap ---
    wait(&shared, "BindKey", |ed| {
        (ed.keymap.get(&(Mode::Normal, "SPC f f".into())) == Some(&"find-file".to_string()))
            .then_some(())
    });

    // --- the &optional wrapper: (save-file) means "save in place" ---
    // Saving needs the filesystem, so unlike the settings above it goes to the
    // app rather than being applied in place.
    wait_for(&rx, &mut seen, "SaveFile(None)", |c| {
        *c == EditorCommand::SaveFile(None)
    });

    // --- it is genuinely Common Lisp, not a lookalike ---
    wait_message(&shared, "message from init", |m| {
        m.contains("hello from ECL")
    });

    // --- the error is caught in Lisp and reported, not fatal ---
    wait_message(&shared, "caught init error", |m| {
        m.contains("init.lisp error")
    });
    assert!(
        !shared.lock().unwrap().messages.iter().any(|m| m == "unreachable"),
        "load should have stopped at the failing form"
    );

    // --- and the image survived it ---
    lisp.eval("(zemacs:message (format nil \"~a ~a\" (lisp-implementation-type) (+ 1 2)))".into());
    wait_message(&shared, "message from eval", |m| m.contains("ECL 3"));

    // --- a bad eval is reported the same way, and does not stop the next one ---
    lisp.eval("(no-such-function)".into());
    wait_message(&shared, "caught eval error", |m| m.contains("lisp error"));
    lisp.eval("(zemacs:set-tab-width 8)".into());
    wait(&shared, "SetTabWidth after error", |ed| {
        (ed.settings.tab_width == 8).then_some(())
    });

    // --- a form's value is echoed, the way `eval-last-sexp' does ---
    lisp.eval("(+ 1 2)".into());
    wait_message(&shared, "value of (+ 1 2)", |m| m == "3");
    // Several forms in one string: the *last* value is the one reported.
    lisp.eval("(+ 1 2) (* 6 7)".into());
    wait_message(&shared, "value of the last form", |m| m == "42");
    // Printed with ~S, so a string is not confusable with a symbol of the same
    // name — the whole reason `eval-last-sexp' uses `prin1' and not `princ'.
    lisp.eval(r#"(concatenate 'string "scr" "atch")"#.into());
    wait_message(&shared, "readable string value", |m| m == "\"scratch\"");
    lisp.eval("'scratch".into());
    wait_message(&shared, "readable symbol value", |m| m == "SCRATCH");

    // --- NIL is deliberately silent ---
    // Every command that already called `message' returns NIL, so echoing NIL
    // would immediately overwrite the message the command just produced.
    let at = mark(&shared);
    lisp.eval(r#"(progn (zemacs:message "kept") nil)"#.into());
    wait_message(&shared, "the command's own message", |m| m == "kept");
    std::thread::sleep(Duration::from_millis(250));
    let tail = messages_since(&shared, at);
    assert!(
        !tail.iter().any(|m| m == "NIL"),
        "a NIL value must not clobber the message the form produced; got {tail:#?}"
    );

    // --- the config we actually ship must load cleanly ---
    let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp");
    let at = mark(&shared);
    lisp.eval(format!(
        "(load {:?} :verbose nil :print nil)",
        shipped.display().to_string()
    ));
    wait_message(&shared, "runtime/init.lisp banner", |m| {
        m.contains("is driving the editor")
    });
    let tail = messages_since(&shared, at);
    assert!(
        !tail.iter().any(|m| m.contains("lisp error")),
        "runtime/init.lisp should load without errors; got {tail:#?}"
    );
    // The config sets a banner at all.
    assert!(!shared.lock().unwrap().dashboard.banner.is_empty());

    // NOT asserted here, because it is not true: a non-ASCII string in a form
    // handed to `zemacs_eval` does *not* survive to the editor. It arrives as a
    // base string of UTF-8 bytes, ECL's reader parses those bytes as Latin-1
    // characters, and `dup_utf8` then encodes each of those back into UTF-8 — so
    // `(message "λ")` from a keybinding reaches the status line as `Î»`.
    //
    // The previous assertion here was "the banner contains a non-ASCII
    // character", which double-encoded text satisfies, so this went unnoticed.
    // The path that *is* correct is loading a file: `load` decodes through the
    // stream's external format, which is why org's `◉` bullets are right.
    //
    // Not fixed here because the fix is not local. Lisp deliberately holds text
    // as UTF-8 *bytes* — `buffer-string` returns them, `%byte-index` and
    // `%char-index` convert between the two counts, and `search-forward`
    // compares a pattern against them — so decoding eval'd source into real
    // characters makes it incomparable with buffer text and breaks every search.
    // Making it coherent means moving the whole model to character strings and
    // deleting the two index conversions, which is the ceiling `f_query` in
    // `shim.c` already writes up. Reaches: an accented answer to `read-string`,
    // a path with a non-ASCII component, any form built by Rust.

    // --- the link a terminal click lands on ---
    //
    // The whole of "is that a link" lives in `%url-at`; the app only hands it a
    // screen row and a column. Every case here is one the terminal produces:
    // a URL in prose, the blank right half of a row, and a bare word.
    for (line, col, want) in [
        ("see https://zemacs.dev/a. next", 12, "https://zemacs.dev/a"),
        ("see https://zemacs.dev/a. next", 25, "NIL"), // the space after it
        ("open file:///tmp/report.html", 20, "file:///tmp/report.html"),
        ("cargo test --lib", 3, "NIL"), // a bare word is not a link
    ] {
        // Reported through `message' rather than read off the echoed value,
        // because NIL is deliberately silent — see above — and NIL is half the
        // answers here.
        let at = mark(&shared);
        lisp.eval(format!(
            "(zemacs:message (format nil \"~a\" (zemacs::%url-at {line:?} {col})))"
        ));
        let got = wait(&shared, "%url-at value", |ed| {
            ed.messages.get(at..)?.first().cloned()
        });
        assert_eq!(got, want, "%url-at {line:?} {col}");
    }

    // --- what M-x offers ---
    let names = shared.lock().unwrap().commands.clone();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    // A candidate is *the source of a call*: `runtime/modes/which-key.lisp`
    // publishes `name #| docstring   KEY |#`, and core runs an unrecognised
    // candidate as `(candidate)`, where the block comment reads as nothing. So
    // the command's own name is the head, and the annotation is everything the
    // reader will throw away.
    let heads: Vec<&str> = names
        .iter()
        .map(|n| n.split_whitespace().next().unwrap_or(n))
        .collect();
    // `refresh-commands` clears first so a reload cannot duplicate the list.
    // The init file has now been loaded twice — once by `spawn`, once above — so
    // a missing clear would show up right here as every name appearing twice.
    let mut unique = heads.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        heads.len(),
        "reloading the config must not duplicate the M-x list; got {names:?}"
    );
    assert!(
        heads.contains(&"text-scale-increase"),
        "a zero-argument command must be offered; got {names:?}"
    );
    // M-x calls `(name)`, so anything needing an argument would only ever error.
    // ECL reports no lambda list for the C primitives, which is why `set-font-size`
    // (a primitive taking one argument) stays out.
    assert!(
        !heads.contains(&"set-font-size") && !heads.contains(&"set-scale"),
        "commands taking arguments must not be offered; got {names:?}"
    );
    assert!(
        !heads.iter().any(|n| n.starts_with('%')),
        "internal helpers must not be offered; got {names:?}"
    );
    // ECL stores SYMBOL-NAME upcased; the list is what the user reads.
    assert!(
        heads.iter().all(|n| **n == n.to_lowercase()),
        "command names must be lowercase; got {names:?}"
    );
    // The evaluation commands have to be reachable from M-x, not just from a key.
    for wanted in ["eval-file-dwim", "lisp-scratch", "yank-buffer-file-name"] {
        assert!(
            heads.contains(&wanted),
            "M-x must offer {wanted}; got {names:?}"
        );
    }

    // --- C-c evaluates Lisp in every mode you can be typing in ---
    //
    // `C-c C-c` outside Insert, so that `C-c` is left a *prefix* and the
    // `C-c <letter>` family is reachable at all: `normal_key` finds an exact
    // global binding before it asks whether the sequence is a prefix, so a bare
    // `C-c` made every global `C-c d` untypable.
    //
    // Insert is the exception and has to be: `insert_key` consults only
    // single-key bindings and never waits for a second, so `C-c C-c` cannot be
    // spelled there. The one-key binding stays, which also keeps it winning
    // over core's hardcoded "C-c is a synonym for Esc" — that rule is consulted
    // after the user keymap.
    {
        let ed = shared.lock().unwrap();
        let bound = |m, k: &str| ed.keymap.get(&(m, k.into())).map(String::as_str);
        for mode in [Mode::Normal, Mode::Visual, Mode::Dashboard] {
            assert_eq!(
                bound(mode, "C-c C-c"),
                Some("eval-dwim"),
                "expected C-c C-c bound to eval-dwim in {mode:?} mode"
            );
            assert_eq!(
                bound(mode, "C-c"),
                None,
                "C-c must stay a prefix in {mode:?} mode, not an exact binding"
            );
            // ...and that prefix is what the whole family hangs off. Listed out
            // rather than counted: the failure this guards against is one of
            // them quietly not being bound, and a count would still pass.
            for (keys, command) in [
                ("C-c d", "dired"),
                ("C-c t", "terminal"),
                ("C-c m", "magit-status"),
                ("C-c p", "project-switch"),
                ("C-c c", "project-find-file"),
                ("C-c a", "ai"),
                ("C-c i", "edit-config"),
                ("C-c s", "switch-buffer"),
                ("C-c b", "messages-buffer"),
                ("C-c y", "yank-buffer-file-name"),
            ] {
                assert_eq!(bound(mode, keys), Some(command), "{keys} in {mode:?}");
            }
        }
        assert_eq!(bound(Mode::Insert, "C-c"), Some("eval-dwim"));
        // The shipped theme drives the modeline through the same primitives.
        assert_ne!(
            ed.theme.color(zemacs_core::HlKind::Modeline, [0.0; 3]),
            [0.0; 3],
            "runtime/init.lisp should set a modeline color"
        );
    }

    // A command it defines is callable by *bare* name — which is exactly what a
    // dashboard item or a keybinding sends: core emits `(lisp-version)`, with no
    // package prefix.
    //
    // Regression: LOAD restores `*package*` to CL-USER when it returns, so
    // unless `eval-string` binds `*package*` to ZEMACS around the READ, this
    // reads as an undefined CL-USER::LISP-VERSION and every binding in the
    // editor fails with "the function ... is not defined".
    let at = mark(&shared);
    lisp.eval("(lisp-version)".into());
    wait_message(&shared, "bare user-defined command", |m| {
        m.contains("ECL") && m.contains("symbols")
    });
    // Host primitives resolve unprefixed too.
    lisp.eval("(set-tab-width 3)".into());
    wait(&shared, "bare primitive", |ed| {
        (ed.settings.tab_width == 3).then_some(())
    });
    let tail = messages_since(&shared, at);
    assert!(
        !tail.iter().any(|m| m.contains("not defined")),
        "bare command names must resolve in ZEMACS; got {tail:#?}"
    );

    // --- reading the editor ---
    //
    // Everything above pushes state *at* the editor. This is the other
    // direction, and the reason the image shares the editor rather than only
    // shouting commands at it: without it nothing in a config can depend on
    // where the cursor is or what is selected.
    {
        let mut ed = shared.lock().unwrap();
        ed.load("alpha\nbeta\ngamma\n", None, None);
        ed.apply(EditorCommand::SetMode(Mode::Normal));
        ed.buffer.move_to_line_col(1, 0);
    }
    // Integers come back as integers, so they are arithmetic, not strings.
    lisp.eval(r#"(message (format nil "~a" (+ (point) (line-number) (buffer-size))))"#.into());
    wait_message(&shared, "position queries", |m| m == "25"); // 6 + 2 + 17
    lisp.eval("(message (line-string))".into());
    wait_message(&shared, "line-string", |m| m == "beta");
    lisp.eval(r#"(message (format nil "~a/~a" (evil-state) (major-mode)))"#.into());
    wait_message(&shared, "modes", |m| m == "normal/fundamental-mode");
    // NIL is a real NIL, not the string "nil" — `(when (region) ...)` has to work.
    lisp.eval(r#"(message (format nil "~a" (null (region))))"#.into());
    wait_message(&shared, "no region", |m| m == "T");

    // A visual selection is a cons of char offsets.
    {
        let mut ed = shared.lock().unwrap();
        ed.apply(EditorCommand::SetMode(Mode::Visual));
        ed.apply(EditorCommand::MoveTo(9));
    }
    lisp.eval("(message (region-text))".into());
    wait_message(&shared, "region-text", |m| m == "beta");

    // --- writing the editor ---
    //
    // `replace-region` is the atomic primitive: one lock, one undo step, and no
    // window for a keystroke to land in the middle of it. This is the shape of
    // every "wrap the selection in something" command.
    lisp.eval(
        r#"(let ((r (region)))
             (replace-region (car r) (cdr r)
                             (concatenate 'string "*" (region-text) "*")))"#
            .into(),
    );
    wait(&shared, "replace-region", |ed| {
        (ed.buffer.text.to_string() == "alpha\n*beta*\ngamma\n").then_some(())
    });
    // ...and it is *one* user-level edit, so a single undo takes all of it back.
    {
        let mut ed = shared.lock().unwrap();
        ed.apply(EditorCommand::Undo);
        assert_eq!(ed.buffer.text.to_string(), "alpha\nbeta\ngamma\n");
    }

    // Text a Lisp command inserts is one user-level edit too, so a Checkpoint
    // has to precede it — otherwise `u` skips back past it to whatever the user
    // last typed by hand.
    lisp.eval(r#"(progn (goto-char 0) (insert "(lambda (x) x)"))"#.into());
    wait(&shared, "insert from Lisp", |ed| {
        ed.buffer
            .text
            .to_string()
            .starts_with("(lambda (x) x)alpha")
            .then_some(())
    });
    {
        let mut ed = shared.lock().unwrap();
        ed.apply(EditorCommand::Undo);
        assert_eq!(
            ed.buffer.text.to_string(),
            "alpha\nbeta\ngamma\n",
            "one undo must take back everything a Lisp command inserted"
        );
    }

    // A read *after* a write sees it, because pure commands are applied on the
    // spot rather than queued. This is the whole reason the split in
    // `EditorCommand::needs_app` exists.
    lisp.eval(r#"(progn (goto-char 0) (insert "zz") (message (format nil "~a" (point))))"#.into());
    wait_message(&shared, "point after an insert", |m| m == "2");

    // --- evaluating a whole file of Lisp ---
    // `%eval-file` binds *PACKAGE* to ZEMACS around the LOAD, which is what lets
    // a file that never says `(in-package :zemacs)` — the scratch buffer — call
    // the primitives unqualified. Without that binding this reads as an
    // undefined CL-USER::MESSAGE.
    let file = std::env::temp_dir().join("zemacs_test_eval_file.lisp");
    std::fs::write(&file, "(message \"evaluated from a file\")\n").unwrap();
    lisp.eval(format!("(%eval-file {:?})", file.display().to_string()));
    wait_message(&shared, "message from the loaded file", |m| {
        m == "evaluated from a file"
    });
    wait_message(&shared, "report naming the file", |m| {
        m.contains("evaluated zemacs_test_eval_file")
    });

    // A broken file is reported and the image keeps going.
    std::fs::write(&file, "(this-is-not-defined)\n").unwrap();
    lisp.eval(format!("(%eval-file {:?})", file.display().to_string()));
    wait_message(&shared, "report of a bad file", |m| {
        m.contains("zemacs_test_eval_file") && m.contains("NOT-DEFINED")
    });

    // --- the scratch buffer ---
    // It is a real file, because `find-file` is the only primitive that can put
    // the editor in a *different* buffer; `insert` would write the header into
    // whatever the user was already editing.
    let scratch = std::env::temp_dir().join("zemacs_test_scratch.lisp");
    let _ = std::fs::remove_file(&scratch);
    lisp.eval(format!(
        "(setf *scratch-file* (pathname {:?}))",
        scratch.display().to_string()
    ));
    lisp.eval("(lisp-scratch)".into());
    wait_for(&rx, &mut seen, "OpenFile for the scratch buffer", |c| {
        matches!(c, EditorCommand::OpenFile(p)
                 if p.file_name() == Some(std::ffi::OsStr::new("zemacs_test_scratch.lisp")))
    });
    let header = std::fs::read_to_string(&scratch).expect("lisp-scratch must create the file");
    assert!(
        header.contains("*scratch*") && header.contains("ECL"),
        "the scratch file should be seeded with a header; got {header:?}"
    );
    // Seeded once: opening it again must not overwrite what the user wrote.
    // The marker rides behind it on the same FIFO, so seeing it means the
    // second `lisp-scratch` has already finished.
    std::fs::write(&scratch, "(message \"mine\")\n").unwrap();
    lisp.eval("(lisp-scratch)".into());
    lisp.eval(r#"(message "scratch reopened")"#.into());
    wait_message(&shared, "the scratch buffer reopening", |m| {
        m == "scratch reopened"
    });
    assert_eq!(
        std::fs::read_to_string(&scratch).unwrap(),
        "(message \"mine\")\n",
        "re-opening the scratch buffer must not clobber it"
    );

    // --- markers ---
    //
    // A char offset read into Lisp names a character only until the next edit;
    // a marker names it afterwards too. This is the whole round trip: make one,
    // edit in front of it from Lisp, and it still names the same word.
    {
        let mut ed = shared.lock().unwrap();
        ed.load("alpha\nbeta\ngamma\n", None, None);
        ed.apply(EditorCommand::SetMode(Mode::Normal));
    }
    lisp.eval(
        r#"(let ((m (progn (goto-char 6) (point-marker))))
             (goto-char 0)
             (insert "XY")
             (message (format nil "~a ~a" (marker-position m)
                              (buffer-substring (marker-position m)
                                                (+ (marker-position m) 4))))
             (delete-marker m)
             (message (format nil "deleted ~a bogus ~a"
                              (marker-position m) (marker-position 99999))))"#
            .into(),
    );
    wait_message(&shared, "a marker pushed along by an insert", |m| {
        m == "8 beta"
    });
    // A handle naming nothing answers NIL rather than a wrong position: holding
    // one too long is a mistake a config makes, not one it can crash on.
    wait_message(&shared, "a deleted marker", |m| m == "deleted NIL bogus NIL");

    // The insertion type is what happens to a marker at exactly the point of an
    // insertion, and it defaults to staying put, as in Emacs.
    lisp.eval(
        r#"(let ((stay (make-marker 2)) (ride (make-marker 2 t)))
             (goto-char 2)
             (insert "..")
             (message (format nil "stay ~a ride ~a"
                              (marker-position stay) (marker-position ride))))"#
            .into(),
    );
    wait_message(&shared, "both insertion types", |m| m == "stay 2 ride 4");
}
