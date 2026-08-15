//! `runtime/plugins/project.lisp` — wave 3, the four pickers.
//!
//! `project-find-file`, `project-find-dir`, `project-switch` and
//! `project-open` were `Project::try_run` arms in `crates/app/src/project.rs`
//! until a prompt could be seeded from Lisp. They are `defun`s now, and what
//! this pins is the seam they cross, in both directions:
//!
//! * a picker over a list the *app* owns leaves the image as one
//!   `EditorCommand::PromptSource` naming the list, with the root Lisp climbed
//!   to — so the tens of thousands of paths behind it never touch the shim;
//! * a picker over a list small enough to hold — the projects visited before —
//!   is an ordinary `completing-read` over a reader, which is what lets
//!   `project-switch` notice the list is empty;
//! * `project-open` seeds the *editor's own* file picker with a label and an
//!   expanded home, which is two commands core applies on the spot.
//!
//! The other half of the seam — the app pouring a real directory listing into
//! the prompt — is `fill_prompt`'s own test in `crates/app/src/project.rs`.
//! There is no application loop here to drain the channel, which is exactly why
//! reading the channel is how this file asserts what left.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, PromptKind, Shared};

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

fn runtime(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .expect("the runtime directory ships with the source")
}

/// Drain what the image has emitted so far and answer the first `PromptSource`,
/// waiting for one to arrive. Everything else is let through — a picker emits a
/// `ReadFromMinibuffer` first, and that one is core's.
fn wait_source(rx: &crossbeam_channel::Receiver<EditorCommand>) -> String {
    let deadline = Instant::now() + PATIENCE;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(EditorCommand::PromptSource(spec)) => return spec,
            Ok(other) => seen.push(format!("{other:?}")),
            Err(_) => {}
        }
    }
    panic!("no PromptSource arrived; saw {seen:#?}");
}

#[test]
fn the_pickers_name_their_list_instead_of_carrying_it() {
    // A repository with a file in it, deep enough that the climb has work to do.
    let repo = std::env::temp_dir().join(format!("zemacs_test_project_pick-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    let deep = repo.join("crates").join("app").join("src");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let file = deep.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let init = std::env::temp_dir().join(format!(
        "zemacs_test_project_pick_init-{}.lisp",
        std::process::id()
    ));
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (message \"project pick test init loaded\")\n",
            runtime("modes/modes.lisp"),
            runtime("init.lisp"),
        ),
    )
    .unwrap();

    let (tx, rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init.clone());
    wait_message(&shared, "the init file", |m| {
        m == "project pick test init loaded"
    });

    // The climb starts at the live buffer's file, so put one there.
    shared.lock().unwrap().buffer.path = Some(file.clone());

    // --- a picker over a list the app owns -----------------------------------

    lisp.eval("(project-find-file)".into());
    // The prompt is Lisp's own — a `completing-read` — which is what makes the
    // callback, and therefore what accepting one *does*, a thing you can change
    // without touching Rust.
    let label = wait(&shared, "the file picker", |ed| {
        ed.prompt
            .as_ref()
            .filter(|p| matches!(p.kind, PromptKind::Lisp { completing: true, .. }))
            .map(|p| p.label.clone())
    });
    // Labelled with the project's own directory name, not the whole path.
    let name = repo.file_name().unwrap().to_str().unwrap();
    assert_eq!(label, format!("{name}: "));

    // ...and the list left as a *name*. This is the assertion the whole
    // migration turns on: had the candidates gone through the image, they would
    // be `PromptItems` here, and a repository with fifty thousand files would
    // pay for that on every `SPC p f`.
    let spec = wait_source(&rx);
    let (source, root) = spec.split_once(' ').expect("SOURCE ARGUMENT");
    assert_eq!(source, "project-files");
    assert_eq!(
        std::path::Path::new(root).canonicalize().unwrap(),
        repo.canonicalize().unwrap(),
        "the root Lisp climbed to is the one the app should walk"
    );

    // The directory picker is the same shape over the other list.
    shared.lock().unwrap().prompt = None;
    lisp.eval("(project-find-dir)".into());
    let spec = wait_source(&rx);
    assert!(spec.starts_with("project-dirs "), "{spec}");

    // --- a picker over a list small enough to hold ---------------------------
    //
    // `project-recent` is a reader rather than a source, so the answer is in the
    // image and `project-switch` can branch on it. A list, whatever this machine
    // happens to have visited.
    lisp.eval(
        "(message (format nil \"recent is a ~a\" (if (listp (project-recent)) \"list\" \"lie\")))"
            .into(),
    );
    wait_message(&shared, "project-recent", |m| m == "recent is a list");

    // --- seeding the editor's own picker -------------------------------------
    //
    // `project-open` is the case that needed `prompt-text`: the app completes a
    // `File` prompt with *absolute* paths, so a literal `~/` would match none of
    // them and the picker would open on an empty list looking broken.
    shared.lock().unwrap().prompt = None;
    lisp.eval("(project-open)".into());
    let (kind, label, text) = wait(&shared, "the directory picker", |ed| {
        ed.prompt
            .as_ref()
            .filter(|p| !p.label.is_empty())
            .map(|p| (p.kind, p.label.clone(), p.text.clone()))
    });
    assert_eq!(kind, PromptKind::File);
    assert_eq!(label, "Open directory: ");
    assert!(text.starts_with('/'), "not absolute: {text}");
    // Trailing separator, or the listing is of the *parent* and the first
    // keystroke re-reads a different directory.
    assert!(text.ends_with('/'), "not a directory: {text}");

    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_file(&init);
}
