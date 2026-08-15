//! Incremental `didChange`, and the UTF-16 columns it is measured in.
//!
//! Two claims, and they are the same claim twice: **a position that crosses this
//! client is in UTF-16 code units**, and the range in a `contentChanges` entry
//! is a position. `tests/lsp.rs` proves the client against a fake server that
//! advertises full sync, which is the path that has always worked; this one
//! points a fake at the other branch and reads back what actually went down the
//! pipe.
//!
//! The buffer is deliberately `# 😀 café`, which is the only line that can tell
//! the units apart: eight characters, nine UTF-16 code units — the emoji is a
//! surrogate *pair* — and twelve UTF-8 bytes on disk. Lisp holds all eight
//! characters as eight characters, so every number asserted below is one of the
//! first two; on an ASCII line they would be the same number and none of this
//! would be provable.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use zemacs_core::{EditorCommand, Mode, Shared};

const PATIENCE: Duration = Duration::from_secs(20);

const DIR: &str = "/tmp/zemacs_lsp_sync";
/// Eight characters on the first line, and a sentinel on the second that is
/// unique in the whole log — resending it is how a full-text `didChange` is
/// caught pretending to be a ranged one.
const PY_SOURCE: &str = "# 😀 café\nZSENTINELZ = 1\n";
const C_SOURCE: &str = "int main(void) { return 0; }\n";

/// The same eleven lines of `sh` as `tests/lsp.rs`, with the sync kind it
/// advertises as an argument: `$1` is the log to append to and `$2` is the
/// `textDocumentSync` it claims.
const FAKE_SERVER: &str = r#"#!/bin/sh
send() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
send '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"textDocumentSync":'"$2"'}}}'
cat >> "$1"
"#;

/// The drain in `crates/app/src/main.rs`, transcribed — see `tests/rpc.rs`.
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

/// The first message matching `pred`, pumping the RPC channel while it waits —
/// the handshake happens underneath every one of these.
fn wait_message(
    shared: &Shared,
    lisp: &zemacs_lisp::Lisp,
    what: &str,
    pred: impl Fn(&str) -> bool,
) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        pump(lisp);
        {
            let ed = shared.lock().unwrap();
            if let Some(m) = ed.messages.iter().find(|m| pred(m)) {
                return m.clone();
            }
            if Instant::now() >= deadline {
                let seen = &ed.messages[ed.messages.len().saturating_sub(10)..];
                panic!("timed out waiting for {what}; last messages {seen:#?}");
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait(shared: &Shared, lisp: &zemacs_lisp::Lisp, what: &str, pred: impl Fn(&str) -> bool) {
    wait_message(shared, lisp, what, pred);
}

/// Ask the image a question and get the answer back as a message. Everything the
/// image does is asynchronous, so a read is a wait. Transcribed from
/// `tests/org_lists.rs`, with the pump `tests/lsp.rs` needs.
fn says(lisp: &zemacs_lisp::Lisp, shared: &Shared, form: &str, want: &str) {
    let tag = format!("probe{}", shared.lock().unwrap().messages.len());
    lisp.eval(format!(
        "(message (format nil \"{tag} ~a\" \
           (handler-case {form} (error (e) (format nil \"ERROR ~a\" e)))))"
    ));
    let prefix = format!("{tag} ");
    let hit = prefix.clone();
    let line = wait_message(shared, lisp, form, move |m| m.starts_with(&hit));
    assert_eq!(&line[prefix.len()..], want, "{form}");
}

fn after_change(lisp: &zemacs_lisp::Lisp) {
    lisp.eval(
        "(let ((h (find-symbol \"AFTER-CHANGE-HOOK\" :zemacs))) \
           (when (and h (fboundp h)) (funcall h)))"
            .into(),
    );
}

fn runtime(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .expect("the runtime directory ships with the source")
}

fn log(path: &str) -> String {
    let mut text = String::new();
    let _ = std::fs::File::open(path).and_then(|mut f| f.read_to_string(&mut text));
    text
}

fn wait_for_log(lisp: &zemacs_lisp::Lisp, path: &str, needle: &str) {
    let deadline = Instant::now() + PATIENCE;
    while !log(path).contains(needle) {
        pump(lisp);
        assert!(
            Instant::now() < deadline,
            "the server was never sent {needle:?}; log was {:?}",
            log(path)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn changes_go_out_as_ranges_counted_in_utf16() {
    let server = std::env::temp_dir()
        .join(format!("zemacs_fake_lsp_sync-{}.sh", std::process::id()));
    std::fs::write(&server, FAKE_SERVER).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let _ = std::fs::remove_dir_all(DIR);
    std::fs::create_dir_all(DIR).unwrap();
    let py_file = format!("{DIR}/main.py");
    let c_file = format!("{DIR}/main.c");
    let log_inc = format!("{DIR}/incremental.log");
    let log_full = format!("{DIR}/full.log");
    std::fs::write(&py_file, PY_SOURCE).unwrap();
    std::fs::write(&c_file, C_SOURCE).unwrap();
    // A root marker, so both sessions stop here rather than at /tmp.
    std::fs::write(format!("{DIR}/pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();

    let init = std::env::temp_dir()
        .join(format!("zemacs_test_lsp_sync_init-{}.lisp", std::process::id()));
    std::fs::write(
        &init,
        format!(
            // `modes/modes.lisp` first: it is where `*after-change-functions*`
            // and the `after-change-hook' the application calls by name live.
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             ;; Two servers, one program: the only thing that differs is the\n\
             ;; `textDocumentSync' each claims, which is the branch under test.\n\
             (lsp-register-server 'python-mode {server:?} {log_inc:?} \"2\")\n\
             (lsp-register-server 'c-mode {server:?} {log_full:?} \"1\")\n\
             (message \"lsp sync init loaded\")\n",
            runtime("modes/modes.lisp"),
            runtime("rpc.lisp"),
            runtime("lsp.lisp"),
            server = server.to_string_lossy(),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    wait(&shared, &lisp, "the init file", |m| m == "lsp sync init loaded");

    // Nothing is supplied here, and that is the point. Text used to cross the
    // shim twice on the way to a server and be encoded at each crossing, so a
    // server was told `😀` is `ð\u{9f}\u{98}\u{80}` — measured against
    // rust-analyzer, which reported the string literal's value as mojibake and
    // put every column after it on that line eighteen places out. The repair was
    // a decode in `lsp.lisp` and a matching one in `json-string`, and this test
    // carried a local copy of the second while it was missing.
    //
    // Both are gone. The shim decodes what a buffer answers and encodes what
    // goes back, once each, and the bytes asserted against the log below are
    // what that produces with nothing in Lisp arranging it.

    {
        let mut ed = shared.lock().unwrap();
        ed.load(PY_SOURCE, Some(PathBuf::from(&py_file)), Some("python".into()));
    }
    after_change(&lisp);
    wait(&shared, &lisp, "the handshake", |m| {
        m.starts_with("lsp: python-mode ready in /tmp/zemacs_lsp_sync/")
    });
    wait_for_log(&lisp, &log_inc, "textDocument/didOpen");
    // The document the server was told about is the one on disk, byte for byte.
    // This is the end-to-end assertion for the whole encoding boundary: the
    // emoji went out of the editor, through `f_query`, through the JSON encoder
    // and back through `dup_utf8` onto a pipe, and came out as its own four
    // bytes and not as the mojibake spelling of them.
    assert!(
        log(&log_inc).contains("# 😀 café"),
        "the didOpen text is not the buffer's own bytes: {:?}",
        log(&log_inc)
    );

    // --- the three units ----------------------------------------------------
    //
    // `# 😀 café` is eight characters, nine UTF-16 code units and twelve bytes on
    // disk. `line-string` hands over the characters, `column` counts them, and
    // the server is owed the code units — so the line's own length and the
    // editor's own offsets are the same number, which is the whole simplification.
    says(&lisp, &shared, "(length (line-string 1))", "8");
    says(&lisp, &shared, "(- (line-end 1) (line-start 1))", "8");
    says(&lisp, &shared, "(%lsp-utf16-column (line-string 1) 8)", "9");
    // Point on the `a` of `café`: character 5, code unit 6 — the emoji is two.
    says(&lisp, &shared, "(%lsp-utf16-column (line-string 1) 5)", "6");
    says(&lisp, &shared, "(%lsp-char-column (line-string 1) 6)", "5");
    says(&lisp, &shared, "(%lsp-char-column (line-string 1) 9)", "8");
    // A unit landing inside the surrogate pair rounds up to the whole character,
    // which is the only answer an editor with no half-characters can give.
    says(&lisp, &shared, "(%lsp-char-column (line-string 1) 3)", "3");
    // ...and a column past the end of the line clamps rather than running on.
    says(&lisp, &shared, "(%lsp-char-column (line-string 1) 999)", "8");
    // The round trip, which is the property the two directions actually owe
    // each other.
    says(
        &lisp,
        &shared,
        "(let ((l (line-string 1))) \
           (loop for k from 0 to 8 \
                 always (= k (%lsp-char-column l (%lsp-utf16-column l k)))))",
        "T",
    );

    // --- what a change looks like -------------------------------------------
    //
    // Ranges in the *old* document's coordinates, which is why the shadow copy
    // exists: the buffer no longer holds that text by the time anyone asks.
    // Both documents given outright, because the shapes that break a narrowing
    // are not all shapes the buffer has — a last line with no newline after it
    // is the obvious one. NEW is read in a `let*' after OLD, so the buffer-based
    // spellings below still say `old' and mean it.
    let change_of = |old: &str, new: &str| {
        format!(
            "(let* ((old {old}) (new {new}) (c (%lsp-content-change old new))) \
               (format nil \"~a ~a ~a ~a ~a\" \
                 (jget c \"range\" \"start\" \"line\") (jget c \"range\" \"start\" \"character\") \
                 (jget c \"range\" \"end\" \"line\") (jget c \"range\" \"end\" \"character\") \
                 (map 'list #'char-code (jget c \"text\"))))"
        )
    };
    let change = |splice: &str| change_of("(buffer-string)", splice);
    // An insert before the `a`: character 5, code unit 6.
    says(
        &lisp,
        &shared,
        &change(r#"(concatenate 'string (subseq old 0 5) "!" (subseq old 5))"#),
        "0 6 0 6 (33)",
    );
    // The accent changed and *only* the accent — `é` becoming `è`. One character
    // replaced by one character, at code unit 8. This used to need the range
    // snapping back off the middle of a codepoint, because `é` is the bytes
    // 195 169 and `è` is 195 168 and a byte-wise `mismatch` stopped between them.
    says(
        &lisp,
        &shared,
        &change(
            "(concatenate 'string (subseq old 0 7) (string (code-char 232)) (subseq old 8))",
        ),
        "0 8 0 9 (232)",
    );
    // The emoji deleted: one character and **two** code units — this is the
    // assertion the whole file exists for.
    says(
        &lisp,
        &shared,
        &change("(concatenate 'string (subseq old 0 2) (subseq old 3))"),
        "0 2 0 4 NIL",
    );
    // A change on the second line, so the line counting is pinned too.
    says(
        &lisp,
        &shared,
        &change(
            "(let ((n (1+ (position #\\Newline old)))) \
               (concatenate 'string (subseq old 0 n) \"q\" (subseq old n)))",
        ),
        "1 0 1 0 (113)",
    );
    // Nothing changed is not a change. `after-change-hook' fires when the
    // *revision* moves, which a buffer switch does without touching a character.
    says(
        &lisp,
        &shared,
        "(%lsp-content-change (buffer-string) (buffer-string))",
        "NIL",
    );

    // --- the shapes that break a narrowing ----------------------------------
    //
    // The four above are all edits in the middle of the first line, which is the
    // case every implementation gets right. These are the ends and the empty
    // runs. The buffer is twenty-four characters — eight, a newline, fourteen,
    // a newline — and every number below is counted off that, so it is pinned
    // here rather than left implied.
    says(&lisp, &shared, "(length (buffer-string))", "24");
    // At offset 0 there is no common prefix at all, and the common suffix is the
    // whole of the old document.
    says(
        &lisp,
        &shared,
        &change(r#"(concatenate 'string "x" old)"#),
        "0 0 0 0 (120)",
    );
    // At the very end, which is also OLD being a strict prefix of NEW — the case
    // where the answer is OLD's own length rather than an index into it, and the
    // one worth pinning because `string/=' and `mismatch' have to agree on it.
    // The document ends in a newline, so the position is the start of the line
    // after the last one.
    says(
        &lisp,
        &shared,
        &change(r#"(concatenate 'string old "x")"#),
        "2 0 2 0 (120)",
    );
    // ...and the same the other way round, NEW a strict prefix of OLD: the last
    // character is deleted, so the range covers it and the replacement is empty.
    says(
        &lisp,
        &shared,
        &change("(subseq old 0 (1- (length old)))"),
        "1 14 2 0 NIL",
    );
    // A character *replaced* rather than inserted, at offset 0. Insertions and
    // deletions leave everything after the edit aligned and are settled by the
    // forward check on the capped suffix; a replacement is not, and falls
    // through to the backward `mismatch'. This is that branch.
    says(
        &lisp,
        &shared,
        &change(r#"(concatenate 'string "X" (subseq old 1))"#),
        "0 0 0 1 (88)",
    );
    // A last line with no newline after it. The buffer always has one, so this
    // pair is written out: `bc' becomes `bXc' on line 1, and the line's start is
    // found by a scan that has no trailing newline to stop on.
    says(
        &lisp,
        &shared,
        &change_of(r#"(format nil "a~%bc")"#, r#"(format nil "a~%bXc")"#),
        "1 1 1 1 (88)",
    );
    // Multi-byte on *both* sides of the edit point: an emoji and an `é' before
    // it on the same line, an `é' after it. The column is three and not two
    // because the emoji is a surrogate pair — count characters instead of code
    // units here and the server edits a different place than the one meant.
    says(
        &lisp,
        &shared,
        &change_of(
            r#"(format nil "a~%~a~ax~a" (code-char 128512) (code-char 233) (code-char 233))"#,
            r#"(format nil "a~%~a~aXx~a" (code-char 128512) (code-char 233) (code-char 233))"#,
        ),
        "1 3 1 3 (88)",
    );
    // The same document, but the `x' is replaced rather than pushed along — so
    // the backward `mismatch' runs with multi-byte characters on both sides of
    // where it stops, and the end column counts the emoji twice as well.
    says(
        &lisp,
        &shared,
        &change_of(
            r#"(format nil "a~%~a~ax~a" (code-char 128512) (code-char 233) (code-char 233))"#,
            r#"(format nil "a~%~a~aQ~a" (code-char 128512) (code-char 233) (code-char 233))"#,
        ),
        "1 3 1 4 (81)",
    );

    // Every offset in a small multi-byte document, inserted, deleted and
    // replaced, in both directions, against the narrowing this file used to
    // have — which is transcribed below rather than described, because that is
    // the only form of it a test can compare against.
    //
    // The examples above are the shapes somebody thought of. This is the one
    // that matters: a `didChange' naming the wrong range does not fail here, it
    // fails as a wrong completion an hour later and nowhere near the edit, so
    // what has to be true is that the faster spelling answers the *same object*
    // as the slower one it replaced, at every offset and not at four of them.
    says(
        &lisp,
        &shared,
        r#"(labels ((refpos (text at)
                      (jobj "line" (count #\Newline text :end at)
                            "character"
                            (%lsp-utf16-length
                             text
                             :start (let ((nl (position #\Newline text :end at :from-end t)))
                                      (if nl (1+ nl) 0))
                             :end at)))
                    (refchange (old new)
                      (let ((head (mismatch old new)))
                        (when head
                          (let* ((lo (length old)) (ln (length new))
                                 (tail (min (- lo (or (mismatch old new :from-end t) 0))
                                            (- (min lo ln) head)))
                                 (old-end (- lo tail)))
                            (jobj "range" (jobj "start" (refpos old head)
                                                "end" (refpos old old-end))
                                  "text" (subseq new head (- ln (- lo old-end)))))))))
             (let ((doc (format nil "a~a~a~%bb~%c~a~%~%d~a"
                                (code-char 233) (code-char 128512)
                                (code-char 128512) (code-char 233))))
               (loop for k from 0 to (length doc)
                     always (let* ((n (length doc))
                                   (ins (concatenate 'string (subseq doc 0 k) "Q" (subseq doc k)))
                                   (del (if (< k n)
                                            (concatenate 'string (subseq doc 0 k) (subseq doc (1+ k)))
                                            doc))
                                   (rep (if (< k n)
                                            (concatenate 'string (subseq doc 0 k) "Q" (subseq doc (1+ k)))
                                            doc)))
                              (and (equal (refchange doc ins) (%lsp-content-change doc ins))
                                   (equal (refchange ins doc) (%lsp-content-change ins doc))
                                   (equal (refchange doc del) (%lsp-content-change doc del))
                                   (equal (refchange del doc) (%lsp-content-change del doc))
                                   (equal (refchange doc rep) (%lsp-content-change doc rep)))))))"#,
        "T",
    );

    // --- the negotiation ----------------------------------------------------
    says(
        &lisp,
        &shared,
        "(eq :incremental (%lsp-get (lsp-session-for-buffer) :sync))",
        "T",
    );

    // --- an outgoing position ------------------------------------------------
    //
    // Point on the `a` of `café`. The server is owed 6 and this editor's own
    // column is 5; before the conversion it was told 5 and looked at the `f`.
    lisp.eval("(goto-char 5)".into());
    lisp.eval("(lsp-goto-definition)".into());
    wait_for_log(&lisp, &log_inc, r#""position":{"line":0,"character":6}"#);

    // --- an incoming position ------------------------------------------------
    //
    // Measured against the shadow, because a workspace analyser publishes for
    // files that are not the buffer on screen and `line-string` cannot see them.
    lisp.eval(format!(
        r#"(%lsp-publish-diagnostics
             (jobj "uri" (lsp-path-uri {py_file:?})
                   "diagnostics" (jarr (jobj "range" (jobj "start" (jobj "line" 0 "character" 6))
                                             "severity" 2 "message" "on the a"))))"#
    ));
    wait(&shared, &lisp, "the summary", |m| m == "lsp: 0 errors, 1 warning");
    says(&lisp, &shared, "(second (first (lsp-diagnostics)))", "5");
    // A file this client never opened has no shadow to measure against, so the
    // server's own number stands — the old behaviour, and right on ASCII.
    lisp.eval(
        r#"(%lsp-publish-diagnostics
             (jobj "uri" "file:///tmp/zemacs_lsp_sync/never-opened.py"
                   "diagnostics" (jarr (jobj "range" (jobj "start" (jobj "line" 0 "character" 6))
                                             "severity" 2 "message" "elsewhere"))))"#
            .into(),
    );
    says(
        &lisp,
        &shared,
        r#"(second (first (lsp-diagnostics "/tmp/zemacs_lsp_sync/never-opened.py")))"#,
        "6",
    );

    // --- a real edit, and what goes down the pipe ----------------------------
    {
        let mut ed = shared.lock().unwrap();
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::MoveTo(5));
        ed.apply(EditorCommand::InsertText("!".into()));
        ed.apply(EditorCommand::SetMode(Mode::Normal));
    }
    after_change(&lisp);
    wait_for_log(
        &lisp,
        &log_inc,
        r#""contentChanges":[{"range":{"start":{"line":0,"character":6},"end":{"line":0,"character":6}},"text":"!"}]"#,
    );
    assert!(log(&log_inc).contains(r#""version":2"#), "versions must climb");
    // The whole point: the rest of the document did *not* go again. The sentinel
    // is on the second line, which no range above ever covered, so it can only
    // be in the log once — from the `didOpen`.
    assert_eq!(
        log(&log_inc).matches("ZSENTINELZ").count(),
        1,
        "the document was resent whole: {:?}",
        log(&log_inc)
    );

    // ...and a non-ASCII edit goes out as the character it is, not as the two
    // bytes that spell it encoded a second time — the shim's `dup_utf8` is the
    // only thing that ever encodes it. The range is at code unit 6 again because
    // the insert is at the same place.
    {
        let mut ed = shared.lock().unwrap();
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::MoveTo(5));
        ed.apply(EditorCommand::InsertText("é".into()));
        ed.apply(EditorCommand::SetMode(Mode::Normal));
    }
    after_change(&lisp);
    wait_for_log(
        &lisp,
        &log_inc,
        r#""contentChanges":[{"range":{"start":{"line":0,"character":6},"end":{"line":0,"character":6}},"text":"é"}]"#,
    );

    // --- a server that only does full sync still gets full sync --------------
    //
    // The expensive mistake in the other direction: a range sent to a server
    // that asked for whole documents is taken as the whole document, and the
    // damage surfaces much later.
    {
        let mut ed = shared.lock().unwrap();
        ed.load(C_SOURCE, Some(PathBuf::from(&c_file)), Some("c".into()));
    }
    after_change(&lisp);
    wait(&shared, &lisp, "the second handshake", |m| {
        m.starts_with("lsp: c-mode ready in /tmp/zemacs_lsp_sync/")
    });
    wait_for_log(&lisp, &log_full, "textDocument/didOpen");
    says(
        &lisp,
        &shared,
        "(eq :full (%lsp-get (lsp-session-for-buffer) :sync))",
        "T",
    );
    {
        let mut ed = shared.lock().unwrap();
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::MoveTo(0));
        ed.apply(EditorCommand::InsertText("// edited\n".into()));
        ed.apply(EditorCommand::SetMode(Mode::Normal));
    }
    after_change(&lisp);
    wait_for_log(&lisp, &log_full, r#""contentChanges":[{"text":"// edited"#);
    assert!(
        !log(&log_full).contains(r#""range""#),
        "a full-sync server was sent a range: {:?}",
        log(&log_full)
    );

    zemacs_rpc::stop_all();
    let _ = std::fs::remove_dir_all(DIR);
}
