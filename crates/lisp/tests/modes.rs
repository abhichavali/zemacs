//! Headless proof of the major/minor mode machinery in `runtime/modes/`.
//!
//! All of it is Common Lisp on top of the primitives the editor already
//! exports, so the only way to check it is to run the real image against a real
//! [`Editor`] and watch the settings move — which is what this does.
//!
//! One thing the app normally does has to be done here by hand. Core does not
//! call Lisp; it records that a mode hook is *due* in `Editor::pending_hooks`,
//! and the main loop drains that and asks the image to run each one. There is
//! no main loop in a test, so [`pump`] is that loop, spelled the same way.
//!
//! Deliberately a single `#[test]`, for the same reason `eval.rs` is: `cl_boot`
//! initialises a process-wide Lisp image, so there is exactly one `spawn` per
//! test binary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zemacs_core::{Editor, LineOverflow, Shared};

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
            // One lock for the whole report: `Mutex` is not reentrant, so two
            // of these live in one expression would hang instead of failing.
            let ed = shared.lock().unwrap();
            let seen = &ed.messages[ed.messages.len().saturating_sub(8)..];
            panic!(
                "timed out waiting for {what}; status={:?} major={:?} minor={:?} last={seen:#?}",
                ed.status, ed.buffer.major_mode, ed.buffer.minor_modes,
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_message(shared: &Shared, what: &str, pred: impl Fn(&str) -> bool) {
    wait(shared, what, |ed| {
        ed.messages.iter().any(|m| pred(m)).then_some(())
    })
}

/// How many messages have been produced, for "nothing after this point said X".
fn mark(shared: &Shared) -> usize {
    shared.lock().unwrap().messages.len()
}

fn messages_since(shared: &Shared, from: usize) -> Vec<String> {
    shared.lock().unwrap().messages[from..].to_vec()
}

/// The last `n` messages, for failure reports.
fn last_messages(shared: &Shared, n: usize) -> Vec<String> {
    let all = &shared.lock().unwrap().messages;
    all[all.len().saturating_sub(n)..].to_vec()
}

/// How many messages equal `text`, over the whole run. An exit hook that fires
/// twice is as wrong as one that never fires.
fn count(shared: &Shared, text: &str) -> usize {
    shared
        .lock()
        .unwrap()
        .messages
        .iter()
        .filter(|m| *m == text)
        .count()
}

/// Stand in for the application's main loop: hand every pending mode hook to
/// the image, guarded by `fboundp` so a mode with no hook is silence. Copied
/// from `crates/app/src/main.rs` on purpose — if that dispatch changes shape,
/// this test should have to change with it.
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

#[test]
fn modes_are_built_in_lisp() {
    // The entry point, canonicalised: `load` inside it resolves its sibling
    // through `*load-truename*`, and this is what proves that works from an
    // arbitrary working directory.
    let entry = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/modes/modes.lisp"),
    )
    .expect("runtime/modes/modes.lisp must exist");

    let init = std::env::temp_dir().join("zemacs_test_modes_init.lisp");
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)

;; The exit hook is the user's to write — the machinery only fires it.
(defun org-mode-exit-hook () (message "left org-mode"))

;; A mode of our own, to prove the entry body runs, that a child inherits its
;; parent's keys, and that a stranger is not mistaken for a relative.
(define-derived-mode test-mode prog-mode (message "test-mode body"))

;; The shipped minor mode declares a setting and needs no code; this one is the
;; other kind.
(define-minor-mode test-minor "A minor mode with both bodies."
  (:on  (message "test-minor on"))
  (:off (message "test-minor off")))

(message "modes test init loaded")
"#,
            entry.display().to_string()
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = Arc::new(zemacs_lisp::spawn(tx, shared.clone(), init));
    pump(shared.clone(), lisp.clone());

    let at = mark(&shared);
    wait_message(&shared, "the mode files to load", |m| {
        m == "modes test init loaded"
    });
    let tail = messages_since(&shared, at);
    assert!(
        !tail.iter().any(|m| m.contains("error")),
        "runtime/modes must load cleanly; got {tail:#?}"
    );

    // --- the baseline is whatever the config last set globally ---------------
    //
    // These settings are global in the editor and none of them has a reader, so
    // the Lisp side wraps the primitive to record what it was asked for. If the
    // wrap failed, this still passes — the point of it is the revert below.
    lisp.eval("(set-relative-line-numbers t)".into());
    wait(&shared, "a global setting to still reach the editor", |ed| {
        ed.settings.relative_line_numbers.then_some(())
    });

    // --- a mode-local setting applies... -------------------------------------
    //
    // Wrapping comes from `text-mode`, which `org-mode` derives from; the
    // absolute line numbers are org's own claim, over a global `t`.
    lisp.eval("(org-mode)".into());
    wait(&shared, "org-mode's settings", |ed| {
        (ed.buffer.major_mode == "org-mode"
            && ed.settings.line_overflow == LineOverflow::Wrap
            && !ed.settings.relative_line_numbers)
            .then_some(())
    });

    // --- ...and reverts on the way out --------------------------------------
    //
    // The whole point. `fundamental-mode` says nothing about either setting:
    // wrapping goes back to the editor default and relative numbering goes back
    // to the `t` the config asked for, not to the factory `nil`.
    lisp.eval("(fundamental-mode)".into());
    wait(&shared, "org-mode's settings to revert", |ed| {
        (ed.buffer.major_mode == "fundamental-mode"
            && ed.settings.line_overflow == LineOverflow::Truncate
            && ed.settings.relative_line_numbers)
            .then_some(())
    });

    // --- the exit hook fired, exactly once ----------------------------------
    wait_message(&shared, "org-mode-exit-hook", |m| m == "left org-mode");
    // Re-entering and leaving again is one more, never two: the hook is driven
    // by a *change* of mode, and entering a mode you are already in is not one.
    lisp.eval("(org-mode)".into());
    wait(&shared, "org-mode again", |ed| {
        (ed.settings.line_overflow == LineOverflow::Wrap).then_some(())
    });
    lisp.eval("(org-mode)".into());
    lisp.eval("(fundamental-mode)".into());
    wait(&shared, "fundamental-mode again", |ed| {
        (ed.settings.line_overflow == LineOverflow::Truncate).then_some(())
    });
    // Settle: the last hook has landed, so nothing more is in flight.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        count(&shared, "left org-mode"),
        2,
        "one exit per departure — no more, and no less; got {:#?}",
        last_messages(&shared, 12)
    );

    // --- inheritance --------------------------------------------------------
    //
    // `derived-mode-p` answers about a mode you name, and about the live
    // buffer's when you do not.
    lisp.eval(
        r#"(message (format nil "~a ~a ~a ~a"
                            (derived-mode-p 'prog-mode "rust-mode")
                            (derived-mode-p 'text-mode "rust-mode")
                            (derived-mode-p 'prog-mode "test-mode")
                            (derived-mode-p 'prog-mode)))"#
            .into(),
    );
    wait_message(&shared, "derived-mode-p", |m| {
        m == "prog-mode NIL prog-mode NIL"
    });
    // A mode is derived from itself, the way it is in Emacs.
    lisp.eval(r#"(message (format nil "~a" (derived-mode-p 'org-mode "org-mode")))"#.into());
    wait_message(&shared, "a mode is its own ancestor", |m| m == "org-mode");

    // The entry body ran, and ran under the mode's own name.
    lisp.eval("(test-mode)".into());
    wait_message(&shared, "the mode body", |m| m == "test-mode body");
    // ...and it inherited `prog-mode`'s claim on line overflow, so a mode that
    // declares nothing of its own still gets its parent's settings.
    wait(&shared, "prog-mode's settings in a child", |ed| {
        (ed.buffer.major_mode == "test-mode" && ed.settings.line_overflow == LineOverflow::Truncate)
            .then_some(())
    });

    // --- a minor mode toggles, and its bodies run ---------------------------
    lisp.eval("(test-minor)".into());
    wait(&shared, "test-minor on", |ed| {
        ed.buffer
            .minor_modes
            .contains(&"test-minor".to_string())
            .then_some(())
    });
    wait_message(&shared, "the :on body", |m| m == "test-minor on");
    lisp.eval("(test-minor)".into());
    wait(&shared, "test-minor off", |ed| {
        ed.buffer.minor_modes.is_empty().then_some(())
    });
    wait_message(&shared, "the :off body", |m| m == "test-minor off");

    // The shipped one claims a setting instead, and claims it over the major
    // mode's — `test-mode` inherits "truncate" from `prog-mode`, and a minor
    // mode is resolved after the major one.
    lisp.eval("(visual-line)".into());
    wait(&shared, "visual-line to wrap", |ed| {
        (ed.settings.line_overflow == LineOverflow::Wrap
            && ed.buffer.minor_modes.contains(&"visual-line".to_string()))
        .then_some(())
    });
    lisp.eval("(visual-line)".into());
    wait(&shared, "visual-line to release the setting", |ed| {
        (ed.settings.line_overflow == LineOverflow::Truncate && ed.buffer.minor_modes.is_empty())
            .then_some(())
    });

    // --- choosing a mode from the filename ----------------------------------
    lisp.eval(
        r#"(message (format nil "~a ~a ~a"
                            (auto-mode-for "/tmp/notes.TXT")
                            (auto-mode-for "/src/README")
                            (auto-mode-for nil)))"#
            .into(),
    );
    // Case-insensitive, and a bare filename matches as a suffix of its path.
    wait_message(&shared, "auto-mode-for", |m| {
        m == "text-mode text-mode NIL"
    });

    // End to end: opening a file the editor has no grammar for lands in
    // `fundamental-mode`, whose hook consults the alist and moves it on.
    {
        let mut ed = shared.lock().unwrap();
        ed.load(
            "prose\n",
            Some(PathBuf::from("/tmp/zemacs_test_modes.txt")),
            None,
        );
        assert_eq!(
            ed.buffer.major_mode, "fundamental-mode",
            "core has no language for .txt, so this is the mode Lisp has to fix"
        );
    }
    wait(&shared, "auto-mode-alist to pick text-mode", |ed| {
        (ed.buffer.major_mode == "text-mode" && ed.settings.line_overflow == LineOverflow::Wrap)
            .then_some(())
    });

    // --- inherited keymaps --------------------------------------------------
    //
    // The editor's mode keymap is keyed by the exact mode name and knows
    // nothing about inheritance, so `define-mode-key` has to re-issue a
    // parent's bindings under every child's name — including a child defined
    // after the binding was made, which `test-mode` is.
    {
        let ed = shared.lock().unwrap();
        let bound = |mode: &str, keys: &str| {
            ed.mode_keymap
                .get(&(mode.to_string(), keys.to_string()))
                .cloned()
        };
        for mode in ["prog-mode", "rust-mode", "lisp-mode", "test-mode"] {
            assert_eq!(
                bound(mode, "SPC t w").as_deref(),
                Some("visual-line"),
                "prog-mode's binding should reach {mode}"
            );
        }
        assert_eq!(
            bound("text-mode", "SPC t w"),
            None,
            "a mode outside the family must not inherit the binding"
        );
        assert_eq!(
            bound("lisp-mode", "SPC m e").as_deref(),
            Some("eval-dwim"),
            "lisp-mode's own binding"
        );
        assert_eq!(
            bound("rust-mode", "SPC m e"),
            None,
            "a sibling's binding is not inherited"
        );

        // Every mode registers itself with M-x, without waiting for
        // `refresh-commands` — this init file never calls it.
        for wanted in ["org-mode", "rust-mode", "fundamental-mode", "visual-line"] {
            assert!(
                ed.commands.iter().any(|c| c == wanted),
                "M-x must offer {wanted}; got {:?}",
                ed.commands
            );
        }
    }

    // --- a setting nothing knows about is reported, not silently dropped ----
    let at = mark(&shared);
    lisp.eval("(set-mode-local 'org-mode 'bogus 1)".into());
    wait_message(&shared, "a rejected mode setting", |m| {
        m.contains("no such mode setting")
    });
    let tail = messages_since(&shared, at);
    assert!(
        !tail.iter().any(|m| m.contains("lisp error")),
        "a typo in a config should be a message, not an error; got {tail:#?}"
    );

    // --- and all of it survives the shipped config being loaded on top ------
    //
    // `runtime/init.lisp` hand-writes `org-mode-hook`, `fundamental-mode-hook`
    // and `rust-mode-hook`, which are three of the functions
    // `define-derived-mode` generates. Last definition wins, so the order the
    // init file loads this in is not a detail: loading it *after* those DEFUNs
    // is what keeps the machinery attached. This is that order, and it has to
    // leave the modes working — including the settings baseline, which init
    // sets globally before the reload.
    let shipped = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp"),
    )
    .expect("runtime/init.lisp must exist");
    lisp.eval(format!(
        "(load {:?} :verbose nil :print nil)",
        shipped.display().to_string()
    ));
    lisp.eval(format!(
        "(load {:?} :verbose nil :print nil)",
        entry.display().to_string()
    ));
    lisp.eval("(set-relative-line-numbers t)".into());
    wait(&shared, "the shipped config to have loaded", |ed| {
        (ed.settings.relative_line_numbers && ed.settings.font_size == 22.0).then_some(())
    });

    lisp.eval("(org-mode)".into());
    wait(&shared, "org-mode after a reload", |ed| {
        (ed.settings.line_overflow == LineOverflow::Wrap && !ed.settings.relative_line_numbers)
            .then_some(())
    });
    lisp.eval("(rust-mode)".into());
    wait(&shared, "rust-mode after a reload", |ed| {
        (ed.buffer.major_mode == "rust-mode"
            && ed.settings.line_overflow == LineOverflow::Truncate
            && ed.settings.relative_line_numbers)
            .then_some(())
    });
    assert_eq!(
        count(&shared, "left org-mode"),
        3,
        "the exit hook must survive the reload too; got {:#?}",
        last_messages(&shared, 12)
    );
}
