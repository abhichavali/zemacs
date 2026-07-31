//! Headless proof of `runtime/modes/parinfer.lisp` — indentation driving the
//! closing parentheses, which is the inverse of the indenter in
//! `lisp-mode.lisp` and built on the same scanner.
//!
//! Every assertion is about the *buffer text*, because that is the only thing
//! the feature produces: one `replace-region` per command, and the whole
//! argument for the design is that there is never an intermediate state in
//! which the form has the wrong number of parentheses.
//!
//! Nothing here waits on a *state* that a previous step could already have
//! satisfied. Each command is followed into the image by a numbered marker
//! message, and the Lisp queue is FIFO, so seeing the marker means the command
//! has run and the assertion after it is about the buffer it produced.
//!
//! Deliberately a single `#[test]`, as in every file beside it: `cl_boot`
//! initialises a process-wide Lisp image, so there is exactly one `spawn` per
//! test binary and a new file is the only way to add one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Mode, Shared};

const PATIENCE: Duration = Duration::from_secs(20);
static NTH: AtomicUsize = AtomicUsize::new(0);

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

fn messages(shared: &Shared) -> Vec<String> {
    shared.lock().unwrap().messages.clone()
}

/// Evaluate `form`, then wait until it has actually run.
///
/// Answers every message the form produced, so an assertion about what a
/// command *said* looks only at what this call provoked and never at an
/// identical line from three steps ago.
fn run(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str) -> Vec<String> {
    let from = messages(shared).len();
    let tag = format!("#{}", NTH.fetch_add(1, Ordering::Relaxed));
    lisp.eval(format!("(progn {form} (message {tag:?}))"));
    wait(shared, form, |ed| {
        ed.messages.iter().any(|m| *m == tag).then_some(())
    });
    let mut said = messages(shared).split_off(from);
    said.pop(); // the marker itself
    said
}

/// Evaluate `form` and assert its value, printed with `~a`, is `want`.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    let said = run(shared, lisp, &format!("(message (format nil \"~a\" {form}))"));
    assert_eq!(said, vec![want.to_string()], "{form}");
}

fn text(shared: &Shared) -> String {
    shared.lock().unwrap().buffer.text.to_string()
}

fn runtime(file: &str) -> String {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime").join(file))
        .unwrap_or_else(|e| panic!("runtime/{file} must exist: {e}"))
        .display()
        .to_string()
}

/// Put `text` in the live buffer as a Lisp file, cursor at the start of 1-based
/// `line`.
///
/// A fresh path every time, for the reason `lisp_mode.rs` gives: `Editor::load`
/// switches to a buffer already holding that path rather than reloading it.
fn load(shared: &Shared, text: &str, line: usize) {
    let nth = NTH.fetch_add(1, Ordering::Relaxed);
    let at = text
        .split('\n')
        .take(line - 1)
        .map(|l| l.chars().count() + 1)
        .sum::<usize>();
    let mut ed = shared.lock().unwrap();
    ed.load(
        text,
        Some(PathBuf::from(format!("/tmp/zemacs_test_parinfer_{nth}.lisp"))),
        Some("lisp".into()),
    );
    ed.apply(EditorCommand::SetMode(Mode::Normal));
    ed.apply(EditorCommand::MoveTo(at));
}

/// Well-formed and well-indented: the fixed point every case below comes back
/// to, and the thing indent mode must never touch.
const TIDY: &str = "\
(defun f (x)
  (let ((a 1)
        (b 2))
    (list a b)))
";

/// Four parentheses that are not structure — in a string, in a `;' comment,
/// behind `#\\', and in a `#| |#' block — and one that is.
const TRICKY: &str = "\
(list \"a)b\"
  ;; a ) comment
  #\\)
  #| a ) block |#
  2)
";

#[test]
fn indentation_drives_the_closing_parentheses() {
    let init = std::env::temp_dir().join("zemacs_test_parinfer_init.lisp");
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(load {:?} :verbose nil :print nil)
(load {:?} :verbose nil :print nil)
(message "parinfer test init loaded")
"#,
            runtime("modes/modes.lisp"),
            runtime("modes/lisp-mode.lisp"),
            runtime("modes/parinfer.lisp"),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);

    wait(&shared, "the runtime files to load", |ed| {
        ed.messages.iter().any(|m| m == "parinfer test init loaded").then_some(())
    });
    let seen = messages(&shared);
    assert!(
        !seen.iter().any(|m| m.contains("error")),
        "parinfer.lisp must load cleanly; got {seen:#?}"
    );

    // A step is `tab-width` columns, which `library.lisp` claims as 2 for
    // `lisp-mode` — so entering the mode is what makes the shift commands below
    // mean "one level" rather than "four columns".
    load(&shared, TIDY, 1);
    run(&shared, &lisp, "(lisp-mode-hook)");
    says(&shared, &lisp, "(tab-width)", "2");

    // --- the fixed point -----------------------------------------------------
    //
    // The property that makes the command safe to press: code whose parentheses
    // already agree with its indentation is not rewritten at all, so there is no
    // undo step and no `after-change-hook` for a command that did nothing.
    let said = run(&shared, &lisp, "(parinfer-indent-buffer)");
    assert_eq!(said, vec!["parinfer: already agrees".to_string()]);
    assert_eq!(text(&shared), TIDY, "an indenter that reformats tidy code is an opinion");

    // --- dedenting closes ----------------------------------------------------
    //
    // The whole idea, in one edit: `(list a b)` has moved out to column 0, so
    // the three lists it used to be inside close on the line above it instead.
    load(
        &shared,
        "(defun f (x)\n  (let ((a 1)\n        (b 2))\n(list a b)))\n",
        1,
    );
    run(&shared, &lisp, "(parinfer-indent-buffer)");
    assert_eq!(
        text(&shared),
        "(defun f (x)\n  (let ((a 1)\n        (b 2))))\n(list a b)\n"
    );

    // --- shifting a line, and the parentheses following it -------------------
    //
    // This is what parinfer *is*, and the reason the two shift commands exist:
    // real parinfer runs on every keystroke, and here there is no hook to run
    // on, so asking for the indentation change and the paren move together in
    // one command is the honest substitute — and it is one `replace-region`,
    // so it is one undo step with no window in which the form is unbalanced.
    //
    // `2` moving right joins the `(list 1` above it, so the parenthesis that
    // used to close that list moves down past it.
    load(&shared, "(defun f ()\n  (list 1)\n  2)\n", 3);
    run(&shared, &lisp, "(parinfer-shift-right)");
    assert_eq!(text(&shared), "(defun f ()\n  (list 1\n    2))\n");

    // ...and back out again, which puts every parenthesis where it started.
    run(&shared, &lisp, "(parinfer-shift-left)");
    assert_eq!(text(&shared), "(defun f ()\n  (list 1)\n  2)\n");

    // A left shift at column 0 has nowhere to go, and says so rather than
    // rewriting the form anyway.
    load(&shared, "(defun f ()\n  (list 1)\n  2)\n", 1);
    let said = run(&shared, &lisp, "(parinfer-shift-left)");
    assert_eq!(said, vec!["parinfer: nothing to shift".to_string()]);
    assert_eq!(text(&shared), "(defun f ()\n  (list 1)\n  2)\n");

    // --- one form at a time --------------------------------------------------
    //
    // A top-level form is the right unit because its first character is at
    // depth zero by definition, so the scan can start with an empty stack and
    // be sure of it. The form point is *not* in is untouched.
    load(&shared, "(defun f ()\n(list 1))\n\n(defun g ()\n(list 2))\n", 2);
    run(&shared, &lisp, "(parinfer-indent-defun)");
    assert_eq!(
        text(&shared),
        "(defun f ())\n(list 1)\n\n(defun g ()\n(list 2))\n",
        "the second form is not in the span and did not move"
    );

    // --- what is not structure -----------------------------------------------
    //
    // The reason this is built on `lisp-mode.lisp` rather than on a counter: a
    // parenthesis in a string, in a comment or behind `#\` is punctuation, and
    // moving it would be moving somebody's text.
    load(&shared, TRICKY, 1);
    let said = run(&shared, &lisp, "(parinfer-indent-buffer)");
    assert_eq!(said, vec!["parinfer: already agrees".to_string()]);
    assert_eq!(text(&shared), TRICKY, "nothing inside a string or a comment moves");

    // A comment column survives, because the whitespace in front of a trailing
    // comment is held apart from the code the parentheses are appended to.
    load(&shared, "(list 1\n  2)      ; tail\n", 1);
    let said = run(&shared, &lisp, "(parinfer-indent-buffer)");
    assert_eq!(said, vec!["parinfer: already agrees".to_string()]);
    assert_eq!(text(&shared), "(list 1\n  2)      ; tail\n");

    // A line that is only a comment is neutral: it closes nothing, however far
    // left it sits, and takes no closing parenthesis of its own. If it were not,
    // the `;; done` at column 0 would have closed the list above it.
    load(&shared, "(list 1\n  2\n;; done\n  3)\n", 1);
    let said = run(&shared, &lisp, "(parinfer-indent-buffer)");
    assert_eq!(said, vec!["parinfer: already agrees".to_string()]);
    assert_eq!(text(&shared), "(list 1\n  2\n;; done\n  3)\n");

    // --- refusing ------------------------------------------------------------
    //
    // A closing parenthesis with nothing open means this scan and the reader
    // disagree about where the structure is. Moving parentheses on a
    // disagreement is how a structural editor eats a file, so it refuses.
    let broken = "(foo) bar) baz\n";
    load(&shared, broken, 1);
    let said = run(&shared, &lisp, "(parinfer-indent-buffer)");
    assert_eq!(
        said,
        vec!["parinfer: unbalanced parentheses — nothing moved".to_string()]
    );
    assert_eq!(text(&shared), broken, "a refusal changes nothing at all");
}
