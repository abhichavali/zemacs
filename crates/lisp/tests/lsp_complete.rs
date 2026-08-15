//! What a completion actually inserts, and when the list is asked for again.
//!
//! Two bugs with the same shape: the client had the server's answer in its hand
//! and used only part of it.
//!
//! * **`textEdit` was discarded.** A server says where the text it is offering
//!   goes, and that range regularly starts *earlier* than the word this client
//!   finds for itself — `%lsp-word-char-p` stops at a `.` and at a `:`, so
//!   `std::ve` is the word `ve` here and the whole qualified name to clangd.
//!   Replacing `[our anchor, point)` with the server's `newText` then leaves the
//!   `std::` behind and produces `std::std::vector`.
//! * **`isIncomplete` was ignored.** It means "this is not the whole answer";
//!   the client narrowed the truncated list instead of asking again, so the
//!   candidate you were typing towards never arrived however much you typed.
//!
//! Against the *decoded* shape rather than a live server: a decoded JSON object
//! is the alist `jobj` builds, so a `CompletionItem` can be written out here and
//! handed to the same functions a reply reaches. `crates/lisp/tests/lsp.rs` is
//! where the wire is proved; this is where the arithmetic is.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use zemacs_core::{Editor, Shared};

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

/// Evaluate FORM and wait for its printed value to appear in the message log.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"~a\" {form}))"));
    wait(shared, &format!("{form} to answer {want:?}"), |ed| {
        ed.messages.iter().any(|m| m == want).then_some(())
    });
}

fn runtime(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .expect("the runtime directory ships with the source")
}

#[test]
fn a_completion_lands_where_the_server_said_and_a_truncated_list_is_asked_again() {
    let init = std::env::temp_dir().join(format!(
        "zemacs_test_lsp_complete_init-{}.lisp",
        std::process::id()
    ));
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (message \"lsp complete test init loaded\")\n",
            runtime("modes/modes.lisp"),
            runtime("rpc.lisp"),
            runtime("lsp.lisp"),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init.clone());
    wait(&shared, "the init file", |ed| {
        ed.messages
            .iter()
            .any(|m| m == "lsp complete test init loaded")
            .then_some(())
    });

    // Two lines, so a position naming line 1 proves the line arithmetic rather
    // than passing on an offset that happens to be the column.
    // `~%` and not a `\n`: Common Lisp string escapes are `\\` and `\"` and
    // nothing else, so a backslash-n in source is the letter `n`.
    lisp.eval(
        "(progn (create-buffer \"probe.cc\")
                (insert (format nil \"int a;~%auto v = std::ve\")))"
            .into(),
    );
    wait(&shared, "the probe buffer", |ed| {
        ed.buffer.text.to_string().contains("std::ve").then_some(())
    });

    // --- the position arithmetic --------------------------------------------
    //
    // LSP counts lines from 0 and columns in UTF-16 units; the image counts
    // characters from 1. `int a;\n` is seven characters, so line 1 column 0 is
    // offset 7 and column 9 — the `s` of `std` — is offset 16.
    says(&shared, &lisp, "(%lsp-offset-at (jobj \"line\" 1 \"character\" 0))", "7");
    says(&shared, &lisp, "(%lsp-offset-at (jobj \"line\" 1 \"character\" 9))", "16");
    // A line the buffer has not got is NIL, not the last line: a reply about a
    // document that has moved on must edit nowhere rather than somewhere.
    says(&shared, &lisp, "(%lsp-offset-at (jobj \"line\" 99 \"character\" 0))", "NIL");

    // --- what the client makes of a `textEdit` ------------------------------
    //
    // clangd's shape: the range covers `std::ve`, and `newText` is the whole
    // qualified name. Our own word scan stops at the `:`, so the anchor this
    // client would compute is the `v` at offset 23 — six characters later than
    // where the server says the text goes.
    let item = r#"(jobj "label" "vector"
                        "insertText" "IGNORED"
                        "textEdit" (jobj "newText" "std::vector"
                                         "range" (jobj "start" (jobj "line" 1 "character" 9)
                                                       "end"   (jobj "line" 1 "character" 16))))"#;
    // `newText` wins over `insertText`, which is LSP's own precedence.
    says(&shared, &lisp, &format!("(first (%lsp-completion-row {item}))"), "std::vector");
    // ...and the start comes back as a buffer offset, which is the whole fix.
    says(&shared, &lisp, &format!("(fifth (%lsp-completion-row {item}))"), "16");
    says(&shared, &lisp, &format!("(%lsp-edit-range {item})"), "16");
    // An item with no edit answers NIL and leaves the caller on its own anchor.
    says(
        &shared,
        &lisp,
        "(%lsp-edit-range (jobj \"label\" \"plain\"))",
        "NIL",
    );
    // `insertReplaceEdit`'s `insert` range is taken; `replace` would eat the
    // identifier to the right of point, which nobody asked for by pressing RET.
    says(
        &shared,
        &lisp,
        "(%lsp-edit-range (jobj \"textEdit\"
             (jobj \"newText\" \"x\"
                   \"insert\" (jobj \"start\" (jobj \"line\" 1 \"character\" 9)))))",
        "16",
    );

    // --- and what accepting one does to the buffer --------------------------
    //
    // The popup's anchor is the editor's, so this is the real path: show a
    // completion at the word this client found, hand the candidate the server's
    // earlier start, and accept.
    // 21 is the anchor *this client* computes: `%lsp-word-char-p` stops at the
    // `:`, so the word it finds is `ve`. 16 is where the server said the text
    // goes. Without the fix the replacement starts at 21 and the buffer ends up
    // reading `std:std::vector`.
    // Insert mode, because `Editor::completion` refuses to answer outside it —
    // a popup is a thing that exists while you are typing, and that gate is
    // what takes it down when you leave.
    lisp.eval(
        "(progn (set-evil-state \"insert\")
                (goto-char (point-max))
                (setf *lsp-completion*
                      (list :at 21 :path (buffer-file-name)
                            :items nil :index 0
                            :shown (list (list \"std::vector\" \"vector\" \"row\" nil 16))))
                (completion-show 21 0)
                (completion-row \"row\")
                (lsp-complete-accept))"
            .into(),
    );
    let text = wait(&shared, "the accepted completion", |ed| {
        let t = ed.buffer.text.to_string();
        (!t.contains("std::ve\n") && !t.ends_with("std::ve")).then_some(t)
    });
    assert_eq!(
        text, "int a;\nauto v = std::vector",
        "the server's start was ignored and the qualifier was left behind"
    );

    // --- isIncomplete -------------------------------------------------------
    //
    // The flag is read off a `CompletionList` and not off a bare array, which
    // has no way to carry one and is complete by definition.
    says(
        &shared,
        &lisp,
        "(if (jget (jobj \"isIncomplete\" t \"items\" nil) \"isIncomplete\") \"yes\" \"no\")",
        "yes",
    );
    // Set it, and the narrowing shortcut has to decline: with a truncated list
    // in hand, typing another character must go back to the server rather than
    // filter what is left of an answer that was cut off.
    lisp.eval(
        "(progn (setf *lsp-completion* (list :at 23 :path (buffer-file-name)
                                             :items nil :shown nil :index 0
                                             :incomplete t))
                (message (format nil \"incomplete is ~a\"
                                 (if (getf *lsp-completion* :incomplete) \"honoured\" \"lost\"))))"
            .into(),
    );
    wait(&shared, "the incomplete flag", |ed| {
        ed.messages
            .iter()
            .any(|m| m == "incomplete is honoured")
            .then_some(())
    });

    let _ = std::fs::remove_file(&init);
}
