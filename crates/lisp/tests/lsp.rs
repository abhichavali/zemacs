//! Headless proof of the LSP client, against a fake server rather than a real
//! one: `pylsp` and `clangd` are not build dependencies and this must pass on a
//! machine that has neither.
//!
//! The fake is eleven lines of `sh`. It answers `initialize`, publishes one
//! diagnostic, and then appends everything the client sends it to a file, which
//! is how the test can assert that a `didOpen` carrying the buffer's text
//! actually went out. That is enough to exercise every part of the client that
//! is not the server's own analysis: the handshake, the queue that holds
//! notifications until `initialized` has gone out, document synchronisation,
//! diagnostics arriving as a notification, and the jump a definition answer
//! turns into.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use zemacs_core::{EditorCommand, Shared};

const PATIENCE: Duration = Duration::from_secs(20);

const SOURCE: &str = "import os\n\n\ndef main():\n    return foo\n";
const FILE: &str = "/tmp/zemacs_lsp_test.py";
const LOG: &str = "/tmp/zemacs_lsp_test.log";

/// A language server that never analyses anything: it answers the handshake,
/// publishes one diagnostic, and records what it was sent.
///
/// `${#1}` is the byte length of the body, which is what Content-Length means —
/// everything here is ASCII, so counting characters happens to agree.
const FAKE_SERVER: &str = r#"#!/bin/sh
send() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
send '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"textDocumentSync":1}}}'
send '{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///tmp/zemacs_lsp_test.py","diagnostics":[{"range":{"start":{"line":4,"character":11},"end":{"line":4,"character":14}},"severity":1,"source":"pyflakes","message":"undefined name foo"}]}}'
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

fn wait(shared: &Shared, lisp: &zemacs_lisp::Lisp, what: &str, pred: impl Fn(&str) -> bool) {
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

fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"~a\" {form}))"));
    wait(shared, lisp, form, |m| m == want);
}

/// What the editor does when the document moves, spelled exactly as the main
/// loop spells it — by name, guarded with `fboundp`.
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

fn log_contains(needle: &str) -> bool {
    let mut text = String::new();
    std::fs::File::open(LOG)
        .and_then(|mut f| f.read_to_string(&mut text))
        .map(|_| text.contains(needle))
        .unwrap_or(false)
}

fn wait_for_log(lisp: &zemacs_lisp::Lisp, needle: &str) {
    let deadline = Instant::now() + PATIENCE;
    while !log_contains(needle) {
        pump(lisp);
        assert!(Instant::now() < deadline, "the server was never sent {needle:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_lsp_client_talks_to_a_server() {
    let server = std::env::temp_dir()
        .join(format!("zemacs_fake_lsp-{}.sh", std::process::id()));
    std::fs::write(&server, FAKE_SERVER).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let _ = std::fs::remove_file(LOG);
    std::fs::write(FILE, SOURCE).unwrap();

    let init = std::env::temp_dir()
        .join(format!("zemacs_test_lsp_init-{}.lisp", std::process::id()));
    std::fs::write(
        &init,
        format!(
            // `modes/modes.lisp` first, because it is where
            // `*after-change-functions*` and the `after-change-hook` the
            // application calls by name are declared — the client only
            // registers on them. The shipped config loads it above `lsp.lisp`
            // for the same reason, so this is what the editor actually does.
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             ;; Replaces the shipped pylsp entry — the point of the registry is\n\
             ;; that a server is a one-line change in Lisp and nothing else.\n\
             (lsp-register-server 'python-mode {:?} {LOG:?})\n\
             (message \"lsp test init loaded\")\n",
            runtime("modes/modes.lisp"),
            runtime("rpc.lisp"),
            runtime("lsp.lisp"),
            server.to_string_lossy(),
        ),
    )
    .unwrap();

    let (tx, rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    wait(&shared, &lisp, "the init file", |m| m == "lsp test init loaded");

    // --- URIs ---------------------------------------------------------------
    //
    // A path with a space in it is not exotic on macOS, and a server rejects an
    // unencoded one outright.
    says(&shared, &lisp, r#"(lsp-path-uri "/tmp/a b.py")"#, "file:///tmp/a%20b.py");
    says(&shared, &lisp, r#"(lsp-uri-path "file:///tmp/a%20b.py")"#, "/tmp/a b.py");
    // Round trip, which is the property that actually matters.
    says(&shared, &lisp, r#"(lsp-uri-path (lsp-path-uri "/tmp/x#y+z.c"))"#, "/tmp/x#y+z.c");
    // A location this editor cannot open answers NIL rather than a broken path.
    says(&shared, &lisp, r#"(lsp-uri-path "jar:file:///x!/y.class")"#, "NIL");

    // --- project root -------------------------------------------------------
    //
    // Where the server is started decides what project it thinks it is in.
    says(&shared, &lisp, r#"(lsp-project-root "/tmp/zemacs_lsp_test.py")"#, "/tmp/");

    // --- nothing happens in a buffer with no server -------------------------
    says(&shared, &lisp, "(lsp-session-for-buffer)", "NIL");
    lisp.eval("(lsp)".into());
    wait(&shared, &lisp, "the report", |m| m == "lsp: this buffer has no file");

    // --- open the file and let the hook do the rest -------------------------
    {
        let mut ed = shared.lock().unwrap();
        ed.load(SOURCE, Some(PathBuf::from(FILE)), Some("python".into()));
    }
    says(&shared, &lisp, "(major-mode)", "python-mode");

    // This is the whole trigger: the editor says the document moved, and the
    // client starts a server, opens the document and keeps it in step. No mode
    // hook, no explicit command.
    after_change(&lisp);
    wait(&shared, &lisp, "the handshake", |m| m.starts_with("lsp: python-mode ready in /tmp/"));
    says(&shared, &lisp, "(if (lsp-session-for-buffer) t nil)", "T");

    // The didOpen was *queued* behind the handshake — nothing may be sent to a
    // server before `initialized` — and carries the buffer's text.
    wait_for_log(&lisp, "textDocument/didOpen");
    wait_for_log(&lisp, "def main()");
    assert!(log_contains(r#""languageId":"python""#), "the languageId comes from the mode");
    assert!(log_contains(r#""version":1"#));

    // --- diagnostics --------------------------------------------------------
    //
    // They arrived as a notification during the handshake, so they are already
    // here: line 5 (LSP counts from 0 and this API from 1), an error, from
    // pyflakes.
    says(&shared, &lisp, "(length (lsp-diagnostics))", "1");
    says(&shared, &lisp, "(first (lsp-diagnostics))",
         "(5 11 1 undefined name foo pyflakes)");
    says(&shared, &lisp, r#"(lsp-severity-name (third (first (lsp-diagnostics))))"#, "error");
    // Visible today, before any overlay exists: a summary when they land, and a
    // command that reads out the one under the cursor.
    wait(&shared, &lisp, "the summary", |m| m == "lsp: 1 error, 0 warnings");
    lisp.eval("(goto-line 5)".into());
    lisp.eval("(lsp-diagnostics-at-point)".into());
    wait(&shared, &lisp, "the diagnostic at point", |m| {
        m == "error: undefined name foo [pyflakes]"
    });
    lisp.eval("(goto-line 1)".into());
    lisp.eval("(lsp-diagnostics-at-point)".into());
    wait(&shared, &lisp, "a clean line", |m| m == "no diagnostic on this line");

    // The renderer hung off the seam has already drawn for the diagnostic the
    // fake server published: one `gutter` overlay, on the line it named. Nothing
    // in this test installed it — `lsp.lisp` pushes it onto the seam itself,
    // which is the whole claim that the seam is usable.
    //
    // `gutter` and not `line-prefix`, and asserted untrimmed: a prefix moves the
    // marked line right by its own width, which put every diagnostic a column
    // out of alignment with the code around it.
    let one = "(overlays-in (point-min) (point-max))";
    let mark = format!("(overlay-get (first (first {one})) 'gutter)");
    says(&shared, &lisp, &format!("(length {one})"), "1");
    says(&shared, &lisp, &mark, "●");
    says(&shared, &lisp, &format!("(= (second (first {one})) (line-start 5))"), "T");

    // The seam the overlay renderer attaches to. Nothing in `lsp.lisp` has to
    // change for flymake-style gutter marks to appear — this is the contract.
    lisp.eval(
        r#"(push (lambda (path) (message (format nil "overlay hook ~a ~a"
                                                 path (length (lsp-diagnostics path)))))
                 *lsp-diagnostics-functions*)"#
            .into(),
    );
    lisp.eval(
        r#"(%lsp-publish-diagnostics
             (jobj "uri" "file:///tmp/zemacs_lsp_test.py"
                   "diagnostics" (jarr (jobj "range" (jobj "start" (jobj "line" 0 "character" 0))
                                             "severity" 2 "message" "unused import"))))"#
            .into(),
    );
    wait(&shared, &lisp, "the overlay seam", |m| {
        m == "overlay hook /tmp/zemacs_lsp_test.py 1"
    });

    // Repainted rather than appended: the warning *replaces* the error, so the
    // mark changes shape and moves to the new line instead of the two piling
    // up. A renderer that leaks an overlay per publish is a file wearing a
    // margin full of marks for problems that were fixed ten minutes ago.
    says(&shared, &lisp, &format!("(length {one})"), "1");
    says(&shared, &lisp, &mark, "▲");
    says(&shared, &lisp, &format!("(= (second (first {one})) (line-start 1))"), "T");

    // --- a change is synchronised -------------------------------------------
    //
    // Full text, not a delta: see the ponytail note at the top of `lsp.lisp`.
    // The version has to climb, or a server ignores the update.
    {
        let mut ed = shared.lock().unwrap();
        ed.apply(EditorCommand::SetMode(zemacs_core::Mode::Insert));
        ed.apply(EditorCommand::MoveTo(0));
        ed.apply(EditorCommand::InsertText("# edited\n".into()));
        ed.apply(EditorCommand::SetMode(zemacs_core::Mode::Normal));
    }
    after_change(&lisp);
    wait_for_log(&lisp, "textDocument/didChange");
    wait_for_log(&lisp, "# edited");
    assert!(log_contains(r#""version":2"#), "versions must climb");

    // --- a definition answer becomes a jump ---------------------------------
    //
    // `find-file-at` is the only command that opens a file *and* lands on a
    // line; a `goto-char` after `find-file` would move the cursor in the buffer
    // being left. LSP counts lines from 0 and the hit is 1-based.
    lisp.eval(
        r#"(%lsp-goto-location
             (jobj "uri" "file:///tmp/zemacs_target.py"
                   "range" (jobj "start" (jobj "line" 9 "character" 2))))"#
            .into(),
    );
    let mut seen = Vec::new();
    let deadline = Instant::now() + PATIENCE;
    while !seen.contains(&EditorCommand::OpenAt("/tmp/zemacs_target.py:10:".into())) {
        pump(&lisp);
        assert!(Instant::now() < deadline, "no jump; got {seen:#?}");
        if let Ok(c) = rx.recv_timeout(Duration::from_millis(50)) {
            seen.push(c);
        }
    }
    // A LocationLink rather than a Location, which is what a modern server
    // answers when `linkSupport` is claimed — and this client claims it.
    lisp.eval(
        r#"(%lsp-goto-location
             (jobj "targetUri" "file:///tmp/zemacs_target.py"
                   "targetSelectionRange" (jobj "start" (jobj "line" 0 "character" 0))))"#
            .into(),
    );
    let deadline = Instant::now() + PATIENCE;
    while !seen.contains(&EditorCommand::OpenAt("/tmp/zemacs_target.py:1:".into())) {
        pump(&lisp);
        assert!(Instant::now() < deadline, "no jump for a LocationLink; got {seen:#?}");
        if let Ok(c) = rx.recv_timeout(Duration::from_millis(50)) {
            seen.push(c);
        }
    }

    // --- the status listing --------------------------------------------------
    //
    // Asserted on the block of text rather than through the buffer it goes in:
    // the buffer is four primitives that cannot be individually wrong, and
    // every bug this command can have is in the lines.
    //
    // `program` is the line it exists for. A language server is only ever
    // suspected once you cannot see *which binary* it is — a global `pylsp`
    // against a project `.venv` reports every import as missing, and the two
    // halves of that are this line and the next one.
    says(
        &shared,
        &lisp,
        &format!(
            "(let ((block (format nil \"~{{~a~%}}~}}\"
                                 (%lsp-status-lines (lsp-session-for-buffer)))))
               (if (and (search {:?} block)
                        (search \"none in this project\" block))
                   \"named\" \"vague\"))",
            server.to_string_lossy()
        ),
        "named",
    );

    // --- shutting down ------------------------------------------------------
    //
    // didClose, shutdown and exit all reach the server, because `rpc-stop`
    // writes what is queued before it closes the pipe.
    lisp.eval("(lsp-stop)".into());
    wait(&shared, &lisp, "the stop", |m| m.starts_with("lsp: stopped python-mode"));
    wait_for_log(&lisp, "textDocument/didClose");
    assert!(log_contains(r#""method":"shutdown""#), "the polite sequence goes out");
    assert!(log_contains(r#""method":"exit""#));
    says(&shared, &lisp, "(lsp-session-for-buffer)", "NIL");
    says(&shared, &lisp, "(hash-table-count *lsp-sessions*)", "0");

    // --- and it stays stopped -----------------------------------------------
    //
    // `lsp-ensure` is on `after-change-hook` and starts a server for any buffer
    // that has none, so a stop used to last exactly one keystroke: you stopped
    // it, typed a character, and watched it come back. `*lsp-stopped*` is the
    // memory of having meant it.
    after_change(&lisp);
    says(&shared, &lisp, "(lsp-session-for-buffer)", "NIL");

    // **But only a stop you asked for.** A server that *died* is not one you
    // stopped, and the repair on the next keystroke is the behaviour rather
    // than a bug — so `%lsp-forget` on its own must leave the door open.
    // `remhash` here is what a crash amounts to: the session is gone and
    // nothing was recorded about why.
    lisp.eval("(clrhash *lsp-stopped*)".into());
    after_change(&lisp);
    wait(&shared, &lisp, "the repair", |m| m.starts_with("lsp: python-mode"));
    says(&shared, &lisp, "(if (lsp-session-for-buffer) \"repaired\" \"dead\")", "repaired");
    lisp.eval("(lsp-stop)".into());
    wait(&shared, &lisp, "the second stop", |m| m.starts_with("lsp: stopped"));

    // --- the project's own interpreter ---------------------------------------
    //
    // A server off `$PATH` is the *editor's* server, and for Python that is
    // nearly always the wrong one: a globally-installed `pylsp` resolves imports
    // against the interpreter it was installed under, so every dependency in a
    // `uv` project reads as missing.
    let venv = std::path::Path::new(FILE).parent().unwrap().join("venvproj");
    let _ = std::fs::remove_dir_all(&venv);
    std::fs::create_dir_all(venv.join(".venv/bin")).unwrap();
    std::fs::write(venv.join(".venv/bin/python"), "#!/bin/sh\n").unwrap();
    let root = format!("{}/", venv.display());

    // Found by looking in the *project*, not at `$VIRTUAL_ENV`: the editor was
    // started from a login shell, and that variable names whichever project you
    // last activated — the wrong answer in every window but one.
    says(&shared, &lisp, &format!("(if (%lsp-venv {root:?}) \"found\" \"none\")"), "found");
    // A project with no virtualenv answers NIL, which is what leaves a
    // system-Python checkout behaving exactly as it did.
    says(&shared, &lisp, "(%lsp-venv \"/tmp/\")", "NIL");

    // With no server inside the venv the bare name is kept, and the settings
    // below are what point the global one at the right interpreter.
    says(&shared, &lisp, &format!("(%lsp-program-for \"pylsp\" {root:?})"), "pylsp");
    // ...and with one, it wins: a server inside the venv is already looking at
    // the right site-packages and needs no configuration at either end.
    std::fs::write(venv.join(".venv/bin/pylsp"), "#!/bin/sh\n").unwrap();
    says(
        &shared,
        &lisp,
        &format!("(%lsp-program-for \"pylsp\" {root:?})"),
        &format!("{}/.venv/bin/pylsp", venv.display()),
    );

    // The settings a global server is handed. `jedi.environment` is the field
    // that joins a `pylsp` on `$PATH` to a `.venv` in the project, and is what
    // makes `g d` on an import land in the project's own copy of it.
    says(
        &shared,
        &lisp,
        &format!(
            "(jget (funcall (gethash \"python-mode\" *lsp-settings*) {root:?})
                   \"pylsp\" \"plugins\" \"jedi\" \"environment\")"
        ),
        &format!("{}/.venv/", venv.display()),
    );
    // No virtualenv, no settings — rather than a settings object naming one that
    // is not there.
    says(
        &shared,
        &lisp,
        "(if (funcall (gethash \"python-mode\" *lsp-settings*) \"/tmp/\") \"some\" \"none\")",
        "none",
    );
    let _ = std::fs::remove_dir_all(&venv);

    // A mode with no server registered says so rather than doing nothing.
    lisp.eval(r#"(remhash "python-mode" *lsp-servers*)"#.into());
    lisp.eval("(lsp)".into());
    wait(&shared, &lisp, "the report", |m| m == "lsp: no server registered for python-mode");

    // Last, because it leaves the editor in `*lsp*` rather than in the Python
    // file — a listing is a buffer you switch to, the same as `xref-show`'s and
    // `lsp-list-diagnostics`'. The empty case is the one path with no session to
    // read anything out of, and it used to be the whole command.
    lisp.eval("(lsp-status)".into());
    says(&shared, &lisp, "(buffer-name)", "*lsp*");

    zemacs_rpc::stop_all();
    let _ = std::fs::remove_file(FILE);
}
