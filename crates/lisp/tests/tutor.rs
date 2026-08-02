//! Headless proof of `runtime/modes/tutor.lisp` — the two-pane tutorial.
//!
//! Four claims, and none can be checked without running the real image against
//! a real [`Editor`]:
//!
//! 1. the file **loads out of the shipped config** and the lesson data is
//!    well-shaped — every lesson has org prose, every exercise a prompt, a
//!    template and a check form;
//! 2. it opens as **two panes** — an `org-mode` instruction buffer on the left
//!    and a `lisp-mode` answer buffer on the right — **at the table of
//!    contents**, every time, whatever progress says;
//! 3. **navigation moves both panes**: `RET` on a contents link lands in the
//!    lesson and fills the answer pane with its first exercise, and next and
//!    previous step them together;
//! 4. a **right answer passes and a wrong one does not**, and the two stages
//!    mark answers in different places — Stage 1 in a child `ecl` with a
//!    stopwatch, so `(loop)` is a failed exercise rather than a hung image, and
//!    Stage 2 in this image, so `(set-font-size 30)` moves `Editor::settings`
//!    and the check can see that it did.
//!
//! The instruction pane being *org* rather than a bespoke buffer is the point
//! of the whole layout, so this file asserts on the major mode and on the org
//! schema — `[[id:...]]` links and property drawers — rather than on any
//! rendering of its own.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide Lisp
//! image, so there is exactly one `spawn` per test binary.

use std::path::Path;
use std::sync::Arc;
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
            let seen = &ed.messages[ed.messages.len().saturating_sub(10)..];
            panic!("timed out waiting for {what}; status={:?} last={seen:#?}", ed.status);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_message(shared: &Shared, what: &str, pred: impl Fn(&str) -> bool) {
    wait(shared, what, |ed| ed.messages.iter().any(|m| pred(m)).then_some(()));
}

/// Stand in for the application's main loop: hand every pending mode hook to
/// the image, guarded by `fboundp` so a mode with no hook is silence. Copied
/// from `crates/app/src/main.rs`, exactly as `modes.rs` does — and needed here
/// because the instruction pane's whole appearance comes from the *org-mode
/// body*, which nothing but that dispatch ever runs.
fn pump(shared: Shared, lisp: Arc<zemacs_lisp::Lisp>) {
    std::thread::spawn(move || loop {
        let hooks = std::mem::take(&mut shared.lock().unwrap().pending_hooks);
        for hook in hooks {
            lisp.eval(format!(
                "(let ((h (find-symbol {:?} :zemacs))) (when (and h (fboundp h)) (funcall h)))",
                hook.to_uppercase()
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    });
}

/// Evaluate `form` and wait for its value, printed with `~a`, to be `want`.
///
/// Tagged, unlike the same helper in `overlays.rs`: this file asks several
/// questions whose answer is `T`, and an untagged `wait_message` would be
/// satisfied by an *earlier* one and assert nothing.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, tag: &str, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"{tag}= ~a\" {form}))"));
    let want = format!("{tag}= {want}");
    wait_message(shared, &format!("{tag}: {form}"), |m| m == want);
}

/// Put `text` in the answer pane and check it, which is what a student does:
/// type on the right, press `SPC m c`.
fn answer(lisp: &zemacs_lisp::Lisp, text: &str) {
    lisp.eval(format!("(progn (%tutor-write-answer {text:?}) (tutor-check))"));
}

/// The instruction buffer's whole text, as a Lisp expression.
const LESSON_TEXT: &str = "(with-current-buffer \"*tutor*\" (buffer-string))";

#[test]
fn the_tutor_opens_two_panes_at_the_contents_and_marks_what_you_write() {
    // The *shipped* config, not a synthetic one. The tutor is loaded out of the
    // `dolist` at the foot of `init.lisp`, its instruction pane is drawn by
    // `org-modern.lisp`, its links are followed by `org-open-at-point` and its
    // checker is `%eval-source` from `repl.lisp` — none of which a hand-written
    // init file would exercise.
    let entry =
        std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp"))
            .expect("runtime/init.lisp must exist");

    // Somewhere that is not the developer's own `~/.config/zemacs`: this test
    // records pass marks, and doing that over somebody's real progress would be
    // a rude way to run a test suite.
    let progress = std::env::temp_dir().join("zemacs_test_tutor_progress.lisp");
    let scratch = std::env::temp_dir().join("zemacs_test_tutor_check.lisp");
    let _ = std::fs::remove_file(&progress);

    let init = std::env::temp_dir().join("zemacs_test_tutor_init.lisp");
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(setf *tutor-progress-file* {:?})
(setf *tutor-check-file* {:?})
;; Short, so the runaway-answer assertion below does not spend eight seconds
;; proving something three prove just as well.
(setf *tutor-timeout* 3)
(message "tutor test init loaded")
"#,
            entry.display().to_string(),
            progress.display().to_string(),
            scratch.display().to_string(),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = Arc::new(zemacs_lisp::spawn(tx, shared.clone(), init));
    pump(shared.clone(), lisp.clone());
    wait_message(&shared, "tutor.lisp to load", |m| m == "tutor test init loaded");

    // --- claim 1: the data is there and is the right shape ------------------
    //
    // Shape rather than counts. Adding a lesson is meant to be adding a list
    // entry, and a test that asserted "there are exactly 24" would make the one
    // operation the design is for into a test failure.
    says(&shared, &lisp, "stages", "(length *tutor-stages*)", "3");
    says(
        &shared,
        &lisp,
        "ids",
        "(mapcar (lambda (s) (getf s :id)) *tutor-stages*)",
        "(cl api todo)",
    );
    says(&shared, &lisp, "many", "(> (length (%tutor-path)) 20)", "T");
    says(
        &shared,
        &lisp,
        "lessons-ok",
        "(loop for p in (%tutor-path)
               always (and (stringp (getf (cdr p) :id))
                           (stringp (getf (cdr p) :title))
                           (stringp (getf (cdr p) :prose))))",
        "T",
    );
    says(
        &shared,
        &lisp,
        "exercises-ok",
        "(loop for p in (%tutor-path)
               always (loop for e in (getf (cdr p) :exercises)
                            always (and (stringp (getf e :prompt))
                                        (stringp (getf e :template))
                                        (getf e :check))))",
        "T",
    );

    // --- claim 2: two panes, org on the left, at the contents ---------------
    lisp.eval("(zemacs-tutor)".into());
    says(&shared, &lisp, "panes", "(length (window-list))", "2");
    says(&shared, &lisp, "left", "(and (%tutor-window \"*tutor*\") t)", "T");
    says(&shared, &lisp, "right", "(and (%tutor-window \"*tutor-lisp*\") t)", "T");
    // Focus is on the instruction side, which is where you read from.
    says(&shared, &lisp, "focus", "(buffer-name)", "*tutor*");

    // The pane is *org-mode*, not a bespoke mode of the tutor's own — that is
    // the whole presentation, and everything that makes the lesson look right
    // hangs off it.
    wait(&shared, "the instruction pane to be an org buffer", |ed| {
        (ed.buffer.major_mode == "org-mode").then_some(())
    });
    says(&shared, &lisp, "minor", "(and (minor-mode-p 'tutor-lesson) t)", "T");
    says(&shared, &lisp, "declared", "(and (%tutor-buffer-p) t)", "T");

    // ...and the document is ordinary org, with the schema in property drawers
    // and the contents in `[[id:...]]' links that `org-open-at-point' follows.
    says(
        &shared,
        &lisp,
        "org-links",
        &format!("(and (search \"[[id:tutor-cl-reader]\" {LESSON_TEXT}) t)"),
        "T",
    );
    says(
        &shared,
        &lisp,
        "org-drawer",
        &format!("(and (search \":ZEMACS_TUTOR_EXERCISE: cl reader 0\" {LESSON_TEXT}) t)"),
        "T",
    );
    says(&shared, &lisp, "lessons-found", "(length (%tutor-tagged *tutor-lesson-property*))", "24");

    // Opened *at the contents*, and with nothing selected — which is the point
    // of opening there rather than where you left off.
    says(
        &shared,
        &lisp,
        "at-contents",
        "(let ((s (%tutor-contents-span))) (and s (= (point) (car s)) t))",
        "T",
    );
    says(&shared, &lisp, "unselected", "(null *tutor-current*)", "T");

    // --- claim 3: navigation moves both panes -------------------------------
    //
    // The real gesture: put point on a contents link and press RET. Following
    // it is `org-open-at-point' unmodified; what the tutor adds is the answer
    // pane filling with the exercise you landed on.
    lisp.eval(
        "(let ((at (search-forward \"[[id:tutor-cl-reader\" 0)))
           (when at (goto-char at) (tutor-open-at-point)))"
            .into(),
    );
    says(&shared, &lisp, "landed", "(%tutor-lesson-at-point)", "(cl reader)");
    says(&shared, &lisp, "selected", "*tutor-current*", "(cl reader 0)");
    says(
        &shared,
        &lisp,
        "answer-pane",
        "(and (search \"read-from-string\" (%tutor-read-answer)) t)",
        "T",
    );
    // The right-hand pane really is a Lisp buffer, so it has the indenter and
    // the REPL keys as well as the tutor's own.
    says(
        &shared,
        &lisp,
        "answer-mode",
        "(with-current-buffer \"*tutor-lisp*\"
           (list (major-mode) (and (minor-mode-p 'tutor-answer) t)))",
        "(lisp-mode T)",
    );

    // Next and previous step the selection, the point in the lesson, and the
    // template on the right, together.
    lisp.eval("(tutor-next-exercise)".into());
    says(&shared, &lisp, "next", "*tutor-current*", "(cl reader 1)");
    says(
        &shared,
        &lisp,
        "next-template",
        "(and (search \"(1 2 3)\" (%tutor-read-answer)) t)",
        "T",
    );
    says(
        &shared,
        &lisp,
        "next-point",
        "(and (search \"Exercise 2\" (line-string)) t)",
        "T",
    );
    lisp.eval("(tutor-previous-exercise)".into());
    says(&shared, &lisp, "prev", "*tutor-current*", "(cl reader 0)");

    // --- claim 4: a right answer passes, a wrong one does not ---------------
    //
    // Nothing here matches source text: the check is `(eql value 3)', so any
    // route to 3 passes and the shipped template — which answers the list
    // itself — does not.
    answer(&lisp, "(length (read-from-string \"(+ 1 2)\"))");
    wait_message(&shared, "the right answer to pass", |m| {
        m.contains("exercise 1 passed")
    });
    says(
        &shared,
        &lisp,
        "marked",
        "(and (member (list \"cl\" \"reader\" 0) *tutor-passed* :test #'equal) t)",
        "T",
    );
    // ...and the lesson says so, on the heading, in one rewritten line.
    says(
        &shared,
        &lisp,
        "ticked",
        &format!("(and (search \"Exercise 1 ✓\" {LESSON_TEXT}) t)"),
        "T",
    );
    // The contents carries the count, rebuilt when you go back to it.
    lisp.eval("(tutor-contents)".into());
    says(
        &shared,
        &lisp,
        "toc-count",
        &format!("(and (search \"]] — 1/3\" {LESSON_TEXT}) t)"),
        "T",
    );

    lisp.eval("(%tutor-select (list \"cl\" \"reader\" 1))".into());
    answer(&lisp, "(second (read-from-string \"(+ 1 2)\"))");
    wait_message(&shared, "a wrong answer to be refused", |m| {
        m.contains("exercise 2") && m.contains("not the answer yet")
    });

    // An answer that signals is a third outcome, and reads as one.
    lisp.eval("(%tutor-select (list \"cl\" \"reader\" 2))".into());
    answer(&lisp, "(this-function-does-not-exist)");
    wait_message(&shared, "a signalling answer to be reported", |m| {
        m.contains("exercise 3") && m.contains("signalled")
    });

    // --- progress survives being written out and read back ------------------
    says(&shared, &lisp, "saved", "(and (probe-file *tutor-progress-file*) t)", "T");
    says(
        &shared,
        &lisp,
        "roundtrip",
        "(progn (setf *tutor-passed* nil *tutor-answers* nil)
                (tutor-load-progress)
                (and (member (list \"cl\" \"reader\" 0) *tutor-passed* :test #'equal) t))",
        "T",
    );
    // ...including what you had typed, so a restart does not lose your working.
    says(
        &shared,
        &lisp,
        "kept",
        "(and (search \"read-from-string\"
                      (cdr (assoc (list \"cl\" \"reader\" 0) *tutor-answers* :test #'equal)))
              t)",
        "T",
    );

    // --- Stage 1 is marked somewhere else -----------------------------------
    //
    // The only assertion in this file that distinguishes the child process from
    // the in-image fallback, and the whole reason the child exists: a student's
    // `(loop)` is an exercise they got wrong, not an editor they have to
    // restart. Skipped where `ecl` is not installed, because there the tutor
    // degrades to in-image checking on purpose and this answer would hang it.
    if std::process::Command::new("ecl").arg("--version").output().is_ok() {
        lisp.eval("(%tutor-select (list \"cl\" \"reader\" 0))".into());
        answer(&lisp, "(loop)");
        wait_message(&shared, "a runaway answer to be killed", |m| {
            m.contains("exercise 1") && m.contains("infinite loop")
        });
        // ...and the image is still answering afterwards, which is the point.
        says(&shared, &lisp, "alive", "(+ 40 2)", "42");
    }

    // --- Stage 2 is marked *here* -------------------------------------------
    lisp.eval("(%tutor-select (list \"api\" \"image\" 2))".into());
    // Its third exercise asks for the font size *after* setting it to 30, and
    // the check reads it back out of the live editor. Reading it without
    // setting it is the wrong answer...
    answer(&lisp, "(font-size)");
    wait_message(&shared, "the unset font size to be refused", |m| {
        m.contains("exercise 3") && m.contains("not the answer yet")
    });
    // ...and the right one really resizes the editor. This is the assertion
    // that says Stage 2 could not have run in a child process.
    answer(&lisp, "(progn (set-font-size 30) (font-size))");
    wait_message(&shared, "the live-image answer to pass", |m| {
        m.contains("exercise 3 passed")
    });
    wait(&shared, "the editor to have actually resized", |ed| {
        (ed.settings.font_size == 30.0).then_some(())
    });
    lisp.eval("(set-font-size 22)".into());

    // Stage 2's exercises are evaluated in a scratch buffer, so that an
    // exercise which replaces the whole buffer does not replace the answer it
    // is being marked on.
    says(
        &shared,
        &lisp,
        "playground",
        "(and (member *tutor-playground* (buffer-names) :test #'string=) t)",
        "T",
    );
    says(
        &shared,
        &lisp,
        "answer-intact",
        "(and (search \"font-size\" (%tutor-read-answer)) t)",
        "T",
    );

    // --- and the placeholder stage is reachable and says so -----------------
    says(&shared, &lisp, "todo-id", "(and (%org-id-heading \"tutor-todo-todo\") t)", "T");
    says(
        &shared,
        &lisp,
        "todo-text",
        &format!("(and (search \"placeholder\" {LESSON_TEXT}) t)"),
        "T",
    );

    let _ = std::fs::remove_file(&progress);
}
