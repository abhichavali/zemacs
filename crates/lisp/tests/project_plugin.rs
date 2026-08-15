//! `runtime/plugins/project.lisp` — wave 2 of the Common Lisp migration.
//!
//! Where a project starts and what "build it" means there used to be
//! `Marker::commands` and `Project::build_marker` in `crates/project`, asserted
//! by three tests in `crates/project/tests/project.rs`. Those functions are
//! gone; these are those assertions, made of the Lisp that replaced them,
//! against a real image booting the shipped runtime in its shipped order.
//!
//! The trees are built here rather than pointed at this repository, so what is
//! asserted is the *rule* — deepest repository wins, a build file only counts
//! when no repository is above it, the command comes from the root — instead of
//! whatever zemacs happens to be built with today.
//!
//! Two halves of the plugin are deliberately not asserted, for the reason
//! `project_make.rs` gives about its own: `project-compile` ends in a
//! `(terminal "output:…")`, which needs an application loop and a PTY, and
//! `project-dired` ends in a `(dired …)`, which needs the same. What can be
//! wrong *quietly* is everything before those calls, and that is all of this.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::time::{Duration, Instant};

use zemacs_core::{Editor, Shared};

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

/// Evaluate FORM and wait for its printed value to appear in the message log.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"~a\" {form}))"));
    wait(shared, &format!("{form} to answer {want:?}"), |ed| {
        ed.messages.iter().any(|m| m == want).then_some(())
    });
}

fn runtime(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .expect("the runtime directory ships with the source")
}

/// A directory under the system temp directory, emptied first so a re-run does
/// not inherit last run's markers.
fn tree(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("zemacs_test_project_plugin-{}", std::process::id())).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The Lisp spelling of a directory: absolute, with the trailing separator that
/// tells `merge-pathnames` the last component is a directory and not a file.
fn as_dir(p: &std::path::Path) -> String {
    format!("{}/", p.to_str().unwrap())
}

#[test]
fn the_climb_and_the_build_table_answer_what_crates_project_used_to() {
    // (a) a repository whose root also carries the build file
    let repo = tree("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join("Cargo.toml"), "[package]\n").unwrap();
    let deep = repo.join("crates").join("app").join("src");
    std::fs::create_dir_all(&deep).unwrap();
    // A workspace member with a manifest of its own, which must *not* become the
    // root: the interesting operations are repository-shaped.
    std::fs::write(repo.join("crates/app/Cargo.toml"), "[package]\n").unwrap();

    // (b) a repository with nothing that says how to build it
    let bare = tree("bare");
    std::fs::create_dir_all(bare.join(".git")).unwrap();

    // (c) a Makefile and no repository at all — a build file is a root only here
    let makeonly = tree("makeonly");
    std::fs::write(makeonly.join("Makefile"), "all:\n\techo hi\n").unwrap();

    // (d) a hand-dropped `.project` inside a repository, which outranks it
    let explicit = repo.join("crates").join("app");
    std::fs::write(explicit.join(".project"), "").unwrap();

    let init = std::env::temp_dir()
        .join(format!("zemacs_test_project_plugin_init-{}.lisp", std::process::id()));
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (message \"project plugin test init loaded\")\n",
            runtime("modes/modes.lisp"),
            runtime("init.lisp"),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init.clone());
    wait_message(&shared, "the init file", |m| {
        m == "project plugin test init loaded"
    });

    // The plugin is in `*runtime-modules*` and `load-runtime-modules` reached
    // it. Asserted first because every failure below would otherwise read as a
    // wrong answer rather than as a file that never loaded.
    says(&shared, &lisp, "(if (fboundp 'project-compile) \"yes\" \"no\")", "yes");

    // --- the climb -----------------------------------------------------------

    // From four directories deep, the answer is the repository. This is the
    // precedence rule in one assertion: a `Cargo.toml` in `crates/app` is passed
    // over because a `.git` lies above it.
    //
    // ...except that `crates/app` now has a `.project` in it, which is the one
    // thing that overrules a repository — so the deep file resolves *there*, and
    // the assertion below from the repository root proves the `.git` still wins
    // where nothing has been dropped.
    says(
        &shared,
        &lisp,
        &format!("(second (%project-at {:?}))", as_dir(&deep)),
        "project",
    );
    says(
        &shared,
        &lisp,
        &format!("(namestring (first (%project-at {:?})))", as_dir(&deep)),
        &as_dir(&explicit),
    );
    says(
        &shared,
        &lisp,
        &format!("(namestring (first (%project-at {:?})))", as_dir(&repo)),
        &as_dir(&repo),
    );
    says(
        &shared,
        &lisp,
        &format!("(second (%project-at {:?}))", as_dir(&repo)),
        "git",
    );
    // A build file *is* the root when there is no repository anywhere above it.
    says(
        &shared,
        &lisp,
        &format!("(second (%project-at {:?}))", as_dir(&makeonly)),
        "make",
    );
    // ...and nothing at all above `/` is NIL rather than an error, which is what
    // lets every verb say "not in a project" instead of signalling.
    says(&shared, &lisp, "(%project-at \"/\")", "NIL");

    // --- the build table -----------------------------------------------------

    // `the_build_command_comes_from_the_root_not_from_the_marker`: the marker
    // that made this a root is `.git`, and `cargo build` is still the answer.
    says(
        &shared,
        &lisp,
        &format!("(%project-command {:?} :build)", as_dir(&repo)),
        "cargo build",
    );
    says(
        &shared,
        &lisp,
        &format!("(%project-command {:?} :test)", as_dir(&repo)),
        "cargo test",
    );
    // `a_project_with_nothing_to_build_says_so_instead_of_guessing`.
    says(
        &shared,
        &lisp,
        &format!("(%project-command {:?} :build)", as_dir(&bare)),
        "NIL",
    );
    says(
        &shared,
        &lisp,
        &format!("(%project-command {:?} :test)", as_dir(&bare)),
        "NIL",
    );
    // `a_makefile_project_builds_with_bare_make`: no target for the build, and
    // `test` for the test.
    says(
        &shared,
        &lisp,
        &format!("(%project-command {:?} :build)", as_dir(&makeonly)),
        "make",
    );
    says(
        &shared,
        &lisp,
        &format!("(%project-command {:?} :test)", as_dir(&makeonly)),
        "make test",
    );

    // --- the table itself ----------------------------------------------------
    //
    // `every_builder_row_is_complete_and_no_root_marker_pretends_to_build`,
    // which was `every_build_marker_names_a_program_and_no_vcs_marker_does` in
    // `crates/project`. A table is worth pinning down whole: a row missing its
    // test command is a `SPC p t` that reports the wrong thing for one kind of
    // project and nothing anywhere says which.
    says(
        &shared,
        &lisp,
        "(if (every (lambda (row) (and (= 4 (length row))\
                                      (every (lambda (f) (plusp (length f))) row)))\
                    *project-builders*)\
             \"complete\" \"ragged\")",
        "complete",
    );
    // ...and a root marker is two fields, so nothing can read a build command
    // off `.git` by accident.
    says(
        &shared,
        &lisp,
        "(if (every (lambda (row) (= 2 (length row))) *project-roots*) \"yes\" \"no\")",
        "yes",
    );
    // The three spellings the Rust test named, because a mistyped `npm run
    // build` is a puzzled user rather than a failure anywhere else.
    says(&shared, &lisp, "(fourth (assoc \"Cargo.toml\" *project-builders* :test #'string=))", "cargo test");
    says(&shared, &lisp, "(third (assoc \"package.json\" *project-builders* :test #'string=))", "npm run build");
    says(&shared, &lisp, "(third (assoc \"Makefile\" *project-builders* :test #'string=))", "make");

    // The reason any of this is in Lisp: the table is data, so a kind of project
    // the editor has never heard of is one PUSH in a config and no recompile.
    lisp.eval(
        "(push '(\"build.zig\" \"zig\" \"zig build\" \"zig build test\") *project-builders*)"
            .to_string(),
    );
    let zig = tree("zig");
    std::fs::write(zig.join("build.zig"), "").unwrap();
    says(
        &shared,
        &lisp,
        &format!("(second (%project-at {:?}))", as_dir(&zig)),
        "zig",
    );
    says(
        &shared,
        &lisp,
        &format!("(%project-command {:?} :test)", as_dir(&zig)),
        "zig build test",
    );

    // --- the commands themselves ---------------------------------------------

    // `project-root` end to end. The scratch buffer has no file, so
    // `%project-start` falls back to `*default-pathname-defaults*` — which is
    // also how the verb behaves in a buffer with nothing behind it.
    lisp.eval(format!(
        "(let ((*default-pathname-defaults* (pathname {:?}))) (project-root))",
        as_dir(&makeonly)
    ));
    wait_message(&shared, "project-root to name the root and its marker", |m| {
        m == format!("{} (make)", as_dir(&makeonly))
    });

    // ...and outside a project it says so rather than reporting `/`.
    lisp.eval("(let ((*default-pathname-defaults* #p\"/\")) (project-root))".to_string());
    wait_message(&shared, "project-root outside a project", |m| {
        m.starts_with("not in a project")
    });

    // A project with a root but no build file reports which kind of project it
    // could not build, which is the whole difference between this message and
    // "not in a project".
    lisp.eval(format!(
        "(let ((*default-pathname-defaults* (pathname {:?}))) (project-compile))",
        as_dir(&bare)
    ));
    wait_message(&shared, "project-compile with nothing to build", |m| {
        m == "no compile command for a git project"
    });

    let _ = std::fs::remove_dir_all(std::env::temp_dir()
        .join(format!("zemacs_test_project_plugin-{}", std::process::id())));
}
