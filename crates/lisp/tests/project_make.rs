//! `project-make`'s Makefile scanner, in `runtime/library.lisp`.
//!
//! The picker and the terminal half cannot be asserted from here — one waits on
//! a person and the other on a shell — but the part with an opinion can be, and
//! it is the part that would be wrong quietly. `%makefile-targets` decides what
//! the menu says, so a rule it misreads is a target you cannot reach and a
//! variable it mistakes for a rule is an entry that does nothing.
//!
//! The fixture is written to a temporary directory rather than pointed at this
//! repository's own Makefile, so the assertions describe make's grammar instead
//! of describing whatever this project happens to build today.
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

/// Evaluate FORM and wait for its printed value to appear in the status line.
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

#[test]
fn the_scanner_reads_rules_and_ignores_everything_else() {
    let dir = std::env::temp_dir().join("zemacs_test_project_make");
    std::fs::create_dir_all(&dir).unwrap();
    // Every line here is a claim about make's grammar, and the comment on each
    // says which. `\t` is load-bearing: a recipe line is one that starts with a
    // tab, and `install:` below is a recipe *for* `all`, not a target.
    std::fs::write(
        dir.join("Makefile"),
        "# a comment with a colon: not a rule\n\
         CARGO := cargo\n\
         FLAGS ::= --release\n\
         PREFIX = /usr/local\n\
         \n\
         .PHONY: build test fmt\n\
         \n\
         build:\n\
         \t$(CARGO) build $(FLAGS)\n\
         \n\
         test: build\n\
         \techo install: not a target\n\
         \n\
         fmt check:\n\
         \t$(CARGO) fmt\n\
         \n\
         %.o: %.c\n\
         \tcc -c $<\n\
         \n\
         $(BINS): build\n\
         \ttrue\n\
         \n\
         build:\n\
         \ttrue\n",
    )
    .unwrap();

    let init = std::env::temp_dir().join("zemacs_test_project_make_init.lisp");
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (message \"project-make test init loaded\")\n",
            // `split-string` lives here, and `%makefile-targets` calls it.
            runtime("modes/modes.lisp"),
            runtime("init.lisp"),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init.clone());
    wait_message(&shared, "the init file", |m| {
        m == "project-make test init loaded"
    });

    let makefile = dir.join("Makefile");
    lisp.eval(format!(
        "(defparameter *pm-targets* (%makefile-targets {:?}))",
        makefile.to_str().unwrap()
    ));

    // Declaration order, not sorted: a Makefile's first target is its default,
    // and a picker that reordered them would bury it.
    //
    // `fmt` and `check` both appear because one rule may name several targets.
    // `build` appears once even though it is declared twice, which is a thing
    // real Makefiles do and a duplicate entry in a picker is a bug.
    says(
        &shared,
        &lisp,
        "*pm-targets*",
        "(build test fmt check)",
    );

    // The four kinds of line that are not rules, each asserted by absence:
    // an assignment (`CARGO`, `FLAGS`, `PREFIX`), a dot-target (`.PHONY`), a
    // pattern rule (`%.o`), and a computed name (`$(BINS)`). `install` is the
    // interesting one — it has a colon and sits on a recipe line, so only the
    // leading-tab test keeps it out.
    for absent in [
        "CARGO", "FLAGS", "PREFIX", ".PHONY", "%.o", "$(BINS)", "install",
    ] {
        says(
            &shared,
            &lisp,
            &format!("(if (member {absent:?} *pm-targets* :test #'string=) \"yes\" \"no\")"),
            "no",
        );
    }

    // A directory with no Makefile above it answers NIL rather than erroring,
    // which is what lets `project-make` say "no Makefile here or above".
    says(&shared, &lisp, "(%makefile-near #p\"/\")", "NIL");

    // ...and the climb finds one that is not in the directory you start from.
    let below = dir.join("a").join("b");
    std::fs::create_dir_all(&below).unwrap();
    says(
        &shared,
        &lisp,
        &format!(
            "(if (%makefile-near {:?}) \"found\" \"missing\")",
            format!("{}/", below.to_str().unwrap())
        ),
        "found",
    );
}
