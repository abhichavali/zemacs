//! `%project-at` on a file on another machine — the climb `project-find-file`,
//! `project-compile` and `project-shell` all start from when the buffer is
//! `/ssh:host:/…`.
//!
//! No server: `$ZEMACS_SSH` points at a stand-in that ignores the host and runs
//! the script it was handed with `sh -c`, which is what the far login shell
//! does. So the image, the primitive, tramp's quoting and the script are all
//! real, and the "remote" tree is a temporary directory here.
//!
//! Its own file because the variable is process-wide and `cl_boot` is too —
//! one image, one `#[test]`, as `project_plugin.rs` explains.

use std::time::{Duration, Instant};

use zemacs_core::{Editor, Shared};

fn wait(shared: &Shared, what: &str, f: impl Fn(&Editor) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !f(&shared.lock().unwrap()) {
        if Instant::now() >= deadline {
            let ed = shared.lock().unwrap();
            let last: Vec<_> = ed.messages.iter().rev().take(10).collect();
            panic!("timed out waiting for {what}\nmessages: {last:#?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// FORM's value, printed behind TAG: the tag is what stops an earlier message
/// with the same text from answering for this one.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, tag: &str, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"{tag} ~a\" {form}))"));
    let want = format!("{tag} {want}");
    wait(shared, &want, |ed| ed.messages.iter().any(|m| *m == want));
}

fn runtime(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .unwrap()
}

#[test]
#[cfg(unix)]
fn a_remote_file_finds_its_project_by_the_same_tables() {
    use std::os::unix::fs::PermissionsExt;
    let base = std::env::temp_dir()
        .join(format!("zemacs_test_project_remote-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let repo = base.join("repo it's");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("crates/x/src")).unwrap();
    std::fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::write(repo.join("crates/x/Cargo.toml"), "[package]\n").unwrap();
    std::fs::write(repo.join("crates/x/src/a.rs"), "").unwrap();
    let zig = base.join("zig");
    std::fs::create_dir_all(&zig).unwrap();
    std::fs::write(zig.join("build.zig"), "").unwrap();

    let fake = base.join("ssh");
    std::fs::write(&fake, "#!/bin/sh\nfor a; do last=$a; done\nexec /bin/sh -c \"$last\"\n")
        .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("ZEMACS_SSH", &fake);

    let init = base.join("init.lisp");
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (message \"remote project test init loaded\")\n",
            runtime("modes/modes.lisp"),
            runtime("init.lisp"),
        ),
    )
    .unwrap();
    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    wait(&shared, "the init file", |ed| {
        ed.messages.iter().any(|m| m == "remote project test init loaded")
    });

    let there = |p: &std::path::Path| format!("/ssh:me@box:{}", p.display());
    let file = there(&repo.join("crates/x/src/a.rs"));
    let root = format!("{}/", there(&repo));

    // The workspace member's manifest loses to the repository above it — the
    // local rule — and the root is a tramp name, as a string, ending in `/`.
    says(&shared, &lisp, "r1", &format!("(first (%project-at {file:?}))"), &root);
    says(&shared, &lisp, "r2", &format!("(second (%project-at {file:?}))"), "git");
    // What `SPC p c` would run there, read from the root on the host.
    says(&shared, &lisp, "r3", &format!("(%project-command {root:?} :build)"), "cargo build");
    says(&shared, &lisp, "r4", &format!("(%project-name {root:?})"), "repo it's");

    // The tables are the policy, over ssh as well: a row pushed from an init
    // makes a `build.zig` a root on the host exactly as it would here.
    lisp.eval("(push '(\"build.zig\" \"zig\" \"zig build\" \"zig build test\") *project-builders*)".into());
    let zfile = there(&zig.join("main.zig"));
    says(&shared, &lisp, "r5", &format!("(second (%project-at {zfile:?}))"), "zig");

    // A host that cannot answer is not "not in a project": the verbs say what
    // the host said instead.
    says(
        &shared,
        &lisp,
        "r7",
        "(progn (%project-at \"/ssh:me@box:/nonexistent-zemacs/a.rs\") \
                (if (search \"probing\" (%project-none)) \"said\" (%project-none)))",
        "said",
    );

    // A local name never reaches the primitive at all.
    says(&shared, &lisp, "r6", "(%remote-probe \"/tmp\" \".git\")", "NIL");

    std::env::remove_var("ZEMACS_SSH");
    let _ = std::fs::remove_dir_all(&base);
}
