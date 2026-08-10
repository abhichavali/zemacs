//! `scroll-past-end`, from the Lisp switch down to the clamp it moves.
//!
//! The feature itself is one number in core and is tested there. What this
//! file is for is the *seam*, which is where a setting has always gone wrong
//! here: the writer is a `defun` over `%do` rather than a primitive, the reader
//! is an arm of `query.rs`, and `runtime/modes/modes.lisp` wraps the writer to
//! remember a baseline. Four places, and nothing but a test that boots the real
//! image proves they name the same setting.
//!
//! So the assertions are deliberately about the editor's *scroll offset* and
//! not only about the flag: a switch that reached `Settings` and did not reach
//! the clamp would look perfect from Lisp.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::path::{Path, PathBuf};
use std::sync::Arc;
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
            panic!(
                "timed out waiting for {what}; scroll={} past-end={} major={:?} last={seen:#?}",
                ed.scroll, ed.settings.scroll_past_end, ed.buffer.major_mode,
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

/// The application's main loop, as far as mode hooks go — the same dispatch
/// `crates/app/src/main.rs` makes, without which a mode's claims never apply.
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

/// The scroll offset the *old* rule allows: the last line on the bottom row.
fn full_pane_scroll(ed: &Editor) -> usize {
    ed.buffer
        .len_lines()
        .saturating_sub(ed.viewport_lines.max(1))
}

#[test]
fn scroll_past_end_is_a_setting_the_image_can_read_write_and_claim() {
    let modes = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/modes/modes.lisp"),
    )
    .expect("runtime/modes/modes.lisp must exist");

    let init = std::env::temp_dir().join("zemacs_test_scroll_past_end_init.lisp");
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)

;; A mode that wants the empty space even where the config has turned it off —
;; a slide deck, a rendered page, anything whose end is meant to be reachable.
(define-derived-mode deck-mode text-mode)
(set-mode-local 'deck-mode 'scroll-past-end t)

(defun report-past-end ()
  (message (format nil "past-end=~a" (scroll-past-end-p))))

(message "scroll-past-end test init loaded")
"#,
            modes.display().to_string()
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = Arc::new(zemacs_lisp::spawn(tx, shared.clone(), init.clone()));
    pump(shared.clone(), lisp.clone());
    wait_message(&shared, "the init file", |m| {
        m == "scroll-past-end test init loaded"
    });

    // A file taller than any pane, in a buffer with a path — the dashboard is
    // generated, and a generated buffer keeps the old clamp whatever the
    // setting says.
    let text: String = (0..200).map(|i| format!("line{i}\n")).collect();
    {
        let mut ed = shared.lock().unwrap();
        ed.load(&text, Some(PathBuf::from("/tmp/zemacs_scroll_past_end.txt")), None);
        ed.viewport_lines = 10;
    }

    // --- on by default, which is what the user asked for ---------------------
    lisp.eval("(scroll-lines 1000)".into());
    let last = wait(&shared, "the view to reach the end", |ed| {
        (ed.scroll == ed.buffer.last_line()).then_some(ed.scroll)
    });
    assert!(last > full_pane_scroll(&shared.lock().unwrap()));
    lisp.eval("(report-past-end)".into());
    wait_message(&shared, "the reader", |m| m == "past-end=T");

    // --- off puts back exactly the old clamp, on the spot ---------------------
    //
    // On the spot and not merely on the next wheel notch: the writer goes
    // through `Editor::apply` like every other command, and `apply` ends in the
    // clamp. A view already out past the end would otherwise stay there with
    // the setting that allowed it turned off.
    lisp.eval("(set-scroll-past-end nil)".into());
    wait(&shared, "the view to come back", |ed| {
        (!ed.settings.scroll_past_end && ed.scroll == full_pane_scroll(ed)).then_some(())
    });
    lisp.eval("(scroll-lines 1000)".into());
    lisp.eval("(report-past-end)".into());
    wait_message(&shared, "the reader again", |m| m == "past-end=NIL");
    {
        let ed = shared.lock().unwrap();
        assert_eq!(ed.scroll, full_pane_scroll(&ed), "off is the old rule");
    }

    // --- and a mode may claim it back, over the config's own baseline ---------
    lisp.eval("(deck-mode)".into());
    wait(&shared, "deck-mode's claim", |ed| {
        (ed.buffer.major_mode == "deck-mode" && ed.settings.scroll_past_end).then_some(())
    });
    lisp.eval("(scroll-lines 1000)".into());
    wait(&shared, "the claim to reach the clamp", |ed| {
        (ed.scroll == ed.buffer.last_line()).then_some(())
    });

    // Leaving the mode reverts to the `nil` the config asked for, never to the
    // factory `t` — the whole reason `%wrap-setting-primitives` records a
    // baseline rather than assuming one.
    lisp.eval("(fundamental-mode)".into());
    wait(&shared, "the claim to be released", |ed| {
        (!ed.settings.scroll_past_end && ed.scroll == full_pane_scroll(ed)).then_some(())
    });

    let _ = std::fs::remove_file(&init);
}
