//! AI mode: the harness list, the menu, and the verb they produce.
//!
//! Everything in `runtime/modes/ai.lisp` is Common Lisp on top of primitives the
//! editor already exports, so the only honest way to check it is to boot the
//! real image against the real config and watch what comes out — which is what
//! this does. It loads `runtime/init.lisp` itself rather than a stub, because
//! the thing being proved is partly *that* the config loads it.
//!
//! **None of the three coding agents is a build dependency.** The pure half
//! below runs everywhere. The half that checks a real `--help` skips per binary
//! when it is not installed, exactly as `lsp_live.rs` does for language
//! servers — a harness is not something a test suite may require.
//!
//! Deliberately one `#[test]`: `cl_boot` initialises a process-wide image, so
//! there is exactly one `spawn` per test binary.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Key, PromptKind, Shared};

const PATIENCE: Duration = Duration::from_secs(30);

/// Transcribed from `crates/lisp/tests/prompt.rs`: keys go through the real
/// `handle_key`, and the one command core cannot perform itself — calling the
/// image back with the answer — is handed to the Lisp thread here.
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
    feed(shared, lisp, &text.chars().map(Key::Char).collect::<Vec<_>>());
}

/// Wait for a prompt whose answer goes back to Lisp, and answer `f` about it.
fn wait_prompt<T>(shared: &Shared, what: &str, f: impl Fn(&Editor) -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        {
            let ed = shared.lock().unwrap();
            let lisp_prompt = ed
                .prompt
                .as_ref()
                .is_some_and(|p| matches!(p.kind, PromptKind::Lisp { .. }));
            if lisp_prompt {
                if let Some(v) = f(&ed) {
                    return v;
                }
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}; prompt={:?}", ed.prompt.as_ref().map(|p| &p.label));
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The first message *after* `from` that `pred` accepts. The index matters: the
/// same tag is asked more than once below, and matching a stale answer is a
/// test that passes for the wrong reason.
fn wait_message(
    shared: &Shared,
    from: usize,
    what: &str,
    pred: impl Fn(&str) -> bool,
) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        {
            let ed = shared.lock().unwrap();
            if let Some(m) = ed.messages.get(from..).and_then(|m| m.iter().find(|m| pred(m))) {
                return m.clone();
            }
            if Instant::now() >= deadline {
                let seen = &ed.messages[ed.messages.len().saturating_sub(12)..];
                panic!("timed out waiting for {what}; last messages {seen:#?}");
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Ask the image a question and get the answer back as a message. Everything
/// the image does is asynchronous, so a read is a wait.
fn probe(lisp: &zemacs_lisp::Lisp, shared: &Shared, tag: &str, form: &str) -> String {
    let from = shared.lock().unwrap().messages.len();
    lisp.eval(format!(
        "(message (format nil \"{tag} ~a\" (handler-case {form} (error (e) (format nil \"ERROR ~a\" e)))))"
    ));
    let prefix = format!("{tag} ");
    let line = wait_message(shared, from, tag, move |m| m.starts_with(&prefix));
    line[tag.len() + 1..].to_string()
}

#[test]
fn ai_mode_is_data_in_lisp() {
    let init = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp"),
    )
    .expect("runtime/init.lisp must exist");

    let (tx, rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);

    // The config loads `modes/ai.lisp` at all. Without this line every failure
    // below would look like a bug in this file rather than a missing `load`.
    wait_message(&shared, 0, "init.lisp to finish loading", |m| {
        m.contains("is driving the editor")
    });
    assert_eq!(
        probe(&lisp, &shared, "ai-loaded", "(if (fboundp 'ai) \"yes\" \"no\")"),
        "yes",
        "runtime/init.lisp must load modes/ai.lisp"
    );

    // --- the harness list ---------------------------------------------------
    //
    // Pinned, because these are the *real* flags read out of each tool's own
    // `--help` and the failure mode of getting one wrong is a session that dies
    // on startup with an unhelpful message. The live half below is what keeps
    // them true.
    let names = probe(
        &lisp,
        &shared,
        "names",
        "(format nil \"~{~a~^,~}\" (mapcar #'first *ai-harnesses*))",
    );
    assert_eq!(names, "claude,cursor,opencode");

    let programs = probe(
        &lisp,
        &shared,
        "programs",
        "(format nil \"~{~a~^,~}\" (mapcar #'second *ai-harnesses*))",
    );
    assert_eq!(
        programs, "claude,cursor-agent,opencode",
        "cursor's CLI is `cursor-agent`; `cursor` is the GUI"
    );

    // --- executable-find ----------------------------------------------------
    assert_ne!(
        probe(&lisp, &shared, "sh", "(if (executable-find \"sh\") \"y\" \"n\")"),
        "n",
        "sh is on $PATH on every machine this builds on"
    );
    assert_eq!(
        probe(
            &lisp,
            &shared,
            "nope",
            "(if (executable-find \"zemacs-no-such-harness-9f3c\") \"y\" \"n\")"
        ),
        "n"
    );

    // --- a fourth harness is one line ---------------------------------------
    probe(
        &lisp,
        &shared,
        "added",
        "(ai-add-harness \"aider\" \"aider\" nil '(\"--restore-chat-history\"))",
    );
    assert_eq!(
        probe(
            &lisp,
            &shared,
            "names2",
            "(format nil \"~{~a~^,~}\" (mapcar #'first *ai-harnesses*))"
        ),
        "claude,cursor,opencode,aider"
    );
    // ...and adding it twice does not double it, so `reload-config` is safe.
    probe(&lisp, &shared, "again", "(ai-add-harness \"aider\" \"aider\")");
    assert_eq!(
        probe(
            &lisp,
            &shared,
            "names3",
            "(format nil \"~{~a~^,~}\" (mapcar #'first *ai-harnesses*))"
        ),
        "claude,cursor,opencode,aider"
    );
    probe(
        &lisp,
        &shared,
        "removed",
        "(progn (setf *ai-harnesses* (remove \"aider\" *ai-harnesses* :key #'first :test #'string=)) \"ok\")",
    );

    // --- the verb each harness produces -------------------------------------
    //
    // Read rather than run, so this holds on a machine with none of the three
    // installed. These strings are the entire interface between this config and
    // the editor.
    for (name, resume, want) in [
        ("claude", "t", "terminal-run:claude:claude -r"),
        ("claude", "nil", "terminal-run:claude:claude"),
        ("cursor", "t", "terminal-run:cursor:cursor-agent --resume"),
        ("cursor", "nil", "terminal-run:cursor:cursor-agent"),
        ("opencode", "t", "terminal-run:opencode:opencode --continue"),
        ("opencode", "nil", "terminal-run:opencode:opencode"),
    ] {
        let got = probe(
            &lisp,
            &shared,
            "verb",
            &format!("(%ai-verb (assoc {name:?} *ai-harnesses* :test #'string=) {resume})"),
        );
        assert_eq!(got, want, "{name} resume={resume}");
    }

    // --- and end to end, down the channel the app drains ---------------------
    //
    // `Term` needs the app, so the command travels rather than being applied,
    // and what arrives here is exactly what `crates/app/src/term.rs` parses.
    // Run against `sh` so the test does not require a harness to be installed —
    // `%ai-start` refuses a program that is not on $PATH, which is the point.
    while rx.try_recv().is_ok() {} // whatever the config did on the way up
    probe(
        &lisp,
        &shared,
        "added2",
        "(ai-add-harness \"probe\" \"sh\" nil '(\"-c\" \"true\"))",
    );
    probe(
        &lisp,
        &shared,
        "started",
        "(progn (%ai-start (assoc \"probe\" *ai-harnesses* :test #'string=) t) \"ok\")",
    );
    let verb = loop {
        match rx.recv_timeout(PATIENCE) {
            Ok(EditorCommand::Term(verb)) => break verb,
            Ok(_) => continue,
            Err(e) => panic!("no terminal verb arrived: {e}"),
        }
    };
    assert_eq!(verb, "run:probe:sh -c true");

    // A harness that is not installed says so and sends nothing — the whole
    // reason the menu can offer one it cannot run.
    probe(
        &lisp,
        &shared,
        "missing",
        "(progn (ai-add-harness \"ghost\" \"zemacs-no-such-harness-9f3c\") \
                (%ai-start (assoc \"ghost\" *ai-harnesses* :test #'string=) nil) \"ok\")",
    );
    wait_message(&shared, 0, "the not-installed message", |m| {
        m.contains("ghost is not installed")
    });
    assert!(
        rx.try_recv().is_err(),
        "nothing may be spawned for a harness that is not there"
    );

    // --- the menu itself ----------------------------------------------------
    //
    // `C-a` in one gesture: the harness picker, then new-or-resume, then a verb
    // on the channel. `probe` is not used here because `(ai)` answers before
    // anything has been typed — the whole reason the prompt is a continuation.
    while rx.try_recv().is_ok() {}
    lisp.eval("(ai)".into());
    let labels = wait_prompt(&shared, "the harness picker", |ed| {
        let p = ed.prompt.as_ref()?;
        (p.items.len() >= 4).then(|| p.items.clone())
    });
    assert!(labels.iter().any(|l| l == "claude"), "{labels:?}");
    assert!(labels.iter().any(|l| l == "cursor"), "{labels:?}");
    assert!(labels.iter().any(|l| l == "opencode"), "{labels:?}");
    // The one whose binary does not exist is offered, and says so.
    assert!(
        labels.iter().any(|l| l.starts_with("ghost  (not installed:")),
        "a missing harness is offered with a reason, not hidden: {labels:?}"
    );

    type_text(&shared, &lisp, "probe");
    feed(&shared, &lisp, &[Key::Enter]);
    // The second question comes up from inside the first one's callback.
    wait_prompt(&shared, "the new-or-resume picker", |ed| {
        let p = ed.prompt.as_ref()?;
        (p.label == "probe: " && p.items == ["new", "resume"]).then_some(())
    });
    type_text(&shared, &lisp, "resume");
    feed(&shared, &lisp, &[Key::Enter]);
    let verb = loop {
        match rx.recv_timeout(PATIENCE) {
            Ok(EditorCommand::Term(verb)) => break verb,
            Ok(_) => continue,
            Err(e) => panic!("the menu produced no terminal verb: {e}"),
        }
    };
    assert_eq!(verb, "run:probe:sh -c true");

    // --- the path parser ----------------------------------------------------
    //
    // What "jump to a file the agent mentioned" is actually made of.
    for (line, col, want) in [
        ("edited crates/app/src/term.rs:42 for you", 12, "crates/app/src/term.rs:42"),
        // prose punctuation on both ends
        ("see (src/main.rs:7),", 8, "src/main.rs:7"),
        // no line number
        ("wrote README.org", 12, "README.org"),
    ] {
        let got = probe(
            &lisp,
            &shared,
            "token",
            &format!("(%ai-clean-path (%ai-token-at {line:?} {col}))"),
        );
        assert_eq!(got, want, "token from {line:?} at {col}");
    }
    // A blank column has nothing under it, and must not be an error.
    assert_eq!(
        probe(&lisp, &shared, "blank", "(format nil \"[~a]\" (%ai-token-at \"a  b\" 1))"),
        "[]"
    );

    // --- live: the flags are still the flags --------------------------------
    //
    // Skipped per harness when the binary is absent. This is the only thing
    // that can catch a tool renaming its resume flag out from under the config,
    // and it costs one `--help` rather than a session.
    let mut checked = 0;
    for (program, flag) in [
        ("claude", "--resume"),
        ("cursor-agent", "--resume"),
        ("opencode", "--continue"),
    ] {
        let Some(path) = which(program) else {
            eprintln!("skipping {program}: not installed");
            continue;
        };
        let out = std::process::Command::new(&path)
            .arg("--help")
            .output()
            .unwrap_or_else(|e| panic!("{program} --help: {e}"));
        // Some of them print help on stderr, some on stdout, one prints a
        // banner on both.
        let mut help = String::from_utf8_lossy(&out.stdout).into_owned();
        help.push_str(&String::from_utf8_lossy(&out.stderr));
        assert!(
            help.contains(flag),
            "{program} no longer documents {flag} — runtime/modes/ai.lisp needs updating"
        );
        checked += 1;
    }
    eprintln!("checked {checked}/3 harness CLIs");
}

/// `which`, again, because a test binary may not depend on `zemacs-term`.
fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join(program))
        .find(|p| p.is_file())
}
