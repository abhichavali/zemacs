//! `runtime/modes/magit.lisp`: the half of magit that is a question.
//!
//! The interesting claim is the round trip, and it is the one a reader cannot
//! check by eye. A branch name is *data*, so it is asked for in the image with
//! `read-string`; the verb it is handed to is Rust's, and it gets there as one
//! `EditorCommand::Git` carrying the verb and the answer in a single string. If
//! the two halves ever disagree about that spelling — a missing space, an
//! argument dropped on the floor — `b b` silently checks out nothing, and this
//! is the test that notices.
//!
//! Nothing here runs git. The command channel is the boundary being asserted,
//! and what happens on the far side of it is `crates/git/tests/repo.rs`.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Key, PromptKind, Shared};

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

fn wait_message(shared: &Shared, what: &str, pred: impl Fn(&str) -> bool) {
    wait(shared, what, |ed| {
        ed.messages.iter().any(|m| pred(m)).then_some(())
    });
}

fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"~a\" {form}))"));
    wait(shared, &format!("{form} to answer {want:?}"), |ed| {
        ed.messages.iter().any(|m| m == want).then_some(())
    });
}

/// Type at the editor as the main loop does: core carries out what it can, and
/// anything aimed at the image goes to the image.
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

fn wait_prompt(shared: &Shared, label: &str) {
    wait(shared, &format!("the {label:?} prompt"), |ed| {
        ed.prompt
            .as_ref()
            .filter(|p| matches!(p.kind, PromptKind::Lisp { .. }) && p.label == label)
            .map(|_| ())
    });
}

/// Drain the command channel until a git verb arrives, and answer with it.
fn wait_git(
    rx: &crossbeam_channel::Receiver<EditorCommand>,
    seen: &mut Vec<EditorCommand>,
    what: &str,
) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(EditorCommand::Git(verb)) = seen.iter().find(|c| matches!(c, EditorCommand::Git(_))) {
            let verb = verb.clone();
            seen.retain(|c| !matches!(c, EditorCommand::Git(_)));
            return verb;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; got {seen:#?}"
        );
        if let Ok(c) = rx.recv_timeout(Duration::from_millis(100)) {
            seen.push(c);
        }
    }
}

fn runtime(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .expect("the runtime directory ships with the source")
}

#[test]
fn a_branch_name_is_asked_for_in_lisp_and_arrives_as_one_git_verb() {
    let init = std::env::temp_dir()
        .join(format!("zemacs_test_magit_init-{}.lisp", std::process::id()));
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (message \"magit test init loaded\")\n",
            runtime("modes/magit.lisp"),
        ),
    )
    .unwrap();

    let (tx, rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    let mut seen = Vec::new();
    wait_message(&shared, "the init file", |m| m == "magit test init loaded");

    // --- the round trip -----------------------------------------------------
    lisp.eval("(git-checkout)".into());
    wait_prompt(&shared, "Checkout: ");
    type_text(&shared, &lisp, "feature/x");
    feed(&shared, &lisp, &[Key::Enter]);
    assert_eq!(wait_git(&rx, &mut seen, "the checkout"), "checkout feature/x");

    // A stash message is prose, and the verb keeps all of it: only the *first*
    // space separates the verb from its argument.
    lisp.eval("(git-stash-named)".into());
    wait_prompt(&shared, "Stash message: ");
    type_text(&shared, &lisp, "work in progress");
    feed(&shared, &lisp, &[Key::Enter]);
    assert_eq!(
        wait_git(&rx, &mut seen, "the stash"),
        "stash work in progress"
    );

    // --- and the two ways of answering nothing ------------------------------
    //
    // Cancelling calls the continuation with NIL, and an empty answer is an
    // answer: neither may reach git, because `(magit "checkout")` with no
    // argument would check out whatever the cursor happened to be on.
    lisp.eval("(git-merge)".into());
    wait_prompt(&shared, "Merge: ");
    feed(&shared, &lisp, &[Key::Esc]);
    lisp.eval("(git-branch-delete)".into());
    wait_prompt(&shared, "Delete branch: ");
    type_text(&shared, &lisp, "   ");
    feed(&shared, &lisp, &[Key::Enter]);
    // Something that *does* answer, behind both of them in the queue: if either
    // had sent a verb it would be ahead of this one.
    lisp.eval("(git-rebase-onto)".into());
    wait_prompt(&shared, "Rebase onto: ");
    type_text(&shared, &lisp, "main");
    feed(&shared, &lisp, &[Key::Enter]);
    assert_eq!(wait_git(&rx, &mut seen, "the rebase"), "rebase main");

    // --- the keymap ---------------------------------------------------------
    //
    // The popup is which-key over this table, so a missing binding is a missing
    // entry in the panel as well as a key that does nothing. `c' is the one
    // that has to be a *prefix*: core resolves an exact binding before it asks
    // whether a sequence is a prefix, so a bare `c' would make `c a' dead.
    says(
        &shared,
        &lisp,
        r#"(second (first (where-is "magit-discard")))"#,
        "k",
    );
    says(
        &shared,
        &lisp,
        r#"(second (first (where-is "magit-commit")))"#,
        "c c",
    );
    says(
        &shared,
        &lisp,
        r#"(second (first (where-is "git-checkout")))"#,
        "b b",
    );
    // Nothing may claim `c', `P' or `F' whole in the magit map.
    says(
        &shared,
        &lisp,
        r#"(length (remove-if-not
                    (lambda (b) (and (string= (first b) "magit")
                                     (member (second b) '("c" "P" "F") :test #'string=)))
                    (key-bindings)))"#,
        "0",
    );
}
