//! Headless proof of the extension API: everything a config can reach that is
//! not already covered by `eval.rs` (settings, dashboard, keys, markers) or
//! `modes.rs` (major and minor modes).
//!
//! What is under test here is mostly *composition* — the Lisp library the shim
//! installs on top of `%QUERY` and `%DO` — so the assertions are the shapes a
//! config actually writes: switch to a buffer by name, wrap the region, replace
//! every occurrence of something as one undo step, ask what a key is bound to.
//!
//! Deliberately a single `#[test]`, as in the two files beside it: `cl_boot`
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
    })
}

/// Wait for a message equal to `text`. Most assertions here are "evaluate this
/// and report the answer", so this is the workhorse.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"~a\" {form}))"));
    wait_message(shared, form, |m| m == want);
}

/// Drain the channel until `pred` matches. Only the commands the app has to
/// carry out ever come this way.
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
        assert!(Instant::now() < deadline, "timed out waiting for {what}; got {seen:#?}");
        if let Ok(c) = rx.recv_timeout(Duration::from_millis(100)) {
            seen.push(c);
        }
    }
}

/// A three-line file in a plain text buffer, in Normal mode.
///
/// Restoring it is a rewrite rather than a second `Editor::load`: loading a path
/// that is already open is a *switch* to the buffer showing it, so the text
/// would be whatever the last assertion left behind.
fn load(shared: &Shared) {
    let mut ed = shared.lock().unwrap();
    if ed.buffer.path.is_none() {
        ed.load("", Some(PathBuf::from("/tmp/zemacs_api_test.txt")), None);
    }
    ed.apply(EditorCommand::SetMode(Mode::Insert));
    let n = ed.buffer.len_chars();
    ed.apply(EditorCommand::DeleteRange(0, n));
    ed.apply(EditorCommand::InsertText("alpha\nbeta\ngamma\n".into()));
    ed.apply(EditorCommand::MoveTo(0));
    ed.apply(EditorCommand::SetMode(Mode::Normal));
}

#[test]
fn the_extension_api_reaches_the_editor() {
    // Nothing to configure: the whole point is that the library is installed by
    // the shim, before any config is read.
    let init = std::env::temp_dir().join("zemacs_test_api_init.lisp");
    std::fs::write(&init, "(in-package :zemacs)\n(message \"api test init loaded\")\n").unwrap();

    let (tx, rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    let mut seen = Vec::new();
    wait_message(&shared, "the init file", |m| m == "api test init loaded");

    load(&shared);

    // --- line-oriented reading ------------------------------------------
    //
    // The zero-argument spelling still means "the line point is on"; the
    // argument is 1-based, agreeing with what `line-number` reports.
    says(&shared, &lisp, "(line-string 2)", "beta");
    says(&shared, &lisp, "(line-start 2)", "6");
    says(&shared, &lisp, "(line-end 2)", "10");
    lisp.eval("(goto-line 3)".into());
    says(&shared, &lisp, "(list (point) (line-number) (line-string))", "(11 3 gamma)");
    lisp.eval("(beginning-of-line)".into());
    says(&shared, &lisp, "(point)", "11");
    // 15 and not the 16 `line-end` reports: in Normal mode the cursor sits *on*
    // a character, so `goto-char` past the last one is pulled back. Anything
    // aiming at the end of a line has to expect that.
    lisp.eval("(end-of-line)".into());
    says(&shared, &lisp, "(list (point) (line-end))", "(15 16)");

    // --- searching ------------------------------------------------------
    //
    // Literal, in the buffer's own char offsets, so the answer feeds straight
    // into `goto-char` and `replace-region`.
    // The rope counts the empty line after a trailing newline, so `line-count`
    // is 4 here. `buffer-lines` drops it, which is what keeps every
    // line-oriented command from having to remember that.
    says(&shared, &lisp, "(list (line-count) (buffer-lines))", "(4 (alpha beta gamma))");

    says(&shared, &lisp, "(search-forward \"beta\" 0)", "6");
    says(&shared, &lisp, "(search-forward \"nope\" 0)", "NIL");
    says(&shared, &lisp, "(search-backward \"a\" 5)", "4");
    lisp.eval("(goto-char (search-forward \"gamma\" 0))".into());
    says(&shared, &lisp, "(point)", "11");

    // ...and the answer is a *character* offset even though `buffer-string`
    // comes back as UTF-8 bytes. Without the conversion this would be 7, and
    // `goto-char` would land in the middle of a codepoint.
    {
        let mut ed = shared.lock().unwrap();
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        let n = ed.buffer.len_chars();
        ed.apply(EditorCommand::DeleteRange(0, n));
        ed.apply(EditorCommand::InsertText("αβγ hello".into()));
        ed.apply(EditorCommand::SetMode(Mode::Normal));
    }
    says(&shared, &lisp, "(list (point-max) (search-forward \"hello\" 0))", "(9 4)");
    says(&shared, &lisp, "(search-forward \"γ\" 0)", "2");
    // A start offset is a character offset too, on the way in as well as out.
    says(&shared, &lisp, "(search-forward \"l\" 5)", "6");
    says(&shared, &lisp, "(search-backward \"β\" 9)", "1");
    lisp.eval("(goto-char (search-forward \"hello\" 0))".into());
    says(&shared, &lisp, "(buffer-substring (point) (+ (point) 5))", "hello");
    load(&shared);

    // --- save-excursion -------------------------------------------------
    //
    // A marker rather than an integer, so an edit *in front of* point inside
    // the body still puts the cursor back on the same character.
    lisp.eval("(goto-char 11)".into());
    lisp.eval(r#"(save-excursion (insert-at 0 "XY"))"#.into());
    says(&shared, &lisp, "(buffer-substring (point) (+ (point) 5))", "gamma");
    lisp.eval(r#"(replace-all "XY" "")"#.into());

    // --- editing --------------------------------------------------------
    //
    // `insert-at` is `replace-region` over an empty range: an insert that does
    // not move point and that no keystroke can land in the middle of.
    lisp.eval(r#"(insert-at 0 "0 ")"#.into());
    wait(&shared, "insert-at", |ed| {
        (ed.buffer.text.to_string() == "0 alpha\nbeta\ngamma\n").then_some(())
    });
    lisp.eval("(delete-line 1)".into());
    wait(&shared, "delete-line", |ed| {
        (ed.buffer.text.to_string() == "beta\ngamma\n").then_some(())
    });

    // --- replace-all is one edit ----------------------------------------
    //
    // The whole new text is built in Lisp and applied with a single
    // `replace-region`, so one `u` takes all of it back. N separate edits would
    // be N undo steps and N windows for a keystroke to land in.
    load(&shared);
    says(&shared, &lisp, "(replace-all \"a\" \"@\")", "5");
    wait(&shared, "replace-all", |ed| {
        (ed.buffer.text.to_string() == "@lph@\nbet@\ng@mm@\n").then_some(())
    });
    {
        let mut ed = shared.lock().unwrap();
        ed.apply(EditorCommand::Undo);
        assert_eq!(
            ed.buffer.text.to_string(),
            "alpha\nbeta\ngamma\n",
            "replace-all must be a single undo step"
        );
    }
    // A pattern that is not there changes nothing and says so.
    says(&shared, &lisp, "(replace-all \"zzz\" \"!\")", "0");

    // --- undo and redo from Lisp ----------------------------------------
    lisp.eval(r#"(progn (goto-char 0) (insert "hello "))"#.into());
    wait(&shared, "insert", |ed| {
        ed.buffer.text.to_string().starts_with("hello ").then_some(())
    });
    lisp.eval("(undo)".into());
    wait(&shared, "undo", |ed| {
        (ed.buffer.text.to_string() == "alpha\nbeta\ngamma\n").then_some(())
    });
    lisp.eval("(redo)".into());
    wait(&shared, "redo", |ed| {
        ed.buffer.text.to_string().starts_with("hello ").then_some(())
    });

    // --- the register ---------------------------------------------------
    load(&shared);
    lisp.eval("(progn (copy-region 0 5) (goto-char 5) (paste t))".into());
    wait(&shared, "copy-region then paste", |ed| {
        (ed.buffer.text.to_string() == "alphaalpha\nbeta\ngamma\n").then_some(())
    });
    // ...and one undo takes the paste back, so it was checkpointed.
    {
        let mut ed = shared.lock().unwrap();
        ed.apply(EditorCommand::Undo);
        assert_eq!(ed.buffer.text.to_string(), "alpha\nbeta\ngamma\n");
    }
    lisp.eval(r#"(progn (set-register "zz") (goto-char 0) (paste))"#.into());
    wait(&shared, "set-register then paste", |ed| {
        ed.buffer.text.to_string().starts_with("zzalpha").then_some(())
    });

    // --- buffers ---------------------------------------------------------
    //
    // `buffer-info` is the switcher's list with its fields apart, so a name can
    // be matched rather than parsed out of a display string.
    load(&shared);
    says(&shared, &lisp, "(first (buffer-names))", "zemacs_api_test.txt");
    says(
        &shared,
        &lisp,
        "(second (first (buffer-info)))",
        "/tmp/zemacs_api_test.txt",
    );
    says(&shared, &lisp, "(buffer-index \"*scratch*\")", "2");
    says(&shared, &lisp, "(switch-to-buffer \"nope\")", "NIL");
    wait_message(&shared, "the report of a missing buffer", |m| {
        m == "no such buffer: nope"
    });
    lisp.eval(r#"(switch-to-buffer "*scratch*")"#.into());
    wait(&shared, "the scratch buffer", |ed| {
        (ed.buffer.name() == "*scratch*").then_some(())
    });
    // ...and `with-current-buffer` puts it back afterwards.
    lisp.eval(r#"(switch-to-buffer "zemacs_api_test.txt")"#.into());
    wait(&shared, "back to the text buffer", |ed| {
        (ed.buffer.name() == "zemacs_api_test.txt").then_some(())
    });
    says(
        &shared,
        &lisp,
        r#"(list (with-current-buffer "*scratch*" (buffer-name)) (buffer-name))"#,
        "(*scratch* zemacs_api_test.txt)",
    );

    // --- windows and frames ----------------------------------------------
    says(&shared, &lisp, "(length (window-list))", "1");
    lisp.eval("(split-window-right)".into());
    wait(&shared, "the split", |ed| {
        (ed.frame().windows.len() == 2).then_some(())
    });
    // The split focuses the new window, so this is the id to come back to.
    says(&shared, &lisp, "(window-id)", "1");
    lisp.eval("(select-window 0)".into());
    says(&shared, &lisp, "(window-id)", "0");
    lisp.eval("(other-window)".into());
    says(&shared, &lisp, "(window-id)", "1");
    // A window lists the buffer it shows, which is what a status widget needs.
    says(&shared, &lisp, "(second (first (window-list)))", "zemacs_api_test.txt");
    lisp.eval("(delete-window)".into());
    wait(&shared, "the window closing", |ed| {
        (ed.frame().windows.len() == 1).then_some(())
    });

    lisp.eval("(new-frame)".into());
    wait(&shared, "the new frame", |ed| (ed.frames.len() == 2).then_some(()));
    says(&shared, &lisp, "(list (frame-count) (frame-index))", "(2 1)");
    lisp.eval("(select-frame 0)".into());
    says(&shared, &lisp, "(frame-index)", "0");
    // Closing one takes an OS window down with it, so unlike the rest of these
    // it goes to the app rather than being applied on the spot.
    lisp.eval("(delete-frame)".into());
    wait_for(&rx, &mut seen, "CloseFrame", |c| *c == EditorCommand::CloseFrame);

    // --- the backends core has none of ------------------------------------
    lisp.eval(r#"(magit "status")"#.into());
    wait_for(&rx, &mut seen, "Git(status)", |c| {
        *c == EditorCommand::Git("status".into())
    });
    lisp.eval(r#"(dired "open")"#.into());
    wait_for(&rx, &mut seen, "Dired(open)", |c| {
        *c == EditorCommand::Dired("open".into())
    });
    lisp.eval(r#"(terminal "open")"#.into());
    wait_for(&rx, &mut seen, "Term(open)", |c| {
        *c == EditorCommand::Term("open".into())
    });
    // The only way to open a file *and* land on a line: `find-file` reaches the
    // app a frame later, so a `goto-char` after it would move the cursor in the
    // buffer you were leaving.
    lisp.eval(r#"(find-file-at "/tmp/x.rs:12:fn main")"#.into());
    wait_for(&rx, &mut seen, "OpenAt", |c| {
        *c == EditorCommand::OpenAt("/tmp/x.rs:12:fn main".into())
    });

    // --- the editor's own pickers ----------------------------------------
    //
    // Reachable from Lisp, so a command can put one up. What the answer does is
    // fixed by the kind — there is no continuation, and therefore no
    // `read-string`.
    lisp.eval(r#"(open-prompt "buffer")"#.into());
    wait(&shared, "the buffer prompt", |ed| {
        ed.prompt
            .as_ref()
            .map(|p| p.kind == zemacs_core::PromptKind::Buffer)
            .unwrap_or(false)
            .then_some(())
    });
    lisp.eval(r#"(open-prompt "nope")"#.into());
    wait_message(&shared, "an unknown prompt", |m| m == "no such prompt: nope");
    {
        // Leave the prompt behind: the assertions below read the editor, and a
        // live prompt is UI state nothing else here expects.
        shared.lock().unwrap().prompt = None;
    }

    // A verb this build has never heard of is a message, not silence and not an
    // error: a config written against a newer zemacs still loads.
    lisp.eval(r#"(%do "no-such-verb" nil 0 0)"#.into());
    wait_message(&shared, "an unknown verb", |m| m == "no such command: no-such-verb");

    // --- introspection ----------------------------------------------------
    {
        let mut ed = shared.lock().unwrap();
        ed.apply(EditorCommand::BindKey {
            mode: "normal".into(),
            keys: "SPC f f".into(),
            command: "find-file".into(),
        });
        ed.apply(EditorCommand::BindKey {
            mode: "org-mode".into(),
            keys: "SPC m b".into(),
            command: "org-bold".into(),
        });
        ed.apply(EditorCommand::RegisterCommand("lisp-version".into()));
    }
    // Both keymaps in one list, and the mode column is the same vocabulary
    // `define-key` takes — so a row can be fed straight back to it.
    says(
        &shared,
        &lisp,
        "(where-is \"find-file\")",
        "((normal SPC f f find-file))",
    );
    says(&shared, &lisp, "(length (key-bindings))", "2");
    says(&shared, &lisp, "(command-list)", "(lisp-version)");
    says(&shared, &lisp, "(first (last (messages 1)))", "(lisp-version)");

    // --- settings read back ------------------------------------------------
    //
    // Every one of these had a writer and no reader, which is why the mode
    // machinery in `runtime/modes/` wraps the writers to remember what it asked
    // for. A value has to survive being read and written back.
    lisp.eval("(set-font-size 18)".into());
    says(&shared, &lisp, "(font-size)", "18.0");
    lisp.eval("(set-font-size (+ (font-size) 4))".into());
    wait(&shared, "font-size round trip", |ed| {
        (ed.settings.font_size == 22.0).then_some(())
    });
    lisp.eval("(set-line-overflow \"wrap\")".into());
    says(&shared, &lisp, "(line-overflow)", "wrap");
    lisp.eval("(set-tab-width 2)".into());
    says(&shared, &lisp, "(list (tab-width) (line-numbers-p))", "(2 T)");
    // A face is named by a string everywhere else in the API, and a query only
    // takes integers — the wrapper looks the name up in `face-list`.
    lisp.eval("(set-syntax-color \"keyword\" 0.5 0.25 0.0)".into());
    says(&shared, &lisp, "(syntax-color \"keyword\")", "(0.5 0.25 0.0)");
    says(&shared, &lisp, "(syntax-color \"no-such-face\")", "NIL");

    // --- redefinition is the advice mechanism ------------------------------
    //
    // This is Common Lisp, so wrapping a primitive is `fdefinition` and needs no
    // framework — which is exactly how `runtime/modes/modes.lisp` makes a global
    // setting revert when a mode is left.
    lisp.eval(
        r#"(let ((old (fdefinition 'set-tab-width)))
             (setf (fdefinition 'set-tab-width)
                   (lambda (n) (message (format nil "tab-width -> ~a" n))
                               (funcall old n))))"#
            .into(),
    );
    lisp.eval("(set-tab-width 7)".into());
    wait_message(&shared, "the advice", |m| m == "tab-width -> 7");
    wait(&shared, "the wrapped primitive still reaching core", |ed| {
        (ed.settings.tab_width == 7).then_some(())
    });
}
