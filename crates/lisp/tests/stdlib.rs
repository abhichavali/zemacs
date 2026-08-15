//! The four helpers that stopped being written out per file.
//!
//! `add-hook` is in `LIBRARY_FORM` in `crates/lisp/src/shim.c` and comes up with
//! the image; `split-string`, `executable-find` and `run-process` are in
//! `runtime/modes/modes.lisp`, the file every mode already loads. Between them
//! they replace two copies of a string splitter, two copies of a
//! child-process-with-a-deadline loop, an `executable-find` reached through an
//! `fboundp` guard from two files, and a defensive `(defvar *some-hook* nil)`
//! in six.
//!
//! What is worth proving is what each of them promises the callers that gave up
//! their own copy. For `add-hook` that is *binding a list nobody has declared* —
//! the whole reason `init.lisp` can register a poller two hundred lines above
//! the `load` of the file that declares the list — and not stacking a duplicate
//! on a config reload. For `run-process` it is the three things the loop does at
//! once: read everything a chatty child says without deadlocking on a full
//! pipe, feed it stdin and close the pipe so a reader waiting on end-of-file
//! gets one, and come back at the deadline when the child never will.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::time::{Duration, Instant};

use zemacs_core::{Editor, Shared};

const PATIENCE: Duration = Duration::from_secs(30);

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

/// Evaluate FORM and wait for its printed value to appear in the status line.
///
/// The whole history is scanned rather than the last line, so the assertions
/// below are independent of anything else the image happens to say.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"~a\" {form}))"));
    wait(shared, &format!("{form} to answer {want:?}"), |ed| {
        ed.messages.iter().any(|m| m == want).then_some(())
    });
}

fn runtime(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .expect("the runtime directory ships with the source")
}

#[test]
fn the_shared_helpers_do_what_the_copies_they_replaced_did() {
    let init = std::env::temp_dir()
        .join(format!("zemacs_test_stdlib_init-{}.lisp", std::process::id()));
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             ;; `add-hook' is asked about *before* this load, which is the\n\
             ;; property the six deleted DEFVARs were standing in for.\n\
             (add-hook '*stdlib-test-hook* 'car)\n\
             (load {:?} :verbose nil :print nil)\n\
             (message \"stdlib test init loaded\")\n",
            runtime("modes/modes.lisp"),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init.clone());
    wait_message(&shared, "the init file", |m| m == "stdlib test init loaded");

    // --- add-hook ------------------------------------------------------------
    //
    // The list was unbound when the init file above pushed onto it, and it is a
    // one-element list now. That is the case every registration site used to
    // open with a DEFVAR for.
    says(&shared, &lisp, "*stdlib-test-hook*", "(CAR)");

    // Twice is once: a config reload re-reads every file, and a list walked on
    // every keystroke must not grow a duplicate each time.
    lisp.eval("(add-hook '*stdlib-test-hook* 'car)".into());
    says(&shared, &lisp, "*stdlib-test-hook*", "(CAR)");

    // A second function goes on the front, so the most recently added runs
    // first — Emacs' order, and the one PUSHNEW already had.
    lisp.eval("(add-hook '*stdlib-test-hook* 'cdr)".into());
    says(&shared, &lisp, "*stdlib-test-hook*", "(CDR CAR)");

    // And it answers the function, so a registration can be written inline.
    says(&shared, &lisp, "(add-hook '*stdlib-test-hook* 'cdr)", "CDR");

    // The two hooks the editor actually reports are declared by `modes.lisp`
    // and are both dispatchers over a list — this is the duplication that used
    // to live in `lsp.lisp` and again in `org-modern.lisp`.
    says(
        &shared,
        &lisp,
        "(and (boundp '*after-change-functions*) \
              (boundp '*point-moved-functions*) \
              (fboundp 'after-change-hook) \
              (fboundp 'point-moved-hook))",
        "T",
    );

    // --- split-string --------------------------------------------------------
    //
    // Empty fields are kept, which is the contract `executable-find` relies on
    // to notice an empty $PATH entry and the doc wrapper relies on to keep a
    // blank line between two paragraphs.
    says(&shared, &lisp, r#"(split-string "a:b::c" #\:)"#, "(a b  c)");
    says(&shared, &lisp, r#"(length (split-string "" #\:))"#, "1");
    says(&shared, &lisp, r#"(length (split-string ":" #\:))"#, "2");

    // --- executable-find -----------------------------------------------------
    //
    // `/bin/sh` is the one program a POSIX machine is allowed to be assumed to
    // have, and it is what `run-process` is exercised with below.
    says(&shared, &lisp, r#"(if (executable-find "sh") "y" "n")"#, "y");
    // A name with a separator in it is a path, not a $PATH lookup.
    says(&shared, &lisp, r#"(if (executable-find "/bin/sh") "y" "n")"#, "y");
    says(
        &shared,
        &lisp,
        r#"(if (executable-find "zemacs-no-such-program-4b71") "y" "n")"#,
        "n",
    );
    // A *directory* on $PATH called `sh` would answer here without the
    // `pathname-name` test, which is the bug the comment beside it records.
    says(&shared, &lisp, r#"(if (executable-find "/bin") "y" "n")"#, "n");

    // --- run-process ---------------------------------------------------------
    //
    // Exit status, stdout, and stderr merged into it.
    says(
        &shared,
        &lisp,
        r#"(multiple-value-bind (out status) (run-process "sh" (list "-c" "echo hi"))
             (format nil "~a ~a" status (string-trim '(#\Newline) out)))"#,
        "EXITED hi",
    );
    says(
        &shared,
        &lisp,
        r#"(string-trim '(#\Newline) (run-process "sh" (list "-c" "echo oops 1>&2")))"#,
        "oops",
    );

    // Everything the child said, not the first pipe-full of it. A child that
    // writes more than a pipe buffer blocks in `write` until somebody drains
    // it, and a runner that waited for the exit before reading would deadlock
    // here rather than fail — which is why this number is well past 64K.
    says(
        &shared,
        &lisp,
        r#"(length (run-process "sh" (list "-c" "i=0; while [ $i -lt 4000 ]; do \
             echo 0123456789012345678901234567890123456789; i=$((i+1)); done")))"#,
        "164000",
    );

    // stdin is written and the pipe is *closed*, so a child reading to
    // end-of-file gets one. Without the close this call never returns.
    says(
        &shared,
        &lisp,
        r#"(string-trim '(#\Newline) (run-process "sh" (list "-c" "cat") :stdin "from lisp"))"#,
        "from lisp",
    );

    // The deadline, which is the whole reason this is not `:wait t`: ECL has no
    // timeout of its own. The child is SIGKILLed and the status says so.
    says(
        &shared,
        &lisp,
        r#"(multiple-value-bind (out status)
               (run-process "sh" (list "-c" "sleep 30") :timeout 1)
             (declare (ignore out))
             status)"#,
        "TIMEOUT",
    );

    // A program that is not there comes back as an ordinary answer rather than
    // as an error escaping into whichever hook called it. ECL forks first and
    // the *child* fails to exec, so this is an :EXITED with the failure on the
    // merged stream — which is exactly what `%tutor-check-child` reads it as,
    // and why that function treats "exited without a verdict" as :broken.
    says(
        &shared,
        &lisp,
        r#"(multiple-value-bind (out status)
               (run-process "zemacs-no-such-program-4b71" nil)
             (format nil "~a ~a" status (plusp (length out))))"#,
        "EXITED T",
    );

    let _ = std::fs::remove_file(&init);
}
