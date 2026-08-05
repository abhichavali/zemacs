//! corfu, end to end — a server's answer becoming a box you can accept from.
//!
//! Written for `which_key.rs`'s reason and against the same failure: every link
//! in this feature has a unit test somewhere, and a chain is not tested by
//! testing links. The links:
//!
//! 1. the **image** turns a decoded `textDocument/completion` reply into rows —
//!    which means `%lsp-completion-prefix` reading the word behind point,
//!    `%lsp-completion-items` accepting either reply shape, and the prefix
//!    filter throwing away what the server offered and we no longer match;
//! 2. those rows reach `Editor::completion()`, which is what the renderer draws;
//! 3. the *shipped* `init.lisp` binds `C-n`/`C-y` in **Insert mode**, so a
//!    keystroke reaches `lsp-complete-next` at all — the link that was dead in
//!    which-key and would be dead here, since nothing else in the editor binds
//!    a bare Ctrl in Insert and a missing binding is silence rather than error;
//! 4. accepting replaces exactly `[anchor, point)` in the document;
//! 5. and core retires the popup on its own, with no Lisp involved, the moment
//!    point stops being in the word it describes.
//!
//! What is *not* here is a language server. The socket is the one piece that
//! cannot run in a test on a machine with no `pylsp` on it, so the reply is
//! injected at `%lsp-completion-reply` — which is also exactly where the
//! staleness rules live, and they are asserted rather than assumed.
//!
//! Deliberately a single `#[test]`, like every other file here: `cl_boot`
//! initialises a process-wide image, so there is one `spawn` per test binary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zemacs_core::{Buffer, Editor, EditorCommand, Key, Mode, Shared};

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

/// Press `key` the way the application does: core turns it into commands, and a
/// Lisp call is the one it cannot carry out itself.
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

/// The rows on screen, which is what the renderer would draw.
fn rows(shared: &Shared) -> Vec<String> {
    shared
        .lock()
        .unwrap()
        .completion()
        .map(|c| c.rows.clone())
        .unwrap_or_default()
}

/// A decoded `CompletionList`, in the alist shape `rpc.lisp` hands `jget`.
/// `format` carries a `detail` and the other two do not, so the row a server
/// gets drawn for it is visibly not its `label`.
const REPLY: &str = r#"'(("isIncomplete" . nil)
    ("items" . ((("label" . "format") ("detail" . "fn(&str)"))
                (("label" . "foo_bar"))
                (("label" . "zzz")))))"#;

#[test]
fn a_completion_reply_becomes_a_popup_and_a_keystroke_puts_one_in_the_buffer() {
    // The *shipped* config, not a synthetic one: `lsp.lisp` is loaded out of
    // the block near the foot of `init.lisp`, and the Insert-mode bindings are
    // defined below that behind an `fboundp` guard. A test with three
    // hand-written `define-key` calls would pass with the real editor dead.
    let entry = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp"),
    )
    .expect("runtime/init.lisp must exist");

    let init = std::env::temp_dir().join("zemacs_test_completion_init.lisp");
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(message "completion test init loaded")
"#,
            entry.display().to_string()
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = Arc::new(zemacs_lisp::spawn(tx, shared.clone(), init));

    wait(&shared, "lsp.lisp to load", |ed| {
        ed.messages
            .iter()
            .any(|m| m == "completion test init loaded")
            .then_some(())
    });
    // Bindings arrive asynchronously, so the keystroke below has to wait for
    // them or it tests an empty keymap. This one *is* the assertion for link 3:
    // if `lsp.lisp` failed to load, or the `fboundp` guard around the Insert
    // bindings never fired, the feature is unreachable from the keyboard and
    // this is where that shows up.
    wait(&shared, "the corfu Insert bindings to arrive", |ed| {
        ["C-n", "C-p", "C-y", "C-e"]
            .iter()
            .all(|k| ed.keymap.contains_key(&(Mode::Insert, k.to_string())))
            .then_some(())
    });

    let path = PathBuf::from("/tmp/zemacs_completion_test.py");

    // --- the anchor arithmetic ----------------------------------------------
    //
    // Buffer text reaches Lisp as UTF-8 *bytes* while `(point)` counts
    // characters, and the whole reason the word charset is ASCII is that a scan
    // back over bytes then counts characters exactly. Asserted rather than
    // argued: `café_fo` has a two-byte letter in front of the anchor, so a
    // naive byte count would put the anchor one character to the left and
    // `accept` would eat the `é`.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer = Buffer::from_str("x = café_fo\n");
        ed.buffer.path = Some(PathBuf::from("/tmp/zemacs_completion_utf8.py"));
        ed.mode = Mode::Insert;
        ed.buffer.cursor = 11; // just after the final `o`
    }
    lisp.eval(
        "(let ((w (%lsp-completion-prefix)))
           (message (format nil \"anchor ~a ~a\" (car w) (cdr w))))"
            .to_string(),
    );
    let anchor = wait(&shared, "the prefix reader", |ed| {
        ed.messages.iter().rev().find(|m| m.starts_with("anchor ")).cloned()
    });
    assert_eq!(
        anchor, "anchor 8 _fo",
        "8 is the character offset of `_`, which is where the ASCII run starts \
         — a byte count would have said 7 and pointed at the tail of `é`"
    );

    // And the hook is safe to fire on a buffer with no server at all, which is
    // every keystroke in every other buffer. `after-change-hook` swallows errors
    // per function, so a broken one here would be invisible in the editor —
    // hence running it in front of a message that only appears if it returned.
    lisp.eval("(progn (lsp-complete-maybe) (message \"hook survived\"))".to_string());
    wait(&shared, "the after-change hook", |ed| {
        ed.messages.iter().any(|m| m == "hook survived").then_some(())
    });

    // A file buffer mid-word: `fo` typed, point after it, so the anchor the
    // image should find on its own is 4.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer = Buffer::from_str("x = fo\n");
        ed.buffer.path = Some(path.clone());
        ed.buffer.major_mode = "python-mode".into();
        ed.mode = Mode::Insert;
        ed.buffer.cursor = 6;
    }

    // --- links 1 and 2: a reply becomes rows -------------------------------
    //
    // Seeded the way `%lsp-completion-request` seeds it and then answered, which
    // is precisely the state the socket would have left behind.
    lisp.eval(format!(
        "(progn (setf *lsp-completion*
                      (list :at 4 :path (buffer-file-name)
                            :items nil :shown nil :index 0))
                (%lsp-completion-reply (buffer-file-name) 4 {REPLY} nil))"
    ));
    let drawn = wait(&shared, "the reply to reach the popup", |ed| {
        ed.completion().filter(|c| !c.rows.is_empty()).map(|c| c.rows.clone())
    });
    assert_eq!(
        drawn,
        vec!["format  fn(&str)".to_string(), "foo_bar".to_string()],
        "the two candidates that still start with `fo', in the server's order, \
         and `detail' drawn beside the label it belongs to"
    );
    assert_eq!(
        shared.lock().unwrap().completion().map(|c| c.at),
        Some(4),
        "anchored at the start of the word, which the image found for itself"
    );

    // A prefix that matches nothing takes the *box* down and keeps the
    // candidates. Both halves matter: dropping them here would make every
    // further keystroke re-ask the server about the same position, and — while
    // a request is still in flight — would throw away the plist its reply is
    // checked against, so typing a third character would cancel the request the
    // second one triggered.
    lisp.eval("(%lsp-completion-filter \"zq\")".to_string());
    wait(&shared, "the popup to go quiet", |ed| {
        ed.completion().is_none().then_some(())
    });
    lisp.eval("(%lsp-completion-filter \"fo\")".to_string());
    wait(&shared, "one backspace to bring it back", |ed| {
        (ed.completion().map(|c| c.rows.len()) == Some(2)).then_some(())
    });

    // --- link 3: the keyboard reaches it -----------------------------------
    let cmds = press(&shared, &lisp, Key::Ctrl('n'));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, EditorCommand::CallLisp(s) if s.contains("lsp-complete-next"))),
        "`C-n` in Insert is bound by the shipped config; got {cmds:?}"
    );
    wait(&shared, "the selection to move", |ed| {
        (ed.completion().map(|c| c.selected) == Some(1)).then_some(())
    });
    assert_eq!(
        rows(&shared).len(),
        2,
        "moving the selection must not cost the candidate list"
    );

    // --- link 4: accepting rewrites the word -------------------------------
    press(&shared, &lisp, Key::Ctrl('y'));
    wait(&shared, "the candidate to land in the buffer", |ed| {
        (ed.buffer.text.to_string() == "x = foo_bar\n").then_some(())
    });
    assert!(
        shared.lock().unwrap().completion().is_none(),
        "and the popup goes with it"
    );
    assert_eq!(
        shared.lock().unwrap().buffer.cursor,
        11,
        "point lands after what was inserted, so you go on typing at the end of \
         the word rather than in front of it"
    );

    // --- the staleness rules -----------------------------------------------
    //
    // These are the whole of what makes an asynchronous reply safe, so they are
    // asserted rather than trusted. Point is put back mid-word each time.
    let reseat = |at: usize| {
        let mut ed = shared.lock().unwrap();
        ed.buffer = Buffer::from_str("x = fo\n");
        ed.buffer.path = Some(path.clone());
        ed.mode = Mode::Insert;
        ed.buffer.cursor = at;
    };

    // A reply for an anchor we are no longer asking about is dropped. Nothing
    // is retried: the next keystroke asks again.
    reseat(6);
    lisp.eval(format!(
        "(progn (setf *lsp-completion*
                      (list :at 4 :path (buffer-file-name)
                            :items nil :shown nil :index 0))
                (%lsp-completion-reply (buffer-file-name) 99 {REPLY} nil)
                (message \"stale anchor handled\"))"
    ));
    wait(&shared, "the stale-anchor reply", |ed| {
        ed.messages.iter().any(|m| m == "stale anchor handled").then_some(())
    });
    assert!(
        shared.lock().unwrap().completion().is_none(),
        "an answer to a question we stopped asking must not put a box up"
    );

    // A reply for another file is dropped, however current its anchor.
    lisp.eval(format!(
        "(progn (setf *lsp-completion*
                      (list :at 4 :path (buffer-file-name)
                            :items nil :shown nil :index 0))
                (%lsp-completion-reply \"/tmp/somewhere-else.py\" 4 {REPLY} nil)
                (message \"stale path handled\"))"
    ));
    wait(&shared, "the wrong-file reply", |ed| {
        ed.messages.iter().any(|m| m == "stale path handled").then_some(())
    });
    assert!(shared.lock().unwrap().completion().is_none());

    // A reply that arrives after point has left the word is dropped — the check
    // that matters most, because it is the one the *user* causes by typing on.
    // The anchor still matches; the buffer no longer has a word starting there.
    reseat(3);
    lisp.eval(format!(
        "(progn (setf *lsp-completion*
                      (list :at 4 :path (buffer-file-name)
                            :items nil :shown nil :index 0))
                (%lsp-completion-reply (buffer-file-name) 4 {REPLY} nil)
                (message \"moved point handled\"))"
    ));
    wait(&shared, "the reply for a word point has left", |ed| {
        ed.messages.iter().any(|m| m == "moved point handled").then_some(())
    });
    assert!(
        shared.lock().unwrap().completion().is_none(),
        "point walked off the word while the server was thinking"
    );

    // --- link 5: core retires it with no Lisp involved ----------------------
    reseat(6);
    lisp.eval(format!(
        "(progn (setf *lsp-completion*
                      (list :at 4 :path (buffer-file-name)
                            :items nil :shown nil :index 0))
                (%lsp-completion-reply (buffer-file-name) 4 {REPLY} nil))"
    ));
    wait(&shared, "a popup to put back up", |ed| {
        ed.completion().filter(|c| !c.rows.is_empty()).map(|_| ())
    });
    // Esc leaves Insert. Nothing calls the image on that key — as with
    // which-key, this is the only way it can work.
    press(&shared, &lisp, Key::Esc);
    assert!(
        shared.lock().unwrap().completion().is_none(),
        "leaving Insert takes the popup with it, without asking Lisp"
    );
    // ...and it stays gone when Insert comes back, which is the difference
    // between hidden and forgotten.
    press(&shared, &lisp, Key::Char('i'));
    assert!(
        shared.lock().unwrap().completion().is_none(),
        "a retired popup does not come back with the mode that hid it"
    );
}
