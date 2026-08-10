//! A config that does not live next to the runtime.
//!
//! This is the whole point of splitting `library.lisp` out of `init.lisp`, and
//! it is the one property no other test in this directory can see. Every other
//! file loads `runtime/init.lisp` *in place*, where `themes/`, `modes/` and
//! `library.lisp` are all siblings — so a config that found the runtime by
//! looking next to itself would pass all of them and still be broken for the
//! only person who matters, whose `init.lisp` is in `~/.zemacs.d/` and has none
//! of those beside it.
//!
//! So: copy the shipped config somewhere with nothing else in it, point
//! `$ZEMACS_RUNTIME` at the real runtime, and prove the four things that have to
//! survive the separation.
//!
//!   1. `library.lisp` is found and loaded — the config's very first form, and
//!      the one that fails loudest, since nothing below it can even be read.
//!   2. The *themes* directory is found, which is `runtime-file` rather than
//!      `*load-truename*` doing the work: `load-theme` is the one caller that
//!      used to say out loud that it looked beside the config file.
//!   3. `*runtime-modules*` is loaded from the runtime, not from the config's
//!      own directory — asserted on a mode this config never names.
//!   4. `*config-file*` still points at the config, not at the library, which is
//!      what `SPC h r` and `C-c i` both depend on. `LOAD` rebinds
//!      `*load-truename*` around the nested load of `library.lisp`, and getting
//!      that ordering wrong would leave "edit your configuration" opening a file
//!      inside the installation.
//!
//! Deliberately a single `#[test]`, like every file beside it: `cl_boot`
//! initialises a process-wide image, so there is one `spawn` per test binary —
//! which is also what makes `set_var` below safe. The variable is set before any
//! thread but this one exists.

use std::path::{Path, PathBuf};
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
            let seen = &ed.messages[ed.messages.len().saturating_sub(12)..];
            panic!("timed out waiting for {what}; status={:?} last={seen:#?}", ed.status);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_message(shared: &Shared, what: &str, pred: impl Fn(&str) -> bool) {
    wait(shared, what, |ed| ed.messages.iter().any(|m| pred(m)).then_some(()));
}

/// Evaluate `form` and wait for its value, printed with `~a`, to be `want`.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"~a\" {form}))"));
    wait_message(shared, form, |m| m == want);
}

fn runtime() -> PathBuf {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime"))
        .expect("runtime/ must exist")
}

#[test]
fn a_config_outside_the_runtime_still_finds_the_library() {
    let runtime = runtime();

    // Somewhere with *nothing* beside it. A fresh directory rather than
    // `temp_dir()` itself, which on a developer's machine has whatever the last
    // hundred tests left in it — and one of those is a `zemacs_test_*.lisp`.
    let home = std::env::temp_dir().join("zemacs_config_elsewhere");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let init = home.join("init.lisp");
    std::fs::copy(runtime.join("init.lisp"), &init).expect("the shipped config must exist");

    // The sibling files the config must *not* be able to reach by looking next
    // to itself. Asserting it rather than assuming it: the copy above takes one
    // file, and a future `copy_dir` here would silently gut this whole test.
    for name in ["library.lisp", "themes", "modes", "lsp.lisp"] {
        assert!(
            !home.join(name).exists(),
            "{name} must not be beside the config, or this test proves nothing"
        );
    }

    // The other half of `crates/app/src/main.rs`, which sets this before it
    // starts the image. Read once, in `PATHS_FORM`, while `spawn` boots ECL.
    std::env::set_var("ZEMACS_RUNTIME", &runtime);

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init.clone());

    // 1. It read to the end. The config's last form is this message, so its
    //    arrival means every `load` above it returned — `library.lisp` included,
    //    since the file cannot be read past that line without it.
    wait_message(&shared, "the config to load", |m| {
        m.starts_with("zemacs: init.lisp loaded")
    });

    // ...and the runtime it used is the real one, not the config's own
    // directory. The assertion that fails if the fallback ever wins.
    says(
        &shared,
        &lisp,
        "(namestring (truename *runtime-dir*))",
        &format!("{}/", runtime.display()),
    );

    // 2. The theme directory was found, and the theme in it actually loaded —
    //    `load-theme` reports a miss with `message` and carries on, so the
    //    interesting assertion is that no such message was ever sent.
    says(&shared, &lisp, "(> (length (theme-names)) 3)", "T");
    {
        let ed = shared.lock().unwrap();
        assert!(
            !ed.messages.iter().any(|m| m.starts_with("no such theme")
                || m.starts_with("load-theme:")),
            "the theme did not load: {:#?}",
            ed.messages
        );
    }

    // 3. A module out of `*runtime-modules*` — named by the library, never by
    //    the config. `org-latex-preview-new` is the one that moved out of
    //    `init.lisp` in this same change, so it is also the check that the
    //    extraction left it reachable.
    says(&shared, &lisp, "(and (fboundp 'org-latex-preview-new) t)", "T");
    says(&shared, &lisp, "(and (fboundp 'which-key) t)", "T");
    says(&shared, &lisp, "(and (fboundp 'lsp-goto-definition) t)", "T");

    // 4. `C-c i` opens the config, not the library. Truenames on both sides:
    //    `/tmp` is a symlink to `/private/tmp` on macOS, and comparing one
    //    resolved path against one unresolved is a failure that says nothing.
    says(
        &shared,
        &lisp,
        "(namestring (truename *config-file*))",
        &std::fs::canonicalize(&init).unwrap().display().to_string(),
    );

    // ...and the library did not quietly become the config, which is what a
    // `setf` placed before the nested `load` rather than after it would do.
    says(&shared, &lisp, "(search \"library.lisp\" (namestring *config-file*))", "NIL");

    let _ = std::fs::remove_dir_all(&home);
}
