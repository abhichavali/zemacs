//! Headless proof of the last things the image could not reach: making and
//! killing a buffer, running a built-in verb by name, sending a keystroke to the
//! shell, and — the interesting one — **asking the user a question and getting
//! the answer back**.
//!
//! Every row of the "genuinely unreachable from Lisp" table in
//! `docs/boundary.org` is asserted here, in the shape a config would write.
//!
//! This file also plays the part of the main loop for the half a headless test
//! does not otherwise have: `feed` is the drain in `crates/app/src/main.rs`, and
//! the only thing it does differently from `Editor::apply` is hand a `CallLisp`
//! to the image rather than dropping it. That is the entire route by which a
//! prompt's answer reaches a Lisp closure, so if the two ever disagree this test
//! is the one that notices.
//!
//! Deliberately a single `#[test]`, as in the files beside it: `cl_boot`
//! initialises a process-wide image, so a new file is the only way to add one.

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
    wait(shared, what, |ed| {
        ed.messages.iter().any(|m| pred(m)).then_some(())
    })
}

/// Evaluate `form` and wait for its value to be reported.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"~a\" {form}))"));
    wait_message(shared, form, |m| m == want);
}

/// Drain the app channel until `pred` matches — only the commands core cannot
/// carry out alone ever come this way.
fn wait_for(
    rx: &crossbeam_channel::Receiver<EditorCommand>,
    seen: &mut Vec<EditorCommand>,
    what: &str,
    pred: impl Fn(&EditorCommand) -> bool,
) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if seen.iter().any(&pred) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}; got {seen:#?}");
        if let Ok(c) = rx.recv_timeout(Duration::from_millis(100)) {
            seen.push(c);
        }
    }
}

/// Type at the editor, exactly as the main loop does: apply what core can do
/// itself and hand anything aimed at the image to the image.
///
/// The lock is dropped before a form is queued, because the Lisp thread will
/// take it as soon as it starts evaluating.
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

/// Type a whole string.
fn type_text(shared: &Shared, lisp: &zemacs_lisp::Lisp, text: &str) {
    let keys: Vec<Key> = text.chars().map(Key::Char).collect();
    feed(shared, lisp, &keys);
}

/// Wait for the prompt Lisp asked for to be up.
fn wait_prompt(shared: &Shared, completing: bool) {
    wait(shared, "the Lisp prompt", |ed| {
        ed.prompt
            .as_ref()
            .map(|p| p.kind)
            .and_then(|k| match k {
                PromptKind::Lisp { completing: c, .. } => (c == completing).then_some(()),
                _ => None,
            })
    });
}

#[test]
fn lisp_can_make_buffers_run_verbs_and_ask_questions() {
    let init = std::env::temp_dir().join("zemacs_test_prompt_init.lisp");
    std::fs::write(
        &init,
        "(in-package :zemacs)\n(message \"prompt test init loaded\")\n",
    )
    .unwrap();

    let (tx, rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    let mut seen = Vec::new();
    wait_message(&shared, "the init file", |m| m == "prompt test init loaded");

    // --- a buffer with no file behind it ----------------------------------
    //
    // Applied on the spot, not a frame later like `find-file` — which is the
    // whole reason a command that fills a buffer can be written as "make it,
    // then write into it" instead of hanging off a hook.
    lisp.eval(r#"(create-buffer "*notes*")"#.into());
    wait(&shared, "the new buffer", |ed| {
        (ed.buffer.name() == "*notes*").then_some(())
    });
    says(&shared, &lisp, "(buffer-file-name)", "NIL");
    says(&shared, &lisp, "(buffer-read-only-p)", "NIL");

    // The very next form sees it, which is the point.
    lisp.eval(r#"(progn (create-buffer "*notes*") (insert "one two"))"#.into());
    wait(&shared, "text in the created buffer", |ed| {
        (ed.buffer.text.to_string() == "one two").then_some(())
    });
    // Idempotent by name: a second call shows the buffer rather than stacking a
    // second one nobody could tell from the first.
    says(&shared, &lisp, "(count \"*notes*\" (buffer-names) :test #'string=)", "1");
    says(&shared, &lisp, "(buffer-index \"*notes*\")", "0");

    // A language can be asked for, so a created buffer is not condemned to be
    // uncoloured — the difference between `create-buffer` and a scratchpad that
    // has to be written to disk first.
    lisp.eval(r#"(create-buffer "*lisp-pad*" "lisp")"#.into());
    wait(&shared, "the language", |ed| {
        (ed.buffer.name() == "*lisp-pad*" && ed.buffer.language.as_deref() == Some("lisp"))
            .then_some(())
    });

    // --- killing one -------------------------------------------------------
    says(&shared, &lisp, r#"(kill-buffer "*lisp-pad*")"#, "T");
    wait(&shared, "the buffer going", |ed| {
        (!ed.buffer_names().iter().any(|n| n == "*lisp-pad*")).then_some(())
    });
    // Killing the live buffer adopts the most recently visited other one, and
    // every window that was showing the dead buffer follows.
    lisp.eval(r#"(switch-to-buffer "*notes*")"#.into());
    wait(&shared, "*notes*", |ed| (ed.buffer.name() == "*notes*").then_some(()));
    lisp.eval("(kill-buffer)".into());
    wait(&shared, "the live buffer going", |ed| {
        (ed.buffer.name() != "*notes*").then_some(())
    });
    {
        let ed = shared.lock().unwrap();
        for frame in &ed.frames {
            for w in &frame.windows {
                assert!(
                    ed.buffer_by_id(w.buffer).is_some(),
                    "a window must never point at a killed buffer"
                );
            }
        }
    }
    says(&shared, &lisp, r#"(kill-buffer "nope")"#, "NIL");
    wait_message(&shared, "the report", |m| m == "no such buffer: nope");

    // --- running a built-in verb by name ------------------------------------
    //
    // Lisp could always *bind* "magit-status" and never call it. This is the
    // same resolution `M-x` and every key binding do, so what a key does and
    // what this does cannot drift apart.
    lisp.eval(r#"(call-command "split-window-right")"#.into());
    wait(&shared, "the split", |ed| {
        (ed.frame().windows.len() == 2).then_some(())
    });
    lisp.eval(r#"(call-command "delete-window")"#.into());
    wait(&shared, "the window closing", |ed| {
        (ed.frame().windows.len() == 1).then_some(())
    });
    // ...and a verb the app has to carry out goes down the channel, exactly as
    // `(magit "status")` does — one name, one route, whichever spelling.
    lisp.eval(r#"(call-command "magit-status")"#.into());
    wait_for(&rx, &mut seen, "Git(status)", |c| {
        *c == EditorCommand::Git("status".into())
    });
    // A name that is not a built-in verb falls through to the image, which is
    // what `M-x` does with anything a config defined.
    lisp.eval(r#"(call-command "no-such-verb-here")"#.into());
    wait_for(&rx, &mut seen, "the fallthrough", |c| {
        *c == EditorCommand::CallLisp("(no-such-verb-here)".into())
    });

    // --- a keystroke for the shell ------------------------------------------
    //
    // Spelled the way `key-bindings` reports one, so the string read out of a
    // binding is the string that can be sent. There is no second vocabulary.
    lisp.eval(r#"(term-send-key "C-c")"#.into());
    wait_for(&rx, &mut seen, "TermKey(C-c)", |c| {
        *c == EditorCommand::TermKey(Key::Ctrl('c'))
    });
    lisp.eval(r#"(term-send-key "<esc>")"#.into());
    wait_for(&rx, &mut seen, "TermKey(esc)", |c| {
        *c == EditorCommand::TermKey(Key::Esc)
    });
    lisp.eval(r#"(term-send-key "SPC")"#.into());
    wait_for(&rx, &mut seen, "TermKey(space)", |c| {
        *c == EditorCommand::TermKey(Key::Char(' '))
    });
    // A token no key spells reports rather than sending something arbitrary.
    lisp.eval(r#"(term-send-key "C-hello")"#.into());
    wait_message(&shared, "an unspellable key", |m| m == "no such key: C-hello");

    // --- read-string: the answer reaches a Lisp closure ---------------------
    //
    // The whole point of the exercise. `read-string` returns *before the user
    // has typed anything*, so there is nothing for it to return — the callback
    // is parked and called later, on a turn of the Lisp queue of its own.
    lisp.eval(
        r#"(read-string "Name: " (lambda (a) (message (format nil "hello ~a" a))))"#.into(),
    );
    wait_prompt(&shared, false);
    {
        // No candidate box for a plain read: there is nothing to put in one.
        let ed = shared.lock().unwrap();
        assert!(!ed.prompt.as_ref().unwrap().kind.completes());
        assert_eq!(ed.prompt.as_ref().unwrap().label, "Name: ");
    }
    type_text(&shared, &lisp, "ada");
    feed(&shared, &lisp, &[Key::Enter]);
    wait_message(&shared, "the read-string callback", |m| m == "hello ada");
    wait(&shared, "the prompt closing", |ed| ed.prompt.is_none().then_some(()));

    // Cancelling has to call the continuation too, or the closure sits in the
    // table forever and the command that asked never finishes.
    lisp.eval(
        r#"(read-string "Name: " (lambda (a) (message (format nil "cancelled ~a" a))))"#.into(),
    );
    wait_prompt(&shared, false);
    feed(&shared, &lisp, &[Key::Esc]);
    wait_message(&shared, "the cancellation", |m| m == "cancelled NIL");

    // An answer with a quote and a backslash in it survives the trip: it crosses
    // as Lisp *source*, so it has to be escaped for READ.
    lisp.eval(r#"(read-string "s: " (lambda (a) (message (format nil "got ~a" a))))"#.into());
    wait_prompt(&shared, false);
    type_text(&shared, &lisp, r#"a"b\c"#);
    feed(&shared, &lisp, &[Key::Enter]);
    wait_message(&shared, "an escaped answer", |m| m == r#"got a"b\c"#);

    // --- completing-read ----------------------------------------------------
    //
    // The same continuation with a candidate list and the fuzzy matcher every
    // other picker uses.
    lisp.eval(
        r#"(completing-read "Pick: " '("alpha" "beta" "gamma")
             (lambda (a) (message (format nil "picked ~a" a))))"#
            .into(),
    );
    wait_prompt(&shared, true);
    wait(&shared, "the candidates", |ed| {
        ed.prompt.as_ref().filter(|p| p.items.len() == 3).map(|_| ())
    });
    {
        // Order is the order Lisp gave, which is what a hand-written list means.
        let ed = shared.lock().unwrap();
        let p = ed.prompt.as_ref().unwrap();
        assert!(p.kind.completes());
        assert_eq!(p.current(), Some("alpha"));
        assert_eq!(p.matches.len(), 3);
    }
    // Narrow to one and take it.
    type_text(&shared, &lisp, "gam");
    feed(&shared, &lisp, &[Key::Enter]);
    wait_message(&shared, "the completing-read callback", |m| m == "picked gamma");

    // Nothing is required to match: with no hit the answer is what was typed,
    // which is Emacs' `require-match` NIL and the useful default.
    lisp.eval(
        r#"(completing-read "Pick: " '("alpha" "beta")
             (lambda (a) (message (format nil "free ~a" a))))"#
            .into(),
    );
    wait_prompt(&shared, true);
    wait(&shared, "the candidates", |ed| {
        ed.prompt.as_ref().filter(|p| p.items.len() == 2).map(|_| ())
    });
    type_text(&shared, &lisp, "zzz");
    feed(&shared, &lisp, &[Key::Enter]);
    wait_message(&shared, "an unmatched answer", |m| m == "free zzz");

    // A reply to a continuation nobody is waiting for is dropped in silence —
    // the same rule `%rpc-event` follows for a response to a dead request.
    lisp.eval("(%prompt-reply 99999 \"stale\")".into());
    says(&shared, &lisp, "(hash-table-count *prompt-continuations*)", "0");

    // A callback that signals costs you that one answer, not the editor.
    lisp.eval(r#"(read-string "x: " (lambda (a) (error "boom ~a" a)))"#.into());
    wait_prompt(&shared, false);
    type_text(&shared, &lisp, "q");
    feed(&shared, &lisp, &[Key::Enter]);
    wait_message(&shared, "the handler error", |m| {
        m.starts_with("prompt handler error:")
    });
    says(&shared, &lisp, "(progn (message \"still alive\") nil)", "NIL");

    // --- and the *Messages* buffer, which is what all of this was for --------
    //
    // Five lines of Lisp over `create-buffer` and the `messages` reader, with
    // nothing new in Rust. Copied verbatim from `runtime/init.lisp`, so the form
    // that ships is the form under test — the config file itself cannot be
    // loaded here, since it pulls in the whole of `runtime/modes/`.
    lisp.eval(
        r#"(defun messages-buffer ()
             "Show the message log in a buffer, newest at the bottom."
             (let ((log (messages)))
               (create-buffer "*Messages*")
               (replace-region 0 (point-max)
                               (if log
                                   (format nil "~{~a~%~}" log)
                                   "no messages yet"))
               (goto-char (point-max))
               (message (format nil "~a message~:p" (length log)))))"#
            .into(),
    );
    lisp.eval("(messages-buffer)".into());
    wait(&shared, "*Messages*", |ed| {
        (ed.buffer.name() == "*Messages*" && ed.buffer.text.to_string().contains("still alive"))
            .then_some(())
    });
    // Running it again rewrites the one buffer rather than stacking another.
    lisp.eval("(messages-buffer)".into());
    says(&shared, &lisp, "(count \"*Messages*\" (buffer-names) :test #'string=)", "1");
}
