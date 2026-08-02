//! A primitive called from a Lisp thread the *config* started must reach the
//! editor.
//!
//! This exists because it did not, twice. The host handle a primitive answers
//! through lives in a Rust static; when it was a `thread_local!` it was
//! installed only on the thread ECL was booted on, so `(message "x")` from a
//! thread `mp:process-run-function` made did **nothing at all** — no error, no
//! message, no edit. Silence is the worst failure a primitive can have, and
//! nothing in the suite noticed, because nothing in the suite ever called a
//! primitive from anywhere but the boot thread.
//!
//! `math-written.lisp`'s MathSync watcher is exactly such a thread: it polls
//! `~/Public/MathSync` on its own and writes a transcription into a buffer. With
//! a thread-local host that feature is inert and looks like a bug in the
//! watcher. So this is a small test guarding a large, silent failure.
//!
//! One `#[test]`, as in every file beside it: `cl_boot` initialises a
//! process-wide Lisp image, so there is exactly one `spawn` per test binary.

use std::time::{Duration, Instant};

use zemacs_core::{Editor, Shared};

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
            panic!("timed out waiting for {what}; last={seen:#?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_primitive_reaches_the_editor_from_a_lisp_worker_thread() {
    let init = std::env::temp_dir().join("zemacs_test_worker_init.lisp");
    std::fs::write(
        &init,
        // The marker is written from the *worker*, so the assertion cannot pass
        // by the boot thread having done the work. `#-threads` says so out loud
        // rather than skipping quietly — a build without threads cannot run
        // MathSync either, and that is worth knowing from a test name.
        r#"(in-package :zemacs)
#+threads (mp:process-run-function
           "zemacs-test-worker"
           (lambda () (message "a worker reached the editor")))
#-threads (message "this ECL has no threads")
(message "worker test init loaded")
"#,
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let _lisp = zemacs_lisp::spawn(tx, shared.clone(), init);

    wait(&shared, "the init file to load", |ed| {
        ed.messages
            .iter()
            .any(|m| m == "worker test init loaded")
            .then_some(())
    });

    // The worker is asynchronous — the load returning says nothing about
    // whether it has run yet, so this waits for the message rather than
    // sampling once.
    let reached = wait(&shared, "the worker's message", |ed| {
        ed.messages
            .iter()
            .find(|m| {
                *m == "a worker reached the editor" || *m == "this ECL has no threads"
            })
            .cloned()
    });

    assert_eq!(
        reached, "a worker reached the editor",
        "this ECL build has no threads, so MathSync's watcher cannot run either"
    );
}
