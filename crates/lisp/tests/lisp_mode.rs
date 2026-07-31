//! Headless proof of `runtime/modes/which-key.lisp`, `lisp-mode.lisp` and
//! `repl.lisp` — which-key, structural Lisp editing, Common Lisp indentation,
//! and a REPL that evaluates in this very image.
//!
//! All three are Common Lisp on top of the primitives the editor exports, so
//! the only honest way to check them is to boot the real image against a real
//! [`Editor`] and watch the buffer move — which is what this does.
//!
//! Deliberately a single `#[test]`, for the same reason `modes.rs` is: `cl_boot`
//! initialises a process-wide Lisp image, so there is exactly one `spawn` per
//! test binary.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Mode, Shared};

const PATIENCE: Duration = Duration::from_secs(20);

/// Poll the shared editor until `f` answers, or give up. Everything the image
/// does is asynchronous, so every assertion about editor state is a wait.
fn wait<T>(shared: &Shared, what: &str, f: impl Fn(&Editor) -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(v) = f(&shared.lock().unwrap()) {
            return v;
        }
        if Instant::now() >= deadline {
            let ed = shared.lock().unwrap();
            let seen = &ed.messages[ed.messages.len().saturating_sub(8)..];
            panic!(
                "timed out waiting for {what}; status={:?} text={:?} last={seen:#?}",
                ed.status,
                ed.buffer.text.to_string(),
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_message(shared: &Shared, what: &str, pred: impl Fn(&str) -> bool) {
    wait(shared, what, |ed| {
        ed.messages.iter().any(|m| pred(m)).then_some(())
    });
}

fn messages_since(shared: &Shared, from: usize) -> Vec<String> {
    shared.lock().unwrap().messages[from..].to_vec()
}

/// Put `text` in the live buffer as a Lisp file, cursor at `at`.
///
/// A fresh path every time: `Editor::load` switches to a buffer already holding
/// that path rather than reloading it, which is right for `find-file` and
/// wrong for a fixture.
fn load(shared: &Shared, text: &str, at: usize) {
    static NTH: AtomicUsize = AtomicUsize::new(0);
    let nth = NTH.fetch_add(1, Ordering::Relaxed);
    let mut ed = shared.lock().unwrap();
    ed.load(
        text,
        Some(PathBuf::from(format!("/tmp/zemacs_test_{nth}.lisp"))),
        Some("lisp".into()),
    );
    ed.apply(EditorCommand::SetMode(Mode::Normal));
    ed.apply(EditorCommand::MoveTo(at));
}

fn runtime(file: &str) -> String {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime").join(file))
        .unwrap_or_else(|e| panic!("runtime/{file} must exist: {e}"))
        .display()
        .to_string()
}

#[test]
fn lisp_editing_and_the_repl_are_built_in_lisp() {
    // The transcript goes somewhere disposable rather than into the user's
    // config directory — the REPL writes to the *file* whenever its buffer is
    // not open, which headless it never is, and that is exactly what lets this
    // test read back what the REPL wrote.
    let transcript = std::env::temp_dir().join("zemacs_test_repl.lisp");
    let _ = std::fs::remove_file(&transcript);

    let init = std::env::temp_dir().join("zemacs_test_lisp_mode_init.lisp");
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(load {:?} :verbose nil :print nil)
(load {:?} :verbose nil :print nil)
(load {:?} :verbose nil :print nil)

(setf *repl-file* (pathname {:?}))

;; A leader to ask which-key about, and a command to annotate.
(define-key "normal" "SPC f f" "find-file")
(define-key "normal" "SPC f s" "save-file")
(define-key "normal" "SPC q q" "quit")
(define-key "normal" "SPC x d" "demo-command")
(defun demo-command () "Do the demo thing." nil)
(register-command "demo-command")

(message "lisp mode test init loaded")
"#,
            runtime("modes/modes.lisp"),
            runtime("modes/which-key.lisp"),
            runtime("modes/lisp-mode.lisp"),
            runtime("modes/repl.lisp"),
            transcript.display().to_string(),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);

    wait_message(&shared, "the runtime files to load", |m| {
        m == "lisp mode test init loaded"
    });
    let tail = messages_since(&shared, 0);
    assert!(
        !tail.iter().any(|m| m.contains("error")),
        "runtime/modes must load cleanly; got {tail:#?}"
    );

    // --- which-key -----------------------------------------------------------
    //
    // A prefix that leads only to more keys is a named group; one key deeper,
    // the rows are the commands themselves. The editor calls this with whatever
    // sequence is pending; here it is called by hand, which is the same thing.
    //
    // Normal state first: which-key answers for where you *are*, and a fresh
    // editor is on the dashboard, where a leader binding does not apply.
    shared.lock().unwrap().apply(EditorCommand::SetMode(Mode::Normal));
    lisp.eval(r#"(which-key "SPC")"#.into());
    let line = wait(&shared, "which-key to describe SPC", |ed| {
        ed.status.starts_with("SPC-").then(|| ed.status.clone())
    });
    assert!(
        line.contains("f +file") && line.contains("q +quit"),
        "which-key must name the groups: {line}"
    );
    // ...and pressing the prefix is what actually calls it. This is the one
    // thing Lisp cannot see for itself, so it is worth going through the same
    // two steps the app does: core answers a pending prefix with a `CallLisp`,
    // and `dispatch` hands that to the image.
    let pending = shared.lock().unwrap().handle_key(zemacs_core::Key::Char(' '));
    let form = pending
        .iter()
        .find_map(|c| match c {
            EditorCommand::CallLisp(src) => Some(src.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("a pending prefix must reach the image; got {pending:#?}"));
    lisp.eval(form);
    let line = wait(&shared, "which-key to describe the pending SPC", |ed| {
        ed.status.starts_with("SPC-").then(|| ed.status.clone())
    });
    assert!(line.contains("f +file"), "pressing SPC put which-key up: {line}");

    lisp.eval(r#"(which-key "SPC f")"#.into());
    let line = wait(&shared, "which-key to describe SPC f", |ed| {
        ed.status.starts_with("SPC f-").then(|| ed.status.clone())
    });
    assert!(
        line.contains("f find-file") && line.contains("s save-file"),
        "one key deeper the rows are commands: {line}"
    );

    // --- M-x annotations -----------------------------------------------------
    //
    // The candidate carries its docstring and its key inside a Lisp block
    // comment, which is why core needs to know nothing about a second column:
    // `(demo-command #| ... |#)` is still a call to `demo-command`.
    let candidate = wait(&shared, "an annotated M-x candidate", |ed| {
        ed.commands.iter().find(|c| c.starts_with("demo-command")).cloned()
    });
    assert!(
        candidate.contains("#| Do the demo thing.   SPC x d |#"),
        "the annotation is doc then key: {candidate:?}"
    );

    // --- structure -----------------------------------------------------------
    load(&shared, "(foo (bar baz) qux)", 5);
    lisp.eval("(lisp-kill-sexp)".into());
    wait(&shared, "kill-sexp to take the list out", |ed| {
        (ed.buffer.text.to_string() == "(foo  qux)").then_some(())
    });

    load(&shared, "(foo bar) baz", 1);
    lisp.eval("(lisp-slurp-forward)".into());
    wait(&shared, "slurp to swallow the next form", |ed| {
        (ed.buffer.text.to_string() == "(foo bar baz)").then_some(())
    });
    lisp.eval("(lisp-barf-forward)".into());
    wait(&shared, "barf to spit it back out", |ed| {
        (ed.buffer.text.to_string() == "(foo bar) baz").then_some(())
    });

    load(&shared, "(alpha (beta) gamma)", 0);
    lisp.eval("(lisp-match-paren)".into());
    wait(&shared, "match-paren to reach the closing paren", |ed| {
        (ed.buffer.cursor == 19).then_some(())
    });

    // --- indentation ---------------------------------------------------------
    //
    // The whole point of the mode: `(let ((a 1)` is a binding list and lines up
    // one in, `(when a` takes a body and indents two, `(list 1` is a call and
    // lines up under its first argument. C convention gets all three wrong.
    load(
        &shared,
        "(defun f (x)\n(let ((a 1)\n(b 2))\n(when a\n(list 1\n2))))\n",
        0,
    );
    lisp.eval("(lisp-indent-buffer)".into());
    wait(&shared, "common lisp indentation", |ed| {
        (ed.buffer.text.to_string()
            == "(defun f (x)\n  (let ((a 1)\n        (b 2))\n    (when a\n      (list 1\n            2))))\n")
            .then_some(())
    });

    // --- the REPL ------------------------------------------------------------
    //
    // Evaluated in *this* image: the value comes back through the same status
    // line every other command uses, and the form and its value are appended to
    // the transcript.
    lisp.eval(r#"(%repl-eval "(+ 1 2)")"#.into());
    wait(&shared, "the REPL to answer", |ed| (ed.status == "3").then_some(()));

    // A form that signals is contained rather than fatal, and the image is
    // still there afterwards — which is the whole argument for `handler-case`
    // around an evaluator that shares its image with the editor.
    lisp.eval(r#"(%repl-eval "(zemacs-no-such-function)")"#.into());
    wait_message(&shared, "the error to be reported", |m| {
        m.starts_with("lisp error:")
    });
    lisp.eval(r#"(message "the image survived")"#.into());
    wait_message(&shared, "the image to still be answering", |m| {
        m == "the image survived"
    });

    // Every line the REPL adds is a comment, so the transcript is still Lisp —
    // load it and the session replays.
    let text = std::fs::read_to_string(&transcript).expect("the REPL must write a transcript");
    assert!(
        text.contains("(+ 1 2)") && text.contains(";=> 3") && text.contains(";!! "),
        "transcript should hold the form, its value and the error: {text}"
    );
    for line in text.lines() {
        assert!(
            !line.starts_with(";=>") || line.starts_with(";=> "),
            "a result line is a comment: {line}"
        );
    }

    let _ = std::fs::remove_file(&transcript);
}
