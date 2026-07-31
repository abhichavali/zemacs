//! The same client, against *real* language servers.
//!
//! `tests/lsp.rs` proves the client against a fake, which is what keeps the
//! suite honest on a machine with no toolchain. This one proves it against
//! `clangd` and `pylsp`, which is the only thing that can catch the mistakes a
//! fake cannot: a capability we claim and mishandle, a URI a real server
//! rejects, a `didOpen` whose text a real parser disagrees with.
//!
//! **Each half skips when its server is not installed.** A language server is
//! not a build dependency and never will be, so an absent one is a `return` and
//! not a failure. Both halves live in one `#[test]` because `cl_boot`
//! initialises a process-wide image and two servers are two sessions, not two
//! processes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use zemacs_core::{EditorCommand, Shared};

/// Generous: a real server parses, and the first `initialize` on a cold cache is
/// the slowest thing in this file by an order of magnitude.
const PATIENCE: Duration = Duration::from_secs(60);

const DIR: &str = "/tmp/zemacs_lsp_live";
const C_SOURCE: &str = "static int helper(void) { return 1; }\n\nint main(void) { return helper(); }\n";
const PY_SOURCE: &str = "def helper():\n    return 1\n\n\ndef main():\n    return helper() + undefined_thing\n";

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
        std::thread::sleep(Duration::from_millis(10));
    }
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

fn have(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// Wait for a jump to `suffix`.
///
/// Matched by suffix, not by equality: a server answers with the file's *real*
/// path, and on macOS `/tmp` is a symlink to `/private/tmp`. Nothing here can or
/// should insist a server echo the spelling it was handed.
fn wait_for_jump(
    shared: &Shared,
    lisp: &zemacs_lisp::Lisp,
    rx: &crossbeam_channel::Receiver<EditorCommand>,
    seen: &mut Vec<EditorCommand>,
    suffix: &str,
) {
    let hit = |c: &EditorCommand| matches!(c, EditorCommand::OpenAt(p) if p.ends_with(suffix));
    let deadline = Instant::now() + PATIENCE;
    while !seen.iter().any(hit) {
        pump(lisp);
        assert!(
            Instant::now() < deadline,
            "no jump to {suffix}; got {seen:#?} and {:#?}",
            shared.lock().unwrap().messages
        );
        if let Ok(c) = rx.recv_timeout(Duration::from_millis(50)) {
            seen.push(c);
        }
    }
}

#[test]
fn real_servers_answer() {
    let clangd = have("clangd");
    let pylsp = have("pylsp");
    if !clangd && !pylsp {
        eprintln!("skipping: neither clangd nor pylsp is installed");
        return;
    }

    std::fs::create_dir_all(DIR).unwrap();
    let c_file = format!("{DIR}/main.c");
    let py_file = format!("{DIR}/main.py");
    std::fs::write(&c_file, C_SOURCE).unwrap();
    std::fs::write(&py_file, PY_SOURCE).unwrap();
    // Root markers, so `lsp-project-root` stops here rather than at /tmp — and
    // so clangd has a compilation database instead of guessing.
    std::fs::write(format!("{DIR}/pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
    std::fs::write(
        format!("{DIR}/compile_commands.json"),
        format!(r#"[{{"directory":"{DIR}","file":"{c_file}","command":"clang -c main.c"}}]"#),
    )
    .unwrap();

    let init = std::env::temp_dir().join("zemacs_test_lsp_live_init.lisp");
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             ;; No --background-index: this is one file, and indexing a whole\n\
             ;; tree is the slowest thing clangd can be asked to do.\n\
             (lsp-register-server 'c-mode \"clangd\")\n\
             (message \"live lsp init loaded\")\n",
            runtime("rpc.lisp"),
            runtime("lsp.lisp"),
        ),
    )
    .unwrap();

    let (tx, rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    wait(&shared, &lisp, "the init file", |m| m == "live lsp init loaded");
    let mut seen: Vec<EditorCommand> = Vec::new();

    // --- clangd --------------------------------------------------------------
    if clangd {
        {
            let mut ed = shared.lock().unwrap();
            ed.load(C_SOURCE, Some(PathBuf::from(&c_file)), Some("c".into()));
        }
        // The whole trigger: the editor says the document moved.
        after_change(&lisp);
        wait(&shared, &lisp, "clangd's handshake", |m| {
            m.starts_with("lsp: c-mode ready in /tmp/zemacs_lsp_live/")
        });
        // A real server also *publishes* — an empty list for a file with nothing
        // wrong with it, which is how it says "I looked".
        wait(&shared, &lisp, "clangd's diagnostics", |m| m == "lsp: no diagnostics");

        // Line 3, on the call to `helper`; its definition is on line 1.
        lisp.eval(r#"(goto-char (search-forward "helper()" 0))"#.into());
        lisp.eval("(lsp-goto-definition)".into());
        wait_for_jump(&shared, &lisp, &rx, &mut seen, "/zemacs_lsp_live/main.c:1:");

        lisp.eval("(lsp-stop)".into());
        wait(&shared, &lisp, "clangd shutting down", |m| {
            m.starts_with("lsp: stopped c-mode")
        });
    }

    // --- pylsp ---------------------------------------------------------------
    //
    // Registered here rather than in the init file, so the C half above runs on
    // a machine with clangd and no pylsp.
    if pylsp {
        lisp.eval(r#"(lsp-register-server 'python-mode "pylsp")"#.into());
        {
            let mut ed = shared.lock().unwrap();
            ed.load(PY_SOURCE, Some(PathBuf::from(&py_file)), Some("python".into()));
        }
        after_change(&lisp);
        wait(&shared, &lisp, "pylsp's handshake", |m| {
            m.starts_with("lsp: python-mode ready in /tmp/zemacs_lsp_live/")
        });
        // pyflakes has something to say about this file, which is the half of
        // diagnostics a fake server cannot prove: a real analysis, at a real
        // position, in the buffer that is on screen.
        wait(&shared, &lisp, "pylsp's diagnostics", |m| m.starts_with("lsp: 1 error"));
        lisp.eval("(goto-line 6)".into());
        lisp.eval("(lsp-diagnostics-at-point)".into());
        wait(&shared, &lisp, "the diagnostic at point", |m| {
            m.contains("undefined_thing")
        });

        lisp.eval(r#"(goto-char (search-forward "helper() +" 0))"#.into());
        lisp.eval("(lsp-goto-definition)".into());
        wait_for_jump(&shared, &lisp, &rx, &mut seen, "/zemacs_lsp_live/main.py:1:");

        lisp.eval("(lsp-stop)".into());
        wait(&shared, &lisp, "pylsp shutting down", |m| {
            m.starts_with("lsp: stopped python-mode")
        });
    }

    zemacs_rpc::stop_all();
    let _ = std::fs::remove_dir_all(DIR);
}
