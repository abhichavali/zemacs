//! `M-x` with arguments: the `interactive` declaration in `runtime/library.lisp`.
//!
//! The whole feature is one wrapper — a command declared `interactive` gets its
//! `symbol-function` replaced by a `&rest` lambda that dispatches on *whether it
//! was given any arguments*, asking for the missing ones when it was not. So the
//! four things worth proving are:
//!
//!   1. a one-argument command asks and then runs (`M-x load-theme`, which is
//!      also the end-to-end proof asked for);
//!   2. a two-argument one asks twice, in order (`M-x lsp-register-server`) —
//!      the case that made a collector worth writing rather than nesting the
//!      callbacks by hand;
//!   3. cancelling anywhere in that chain leaves *nothing* half-done;
//!   4. a zero-argument command is bit-for-bit what it was — one `CallLisp`, no
//!      prompt in front of it. That is the regression that would annoy most.
//!
//! Driven through the real `M-x` prompt rather than by evaluating `(load-theme)`
//! directly, because the interesting claim is about the whole route: core's
//! candidate list, `submitted()` taking the first word of an annotated row,
//! `run_action`'s Lisp fallback, and then the continuation coming back. `feed`
//! is the drain in `crates/app/src/main.rs`, as in `prompt.rs` beside this.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Key, PromptKind, Shared};

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

/// Type at the editor exactly as the main loop does: apply what core can do
/// itself, hand anything aimed at the image to the image.
fn feed(shared: &Shared, lisp: &zemacs_lisp::Lisp, keys: &[Key]) {
    let mut forms = Vec::new();
    {
        let mut ed = shared.lock().unwrap();
        for &key in keys {
            for cmd in ed.handle_key(key) {
                match cmd {
                    EditorCommand::CallLisp(form) => forms.push(form),
                    other => ed.apply(other),
                }
            }
        }
    }
    for form in forms {
        lisp.eval(form);
    }
}

fn type_text(shared: &Shared, lisp: &zemacs_lisp::Lisp, text: &str) {
    let keys: Vec<Key> = text.chars().map(Key::Char).collect();
    feed(shared, lisp, &keys);
}

/// Open `M-x`, narrow to `name`, and take it — the same three gestures a user
/// makes. Returns the forms the acceptance produced, so the caller can assert on
/// what core decided to send before the image has had a chance to answer.
fn execute_command(shared: &Shared, lisp: &zemacs_lisp::Lisp, name: &str) -> Vec<String> {
    {
        let mut ed = shared.lock().unwrap();
        assert!(ed.prompt.is_none(), "a prompt was already up before M-x {name}");
        let out = ed.run_action("M-x");
        assert!(out.is_empty(), "M-x opens a prompt and sends nothing");
    }
    type_text(shared, lisp, name);
    let mut forms = Vec::new();
    {
        let mut ed = shared.lock().unwrap();
        // The row is annotated — `name`, padded, then a docstring — so this is
        // also the check that the annotation never becomes part of the command.
        let picked = ed.prompt.as_ref().and_then(|p| p.current().map(str::to_string));
        assert!(
            picked.as_deref().map(|c| c.starts_with(name)).unwrap_or(false),
            "M-x {name} highlighted {picked:?}"
        );
        for cmd in ed.handle_key(Key::Enter) {
            match cmd {
                EditorCommand::CallLisp(form) => forms.push(form),
                other => ed.apply(other),
            }
        }
        assert!(ed.prompt.is_none(), "the M-x prompt must close before its command runs");
    }
    for form in &forms {
        lisp.eval(form.clone());
    }
    forms
}

/// Wait for the prompt the *image* asked for, and check what it reads.
fn wait_asking(shared: &Shared, label: &str, completing: bool) {
    wait(shared, label, |ed| {
        ed.prompt.as_ref().and_then(|p| match p.kind {
            PromptKind::Lisp { completing: c, .. } => {
                (c == completing && p.label == label).then_some(())
            }
            _ => None,
        })
    });
}

/// Answer the prompt that is up.
fn answer(shared: &Shared, lisp: &zemacs_lisp::Lisp, text: &str) {
    type_text(shared, lisp, text);
    feed(shared, lisp, &[Key::Enter]);
}

#[test]
fn m_x_asks_for_the_arguments_a_command_declares() {
    let runtime = runtime();
    // What `crates/app/src/main.rs` sets before it starts the image, and the
    // only way `runtime-file` can find themes/ and modes/.
    std::env::set_var("ZEMACS_RUNTIME", &runtime);

    let init = std::env::temp_dir()
        .join(format!("zemacs_test_interactive_init-{}.lisp", std::process::id()));
    std::fs::write(
        &init,
        format!(
            // `library.lisp` is the subject and pulls `modes/modes.lisp` in
            // itself. `which-key.lisp` because it wraps `register-command` to
            // pad a docstring onto every row — which is what makes the M-x
            // candidate something other than the bare command name, and so is
            // the reason `submitted()` exists at all. `rpc.lisp`+`lsp.lisp` for
            // the two-argument command.
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(load {:?} :verbose nil :print nil)
(load {:?} :verbose nil :print nil)
(load {:?} :verbose nil :print nil)

;; The control: an ordinary zero-argument command, declared nothing.
(defun demo-zero-arg () "Run without asking anything." (message "zero ran"))

(refresh-commands)
(message "interactive test init loaded")
"#,
            runtime.join("library.lisp").display().to_string(),
            runtime.join("modes/which-key.lisp").display().to_string(),
            runtime.join("rpc.lisp").display().to_string(),
            runtime.join("lsp.lisp").display().to_string(),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    wait_message(&shared, "the init file", |m| m == "interactive test init loaded");

    // --- the list itself ----------------------------------------------------
    //
    // A declared command is offered, which is the point: `set-scale` takes a
    // required argument and could never have been in here before. `set-language`
    // is the one that came *off* `*hidden-commands*` — it was hidden precisely
    // because running it with no argument meant "plain text" and silently
    // uncoloured the buffer.
    wait(&shared, "the command list", |ed| {
        (ed.commands.len() > 20).then_some(())
    });
    {
        let ed = shared.lock().unwrap();
        for name in ["load-theme", "set-scale", "set-language", "lsp-register-server"] {
            assert!(
                ed.commands.iter().any(|c| c.split_whitespace().next() == Some(name)),
                "{name} must be offered by M-x; got {:#?}",
                ed.commands
            );
        }
    }
    // The declaration is a property on the symbol, which is what
    // `refresh-commands` asks rather than re-introspecting a closure it did not
    // compile.
    says(&shared, &lisp, "(and (get 'set-scale 'interactive) t)", "T");
    says(&shared, &lisp, "(and (get 'demo-zero-arg 'interactive) t)", "NIL");
    // ...and the docstring survived having its function replaced, or every
    // wrapped command would be the one row in M-x with nothing beside it.
    says(&shared, &lisp, "(and (documentation 'load-theme 'function) t)", "T");

    // --- 4. a zero-argument command is exactly what it was --------------------
    //
    // First, because it is the regression that matters: one `CallLisp`, spelled
    // `(name)`, and no prompt of any kind afterwards.
    let forms = execute_command(&shared, &lisp, "demo-zero-arg");
    assert_eq!(forms, vec!["(demo-zero-arg)".to_string()]);
    wait_message(&shared, "the zero-arg command", |m| m == "zero ran");
    {
        let ed = shared.lock().unwrap();
        assert!(ed.prompt.is_none(), "a zero-argument command must not ask anything");
    }

    // --- 1. one argument, asked for and then used ----------------------------
    //
    // `M-x load-theme` end to end. Core sends the same bare `(load-theme)` it
    // always did — the whole feature is what the image does with it.
    let forms = execute_command(&shared, &lisp, "load-theme");
    assert_eq!(forms, vec!["(load-theme)".to_string()]);
    wait_asking(&shared, "Theme: ", true);
    {
        // The candidates are the *directory*, evaluated when the prompt opened.
        let ed = shared.lock().unwrap();
        let p = ed.prompt.as_ref().unwrap();
        assert!(p.items.len() > 3, "the theme list came from theme-names: {:?}", p.items);
        assert!(p.items.iter().any(|i| i == "nord"));
        // ...and opening it said nothing. `read-string` answers the prompt *id*
        // and `eval-string` echoes the value of the last form it evaluated, so a
        // wrapper that returned what it collected put a bare `1` in the status
        // line the moment the question went up. Caught by driving the real
        // application through `docs/control-mode.md`, which is why it is here.
        assert!(
            !ed.messages.iter().any(|m| m.chars().all(|c| c.is_ascii_digit()) && !m.is_empty()),
            "asking a question must not echo the prompt id: {:#?}",
            ed.messages
        );
    }
    answer(&shared, &lisp, "nord");
    wait_message(&shared, "the theme", |m| m == "theme: nord");
    wait(&shared, "the prompt closing", |ed| ed.prompt.is_none().then_some(()));

    // Called with the argument in hand, it is untouched — a config's
    // `(load-theme "dracula")` never sees a prompt.
    lisp.eval(r#"(load-theme "dracula")"#.into());
    wait_message(&shared, "a direct call", |m| m == "theme: dracula");
    {
        let ed = shared.lock().unwrap();
        assert!(ed.prompt.is_none(), "an argument given is an argument not asked for");
    }

    // --- the live preview ----------------------------------------------------
    //
    // Scrolling the theme list loads each theme as the highlight reaches it, so
    // you are choosing between *themes* rather than between eleven names. The
    // proof that it is really applied rather than only reported is that the
    // command itself is what runs: `load-theme` is the only thing that can emit
    // `theme: X`, and it is not reached until an answer is accepted.
    //
    // "dracula" is showing, from the direct call above.
    execute_command(&shared, &lisp, "load-theme");
    wait_asking(&shared, "Theme: ", true);
    // The candidates arrive one `PromptItem` at a time, so the prompt exists
    // for a moment with nothing in it and `C-n` would have nothing to move to.
    wait(&shared, "the candidates", |ed| {
        ed.prompt.as_ref().filter(|p| p.items.len() > 3).map(|_| ())
    });

    // Nothing is previewed by *opening* — the buffer switcher does not either,
    // and a picker that changed your theme the instant it appeared would be
    // doing it before you asked for anything.
    feed(&shared, &lisp, &[Key::Ctrl('n')]);
    let first = {
        let ed = shared.lock().unwrap();
        assert!(ed.prompt.is_some(), "a preview must not close the prompt");
        ed.prompt.as_ref().unwrap().current().unwrap().to_string()
    };
    wait_message(&shared, "the highlighted theme, loaded", |m| {
        m == format!("theme: {first}")
    });

    // ...and the next one down loads in turn, which is what "as I scroll" means.
    feed(&shared, &lisp, &[Key::Ctrl('n')]);
    let second = {
        let ed = shared.lock().unwrap();
        ed.prompt.as_ref().unwrap().current().unwrap().to_string()
    };
    assert_ne!(first, second, "C-n moved the highlight");
    wait_message(&shared, "the next theme, loaded", |m| {
        m == format!("theme: {second}")
    });

    // Escape puts back what was showing when the prompt opened. Without this a
    // preview is a trap: browsing the list would silently change your theme.
    feed(&shared, &lisp, &[Key::Esc]);
    wait(&shared, "the prompt closing", |ed| ed.prompt.is_none().then_some(()));
    wait_message(&shared, "the theme restored", |m| m == "theme: dracula");
    says(&shared, &lisp, "*current-theme*", "dracula");
    says(&shared, &lisp, "(hash-table-count *prompt-previews*)", "0");

    // --- the `:number` source ------------------------------------------------
    //
    // A string is what a prompt answers with and an integer is what `set-scale`
    // means, and the declaration is the only place that difference can be
    // written down — the parameter is called `n` and a name cannot say that.
    execute_command(&shared, &lisp, "set-scale");
    wait_asking(&shared, "Font size: ", false);
    answer(&shared, &lisp, "31");
    wait_message(&shared, "the new size", |m| m == "font size 31");
    says(&shared, &lisp, "*font-size*", "31");

    // An answer that is not a number stops and says so, rather than handing
    // `set-scale` a string and letting `min` signal from inside it.
    execute_command(&shared, &lisp, "set-scale");
    wait_asking(&shared, "Font size: ", false);
    answer(&shared, &lisp, "huge");
    wait_message(&shared, "the complaint", |m| m == "not a number: huge");
    says(&shared, &lisp, "*font-size*", "31");

    // --- 2. two arguments, asked in order ------------------------------------
    //
    // `lsp-register-server` is `(mode program &rest args)`. Two prompts, the
    // second opened from inside the first's callback, and the command runs once
    // at the end with both answers — not twice with one each.
    execute_command(&shared, &lisp, "lsp-register-server");
    wait_asking(&shared, "Mode: ", true);
    {
        let ed = shared.lock().unwrap();
        let p = ed.prompt.as_ref().unwrap();
        assert!(p.items.iter().any(|i| i == "rust-mode"), "modes: {:?}", p.items);
    }
    // Not one of the offered modes: nothing is required to match, which is what
    // keeps a hand-written candidate list from being a closed set.
    answer(&shared, &lisp, "go-mode");
    wait_asking(&shared, "Program: ", false);
    answer(&shared, &lisp, "gopls");
    says(
        &shared,
        &lisp,
        "(getf (gethash \"go-mode\" *lsp-servers*) :program)",
        "gopls",
    );

    // --- 3. cancelling leaves nothing half-done ------------------------------
    //
    // Escape at the *second* prompt. The first answer is already in hand, and
    // the rule is that it is thrown away with it: the command is applied once,
    // with everything, or not at all.
    execute_command(&shared, &lisp, "lsp-register-server");
    wait_asking(&shared, "Mode: ", true);
    answer(&shared, &lisp, "abandoned-mode");
    wait_asking(&shared, "Program: ", false);
    feed(&shared, &lisp, &[Key::Esc]);
    wait(&shared, "the prompt closing", |ed| ed.prompt.is_none().then_some(()));
    says(&shared, &lisp, "(gethash \"abandoned-mode\" *lsp-servers*)", "NIL");

    // Escape at the *first* prompt is the same rule with nothing collected yet.
    execute_command(&shared, &lisp, "load-theme");
    wait_asking(&shared, "Theme: ", true);
    feed(&shared, &lisp, &[Key::Esc]);
    wait(&shared, "the prompt closing", |ed| ed.prompt.is_none().then_some(()));
    says(&shared, &lisp, "*font-size*", "31"); // a queue drain, to order the next check
    {
        let ed = shared.lock().unwrap();
        assert!(
            !ed.messages.iter().any(|m| m == "theme: " || m.starts_with("no such theme")),
            "a cancelled load-theme must not have loaded anything: {:#?}",
            ed.messages
        );
    }
    // ...and no continuation was left parked. A cancel that said nothing would
    // leak the closure, which is the failure `%prompt-reply` exists to prevent
    // and which nesting them by hand would be the easiest way to reintroduce.
    says(&shared, &lisp, "(hash-table-count *prompt-continuations*)", "0");

    // --- the zero-argument path once more, after all of that ------------------
    //
    // The wrappers are installed process-wide and for good; this is the check
    // that living beside them changed nothing for a command without one.
    let forms = execute_command(&shared, &lisp, "demo-zero-arg");
    assert_eq!(forms, vec!["(demo-zero-arg)".to_string()]);
    wait(&shared, "the zero-arg command again", |ed| {
        (ed.messages.iter().filter(|m| *m == "zero ran").count() == 2).then_some(())
    });
    {
        let ed = shared.lock().unwrap();
        assert!(ed.prompt.is_none(), "still nothing to ask");
    }

    // The one documented behaviour the wrapper takes away, and how to spell it:
    // `(set-language)` used to mean "plain text", and an explicit NIL is an
    // argument, so it still does.
    lisp.eval("(set-language nil)".into());
    says(&shared, &lisp, "(major-mode)", "fundamental-mode");
    {
        let ed = shared.lock().unwrap();
        assert!(ed.prompt.is_none(), "an explicit NIL is an argument, not a missing one");
        assert!(ed.buffer.language.is_none());
    }
}
