//! Headless proof of the JSON-RPC bridge: a Lisp author can drive an arbitrary
//! JSON-RPC child, and an answer to a request made from Lisp reaches a Lisp
//! closure.
//!
//! The fake child is `cat`, which copies its stdin to its stdout byte for byte.
//! That turns the connection into a mirror, and a mirror exercises every part of
//! the path at once:
//!
//! - a *notification* sent from Lisp comes back as a notification, so the
//!   `:on-notify` handler fires;
//! - a *request* sent from Lisp comes back looking like a request from the
//!   child, so `rpc.lisp`'s "answer every request, even to refuse it" rule fires
//!   and sends an error response — which comes back a second time as a
//!   *response* carrying the id we allocated, matches the pending table, and
//!   calls the closure parked under it.
//!
//! So one round trip proves framing in both directions, id allocation, id
//! matching, and the whole async-reply route. Nothing here needs a language
//! server, or any binary that is not on every unix.
//!
//! This file also plays the part of the main loop, which is the other half of
//! the answer to "how does a reply reach Lisp": `pump` is a transcription of the
//! drain in `crates/app/src/main.rs`, and if the two ever disagree this test is
//! the one that notices.
//!
//! Deliberately a single `#[test]`, as in the files beside it: `cl_boot`
//! initialises a process-wide image, so a new file is the only way to add one.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use zemacs_core::{Editor, Shared};

const PATIENCE: Duration = Duration::from_secs(20);

/// What the main loop does with a child's output, transcribed. The form is
/// identical, deliberately.
fn pump(lisp: &zemacs_lisp::Lisp) {
    while let Some((conn, event)) = zemacs_rpc::poll() {
        let (kind, form) = match event {
            zemacs_rpc::Event::Message(v) => (":message", zemacs_rpc::lisp::to_lisp(&v)),
            zemacs_rpc::Event::Protocol(e) => (":error", zemacs_rpc::lisp::string(&e)),
            zemacs_rpc::Event::Exited(e) => (":exit", zemacs_rpc::lisp::string(&e)),
        };
        lisp.eval(format!(
            "(let ((h (find-symbol \"%RPC-EVENT\" :zemacs))) \
               (when (and h (fboundp h)) (funcall h {conn} {kind} '{form})))"
        ));
    }
}

fn wait_message(shared: &Shared, lisp: &zemacs_lisp::Lisp, what: &str, pred: impl Fn(&str) -> bool) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        pump(lisp);
        {
            let ed = shared.lock().unwrap();
            if ed.messages.iter().any(|m| pred(m)) {
                return;
            }
            if Instant::now() >= deadline {
                let seen = &ed.messages[ed.messages.len().saturating_sub(10)..];
                panic!("timed out waiting for {what}; last messages {seen:#?}");
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Evaluate `form` and wait for its value to be reported. The workhorse: most
/// assertions here are "compute this in Lisp and tell me what it was".
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"~a\" {form}))"));
    wait_message(shared, lisp, form, |m| m == want);
}

fn runtime(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .expect("the runtime directory ships with the source")
}

#[test]
fn lisp_can_drive_a_json_rpc_child() {
    let init = std::env::temp_dir().join("zemacs_test_rpc_init.lisp");
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n(load {:?} :verbose nil :print nil)\n\
             (message \"rpc test init loaded\")\n",
            runtime("rpc.lisp")
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    wait_message(&shared, &lisp, "the init file", |m| m == "rpc test init loaded");

    // --- encoding ---------------------------------------------------------
    //
    // Decoding is Rust's; this is the half that stays in Lisp, because building
    // a request *is* protocol. The shapes below are exactly the ones an LSP
    // request is made of.
    says(&shared, &lisp, r#"(json-encode (jobj "line" 3 "character" 0))"#,
         r#"{"line":3,"character":0}"#);
    says(&shared, &lisp, r#"(json-encode (jarr 1 2 3))"#, "[1,2,3]");
    // An array of objects is the shape the inference has to get right, and the
    // one a naive alist test gets wrong.
    says(&shared, &lisp, r#"(json-encode (jarr (jobj "text" "hi")))"#,
         r#"[{"text":"hi"}]"#);
    says(&shared, &lisp, "(json-encode nil)", "null");
    says(&shared, &lisp, "(json-encode t)", "true");
    says(&shared, &lisp, "(json-encode :false)", "false");
    says(&shared, &lisp, "(json-encode :empty-object)", "{}");
    // The one part of encoding that is a primitive, because the string it
    // escapes most often is a whole buffer.
    says(&shared, &lisp, r#"(json-encode "a\"b")"#, r#""a\"b""#);
    says(&shared, &lisp, r#"(json-encode (jobj "a" (jobj "b" (jarr "x"))))"#,
         r#"{"a":{"b":["x"]}}"#);

    // --- reading a decoded message ----------------------------------------
    //
    // `jget` never signals: a key that is not there, and a value that is not an
    // object, both answer NIL. That is what lets a handler walk a message it has
    // never seen the shape of.
    says(&shared, &lisp,
         r#"(jget '(("range" . (("start" . (("line" . 7)))))) "range" "start" "line")"#,
         "7");
    says(&shared, &lisp, r#"(jget '(("a" . 1)) "b")"#, "NIL");
    says(&shared, &lisp, r#"(jget '(("a" . 1)) "a" "b" "c")"#, "NIL");
    says(&shared, &lisp, r#"(jget 42 "a")"#, "NIL");

    // --- a connection ------------------------------------------------------
    //
    // `cat` is a mirror, so everything sent comes straight back and the whole
    // path is exercised without a language server anywhere near it.
    lisp.eval(
        r#"(defparameter *c*
             (rpc-start "cat"
               :name "mirror"
               :on-notify (lambda (c m p) (declare (ignore c))
                            (message (format nil "notify ~a ~a" m (jget p "x"))))))"#
            .into(),
    );
    says(&shared, &lisp, "(and (integerp *c*) (rpc-live-p *c*))", "T");
    says(&shared, &lisp, "(second (first (rpc-connections)))", "mirror");

    // A notification: out, mirrored, and back into the handler.
    lisp.eval(r#"(rpc-notify *c* "hello" (jobj "x" 42))"#.into());
    wait_message(&shared, &lisp, "the notification handler", |m| m == "notify hello 42");

    // A request: out with id 1, mirrored back *as a request* (it has an id and a
    // method), refused by the default responder because this connection declared
    // no `:on-request`, and the refusal is mirrored back *as a response* — which
    // is the message that matches the pending table and calls the closure.
    lisp.eval(
        r#"(rpc-request *c* "ask" (jobj "y" 1)
             (lambda (result error)
               (message (format nil "reply ~a ~a" result (jget error "code")))))"#
            .into(),
    );
    wait_message(&shared, &lisp, "the reply reaching a Lisp closure", |m| {
        m == "reply NIL -32601"
    });

    // Stopping is idempotent, and a send on a dead connection is NIL rather than
    // an error — the same "failure is a value" convention as every reader.
    lisp.eval("(rpc-stop *c*)".into());
    says(&shared, &lisp, "(rpc-live-p *c*)", "NIL");
    says(&shared, &lisp, r#"(rpc-notify *c* "hello")"#, "NIL");
    says(&shared, &lisp, r#"(rpc-request *c* "ask" nil (lambda (r e) r e))"#, "NIL");
    lisp.eval("(rpc-stop *c*)".into());

    // --- a child that will not start ---------------------------------------
    //
    // A config naming a program you have not installed has to load anyway.
    says(&shared, &lisp, r#"(rpc-start "zemacs-no-such-program-x9")"#, "NIL");
    wait_message(&shared, &lisp, "the report", |m| {
        m.starts_with("rpc-start zemacs-no-such-program-x9:")
    });

    // --- a child that dies --------------------------------------------------
    //
    // The exit handler is how Lisp learns its server is gone; without it a
    // session would sit there forever sending into a closed pipe.
    lisp.eval(
        r#"(defparameter *d*
             (rpc-start "sh" :args (list "-c" "echo bye >&2; exit 1")
               :on-exit (lambda (c report) (declare (ignore c))
                          (message (format nil "gone ~a" (search "bye" report))))))"#
            .into(),
    );
    wait_message(&shared, &lisp, "the exit handler", |m| m.starts_with("gone "));
    says(&shared, &lisp, "(rpc-live-p *d*)", "NIL");

    // --- malformed params ---------------------------------------------------
    //
    // The trust boundary in the outgoing direction. A half-written message would
    // desynchronise the child for the rest of the session, so it is refused.
    lisp.eval(
        r#"(defparameter *e* (rpc-start "cat"))"#.into(),
    );
    lisp.eval(r#"(%rpc-send *e* "m" "{not json" t)"#.into());
    wait_message(&shared, &lisp, "the refusal", |m| m.starts_with("rpc-send m: malformed params"));
    // ...and the connection still works afterwards.
    lisp.eval(r#"(rpc-request *e* "still-here" nil (lambda (r e) (declare (ignore r e))
                                                     (message "alive")))"#.into());
    wait_message(&shared, &lisp, "the connection surviving", |m| m == "alive");
    lisp.eval("(rpc-stop *e*)".into());

    // Nothing above should have needed the editor at all — the bridge is
    // independent of the document, which is what lets an agent or a typesetting
    // daemon use it next.
    let ed: &Editor = &shared.lock().unwrap();
    assert_eq!(ed.buffer.len_chars(), 0, "the RPC layer must not touch the buffer");
}
