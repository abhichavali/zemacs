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

/// Wait for the Lisp prompt labelled `label`, and answer whether it completes.
fn wait_prompt(shared: &Shared, label: &str) -> bool {
    wait(shared, &format!("the {label:?} prompt"), |ed| {
        ed.prompt
            .as_ref()
            .and_then(|p| match p.kind {
                PromptKind::Lisp { completing, .. } if p.label == label => Some(completing),
                _ => None,
            })
    })
}

/// One key, the way `feed` types it, but answering what core made of it — the
/// commands a *binding* produces go back to the caller rather than down the
/// channel, so a key bound to a git verb is asserted on here.
fn press(shared: &Shared, lisp: &zemacs_lisp::Lisp, key: Key) -> Vec<EditorCommand> {
    let cmds = shared.lock().unwrap().handle_key(key);
    for cmd in &cmds {
        match cmd.clone() {
            EditorCommand::CallLisp(form) => lisp.eval(form),
            other => shared.lock().unwrap().apply(other),
        }
    }
    cmds
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
    // `modes.lisp` first, as the editor loads it: `git-rebase-mode` is a
    // `define-derived-mode`, and which-key is what the labels are for.
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (message \"magit test init loaded\")\n",
            runtime("modes/modes.lisp"),
            runtime("modes/which-key.lisp"),
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
    //
    // A revision is asked for with a *picker* over the refs — the reader
    // answers `()` here, there being no repository behind a test buffer, and a
    // picker over nothing takes what was typed, which is the whole reason a
    // hash can be typed into it.
    lisp.eval("(git-checkout)".into());
    assert!(wait_prompt(&shared, "Checkout: "), "a revision completes over the refs");
    type_text(&shared, &lisp, "feature/x");
    feed(&shared, &lisp, &[Key::Enter]);
    assert_eq!(wait_git(&rx, &mut seen, "the checkout"), "checkout feature/x");

    // A stash message is prose, and the verb keeps all of it: only the *first*
    // space separates the verb from its argument. Prose is not completed over.
    lisp.eval("(git-stash-named)".into());
    assert!(!wait_prompt(&shared, "Stash message: "), "a message is free text");
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
    //
    // The empty answer is typed into a *free-text* prompt. In a picker, Enter
    // takes the highlighted candidate whatever was typed — that is what a
    // picker is — and the candidates here are real: the reader finds the
    // repository this test is running in.
    lisp.eval("(git-merge)".into());
    wait_prompt(&shared, "Merge: ");
    feed(&shared, &lisp, &[Key::Esc]);
    lisp.eval("(git-branch-new)".into());
    wait_prompt(&shared, "Create branch: ");
    type_text(&shared, &lisp, "   ");
    feed(&shared, &lisp, &[Key::Enter]);
    // Something that *does* answer, behind both of them in the queue: if either
    // had sent a verb it would be ahead of this one. A name no ref of this
    // repository can match, so the text itself is the answer.
    lisp.eval("(git-rebase-onto)".into());
    wait_prompt(&shared, "Rebase onto: ");
    type_text(&shared, &lisp, "zq-nowhere");
    feed(&shared, &lisp, &[Key::Enter]);
    assert_eq!(wait_git(&rx, &mut seen, "the rebase"), "rebase zq-nowhere");

    // --- the keymap ---------------------------------------------------------
    //
    // The popup is which-key over this table, so a missing binding is a missing
    // entry in the panel as well as a key that does nothing. `c' is the one
    // that has to be a *prefix*: core resolves an exact binding before it asks
    // whether a sequence is a prefix, so a bare `c' would make `c a' dead.
    // Discard is on `x', not on magit's own `k'. `k' is *up*, and the whole
    // reason the magit keymap layers over Normal is that the motions keep
    // working — so this assertion is the guard on the motion, not on the verb.
    says(
        &shared,
        &lisp,
        r#"(second (first (where-is "magit-discard")))"#,
        "x",
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
                                     (member (second b) '("c" "P" "F" "r" "b" "t") :test #'string=)))
                    (key-bindings)))"#,
        "0",
    );
    // Magit's own letters, where the verb behind them is new.
    says(&shared, &lisp, r#"(second (first (where-is "magit-rebase-interactive")))"#, "r i");
    says(&shared, &lisp, r#"(second (first (where-is "magit-commit-instant-fixup")))"#, "c F");
    says(&shared, &lisp, r#"(second (first (where-is "git-dispatch")))"#, "?");

    // --- which-key says what Magit's transient says -------------------------
    //
    // The panel beside `b' has to read "Checkout", not "git-checkout": a verb
    // name is a spelling nobody chose, and the whole point of the popup is that
    // it can be read at the speed a hand moves.
    says(&shared, &lisp, r#"(which-key-label "git-checkout")"#, "Checkout");
    says(&shared, &lisp, r#"(which-key-label "magit-commit-instant-fixup")"#, "Instant fixup");
    // ...and a prefix is a group with a name, from the whole table (`?').
    says(
        &shared,
        &lisp,
        r#"(cdr (assoc "b" (which-key-rows "" '("magit-mode" "magit")) :test #'string=))"#,
        "+branch",
    );
    says(
        &shared,
        &lisp,
        r#"(cdr (assoc "b" (which-key-rows "b" '("magit")) :test #'string=))"#,
        "Checkout",
    );
    // `git-dispatch` fills the panel with the whole table, past the limit a
    // passing prefix is capped at.
    lisp.eval("(git-dispatch)".into());
    wait(&shared, "the dispatch panel", |ed| {
        (ed.which_key.len() > 18).then_some(())
    });
    let panel = shared.lock().unwrap().which_key.clone();
    assert!(panel.iter().any(|r| r == "c +commit"), "{panel:?}");
    assert!(panel.iter().any(|r| r == "x Discard"), "{panel:?}");

    // --- git-rebase-mode edits the todo list ---------------------------------
    //
    // A buffer of the mode, holding what `r i` would have opened. The keys are
    // ordinary edits of ordinary text: `s` rewrites the first word and moves
    // down, `M-k` swaps two lines, and `C-c C-c` is the verb that hands the
    // buffer back — which is the last thing this file can see of it.
    lisp.eval(
        r#"(progn (create-buffer "todo")
                  (insert (format nil "pick 1111111 first~%pick 2222222 second~%~%# legend~%"))
                  (set-major-mode "git-rebase-mode")
                  (goto-line 1))"#
            .into(),
    );
    wait(&shared, "the todo buffer", |ed| {
        (ed.buffer.major_mode == "git-rebase-mode" && ed.buffer.text.to_string().starts_with("pick"))
            .then_some(())
    });
    shared.lock().unwrap().mode = zemacs_core::Mode::Normal;
    press(&shared, &lisp, Key::Char('s'));
    says(&shared, &lisp, "(line-string 1)", "squash 1111111 first");
    says(&shared, &lisp, "(line-number)", "2");
    // Moving the step: the two lines trade places and point follows.
    press(&shared, &lisp, Key::Meta('k'));
    says(&shared, &lisp, "(line-string 1)", "pick 2222222 second");
    says(&shared, &lisp, "(line-string 2)", "squash 1111111 first");
    says(&shared, &lisp, "(line-number)", "1");
    // ...but never into the legend, which git does not read.
    press(&shared, &lisp, Key::Meta('k'));
    says(&shared, &lisp, "(line-string 1)", "pick 2222222 second");
    // A letter on a comment line is refused rather than making it a step.
    lisp.eval("(goto-line 4)".into());
    says(&shared, &lisp, "(line-number)", "4");
    press(&shared, &lisp, Key::Char('d'));
    says(&shared, &lisp, "(line-string 4)", "# legend");
    // The two keys that end it are the two verbs, and nothing else may run
    // on `C-c C-c` here — not `eval-dwim`, which is what it is everywhere else.
    press(&shared, &lisp, Key::Ctrl('c'));
    let cmds = press(&shared, &lisp, Key::Ctrl('c'));
    assert!(
        cmds.iter().any(|c| *c == EditorCommand::Git("rebase-finish".into())),
        "C-c C-c hands the list to git: {cmds:?}"
    );
    press(&shared, &lisp, Key::Ctrl('c'));
    let cmds = press(&shared, &lisp, Key::Ctrl('k'));
    assert!(
        cmds.iter().any(|c| *c == EditorCommand::Git("rebase-cancel".into())),
        "C-c C-k throws it away: {cmds:?}"
    );
}
