//! Headless proof of `runtime/modes/math-code.lisp` — a programming problem in
//! a curriculum, turned into somewhere to work.
//!
//! **Nothing here installs a package or forks a build.** That is not a
//! concession, it is the design being tested: everything this feature does to
//! the outside world is a *string* — a shell script and a `terminal-run` verb —
//! computed by functions that can be read without running them, exactly as
//! `%ai-verb` is in `ai.rs`. So the assertions below are about paths, about
//! text, and about what arrives on the channel the app drains; the one moving
//! part left, `ext:run-program`, is reached only through a `python3` that
//! deliberately does not exist, which is also how the "no Python" path is
//! proved.
//!
//! What is worth the file, in order of how much it would hurt to get wrong:
//!
//! 1. **Tangling never clobbers.** The template is written when the file is not
//!    there and never again, and the proof is an edited program surviving the
//!    keystroke you press to look at the problem.
//! 2. `:tangle no` and a path that climbs out of the curriculum are *refused*,
//!    with a sentence rather than a backtrace.
//! 3. The venv is derived from the .org file's path and one curriculum has one,
//!    with packages accumulated from the defaults and the problem's own.
//! 4. A machine with no `python3` says so and does nothing else.
//!
//! Deliberately a single `#[test]`, as in every file beside it: `cl_boot`
//! initialises a process-wide Lisp image, so there is exactly one `spawn` per
//! test binary and a new file is the only way to add one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Key, Mode, PromptKind, Shared};

const PATIENCE: Duration = Duration::from_secs(30);
static NTH: AtomicUsize = AtomicUsize::new(0);

fn wait<T>(shared: &Shared, what: &str, f: impl Fn(&Editor) -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(v) = f(&shared.lock().unwrap()) {
            return v;
        }
        if Instant::now() >= deadline {
            let ed = shared.lock().unwrap();
            let seen = &ed.messages[ed.messages.len().saturating_sub(10)..];
            panic!("timed out waiting for {what}; status={:?} last={seen:#?}", ed.status);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_message(shared: &Shared, what: &str, pred: impl Fn(&str) -> bool) {
    wait(shared, what, |ed| ed.messages.iter().any(|m| pred(m)).then_some(()));
}

/// Ask the image a question and get its answer back, printed with `~a`.
///
/// Numbered, because the log is cumulative and several answers below are the
/// same word: without the counter a second `T` would be satisfied by the first
/// and the wait would prove nothing. Errors come back as text rather than as a
/// timeout, which is the difference between a failure you can read and one you
/// have to bisect.
fn probe(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str) -> String {
    let tag = NTH.fetch_add(1, Ordering::Relaxed);
    lisp.eval(format!(
        "(message (format nil \"#{tag} ~a\" (handler-case {form} (serious-condition (e) (format nil \"ERROR ~a\" e)))))"
    ));
    let prefix = format!("#{tag} ");
    wait(shared, form, |ed| {
        ed.messages
            .iter()
            .find(|m| m.starts_with(&prefix))
            .map(|m| m[prefix.len()..].to_string())
    })
}

/// Run `form` for its effect and wait until it has landed.
fn does(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str) {
    let tag = NTH.fetch_add(1, Ordering::Relaxed);
    lisp.eval(format!("(progn {form} (message \"#{tag} done\"))"));
    let want = format!("#{tag} done");
    wait_message(shared, form, |m| m == want);
}

/// Keys through the real `handle_key`, with the one command core cannot perform
/// itself — calling the image back with a prompt's answer — handed to the Lisp
/// thread here. Transcribed from `ai.rs`, which took it from `prompt.rs`.
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

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// One unit, three problems: written (no program at all), programming (the one
/// everything below is about), and programming with `:tangle no` — which is a
/// block that says "do not write me out" and must be told apart from a block
/// with no `:tangle` at all.
const CURRICULUM: &str = "\
#+TITLE: Linear Algebra I
#+ZEMACS_CURRICULUM: 1

* Vectors and Spaces
:PROPERTIES:
:ID: unit-1
:END:

A basis is a linearly independent spanning set.

** Problem
:PROPERTIES:
:ZEMACS_PROBLEM: written
:ZEMACS_STATUS: todo
:END:

Prove that every basis has the same cardinality.

*** Response

** Problem
:PROPERTIES:
:ZEMACS_PROBLEM: programming
:ZEMACS_PACKAGES: numpy sympy
:ZEMACS_STATUS: todo
:END:

Implement Gaussian elimination with partial pivoting.

#+begin_src python :tangle gauss_test.py
import numpy as np

def solve(A, b):
    ...

if __name__ == \"__main__\":
    print(solve(np.eye(2), np.ones(2)))
#+end_src

** Problem
:PROPERTIES:
:ZEMACS_PROBLEM: programming
:ZEMACS_STATUS: todo
:END:

Read this one; it is not meant to become a file.

#+begin_src python :tangle no
print(\"nothing to see\")
#+end_src
";

#[test]
fn a_programming_problem_becomes_a_program_a_venv_and_a_run_key() {
    // A directory of our own, emptied first: every path below is derived from
    // the .org file's, so a leftover `.venv` from a previous run would make
    // "the environment is not built" untestable.
    let root = std::env::temp_dir().join("zemacs-math-code-test");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // Canonicalised, because `%math-code-curriculum-of` confirms with
    // `probe-file` and therefore answers a *truename* — and on macOS the
    // system temp directory is reached through a symlink.
    let root = std::fs::canonicalize(&root).unwrap();

    let org = root.join("algebra.org");
    std::fs::write(&org, CURRICULUM).unwrap();
    let dir = root.join("algebra"); // the programs directory, named from the file
    let program = dir.join("gauss_test.py");
    let venv = dir.join(".venv");

    let init = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp"),
    )
    .expect("runtime/init.lisp must exist");

    let (tx, rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);

    wait_message(&shared, "init.lisp to finish loading", |m| {
        m.contains("is driving the editor")
    });
    // The config loads this file at all. Without this line every failure below
    // would look like a bug in the feature rather than a missing `load`.
    assert_eq!(
        probe(&shared, &lisp, "(if (fboundp 'math-code-edit) \"yes\" \"no\")"),
        "yes",
        "runtime/init.lisp must load modes/math-code.lisp"
    );

    {
        let mut ed = shared.lock().unwrap();
        ed.load(CURRICULUM, Some(org.clone()), Some("org".into()));
        ed.apply(EditorCommand::SetMode(Mode::Normal));
        ed.apply(EditorCommand::MoveTo(0));
    }

    // No Python anywhere, for the whole test. Two things at once: nothing below
    // can fork a real `venv` or reach the network, and the degradation this
    // produces is itself one of the things being proved.
    does(&shared, &lisp, "(setf *math-code-python* \"zemacs-no-such-python-9f3c\")");

    // --- where things go ------------------------------------------------------
    //
    // Derived from the .org file's path and nothing else: no table, no cache
    // key, nothing to go stale when the curriculum is renamed.
    let quoted = format!("{:?}", org.display().to_string());
    assert_eq!(
        probe(&shared, &lisp, &format!("(namestring (%math-code-dir {quoted}))")),
        format!("{}/", dir.display()),
        "the programs live in a directory beside the .org file, named from it"
    );
    assert_eq!(
        probe(&shared, &lisp, &format!("(namestring (%math-code-venv {quoted}))")),
        format!("{}/", venv.display()),
        "one venv per curriculum, inside that directory"
    );
    assert_eq!(
        probe(&shared, &lisp, &format!("(namestring (%math-code-interpreter {quoted}))")),
        format!("{}/bin/python", venv.display()),
    );
    // ...and the inverse, which is how a program buffer knows what it belongs to
    // without anything having to remember.
    assert_eq!(
        probe(
            &shared,
            &lisp,
            &format!("(namestring (%math-code-curriculum-of {:?}))", program.display().to_string())
        ),
        org.display().to_string(),
    );
    // A Python file that is not in a curriculum's directory belongs to nobody.
    assert_eq!(
        probe(&shared, &lisp, "(%math-code-curriculum-of \"/etc/hosts\")"),
        "NIL",
    );
    // Neither does a *directory*, which is what `buffer-file-name` answers in
    // dired — and which sits next to an .org file as often as anything does.
    assert_eq!(
        probe(&shared, &lisp, &format!("(%math-code-curriculum-of {:?})", format!("{}/", dir.display()))),
        "NIL",
    );

    // --- packages -------------------------------------------------------------
    //
    // The defaults, then the problem's own, deduplicated. numpy is not optional:
    // a skeleton that says `import numpy as np` and then makes you install it
    // has failed at the one thing this file is for.
    assert_eq!(
        probe(
            &shared,
            &lisp,
            "(format nil \"~{~a~^|~}\" (math-code-packages (second (math-problems))))"
        ),
        "numpy|sympy",
    );
    // A written problem declares none and still gets the defaults, so asking
    // about one is never a special case.
    assert_eq!(
        probe(
            &shared,
            &lisp,
            "(format nil \"~{~a~^|~}\" (math-code-packages (first (math-problems))))"
        ),
        "numpy",
    );
    // A problem that lists numpy itself does not ask for it twice.
    assert_eq!(
        probe(
            &shared,
            &lisp,
            "(format nil \"~{~a~^|~}\" (math-code-packages (list :packages '(\"NumPy\" \"scipy\"))))"
        ),
        "numpy|scipy",
    );

    // --- tangling, once -------------------------------------------------------
    //
    // Point in the programming problem, and one command. It writes the template,
    // splits the window and opens it — `find-file` needs the app, so what is
    // asserted here is the file and the split; the buffer it would land in is
    // the app's half.
    does(&shared, &lisp, "(goto-char (search-forward \"Implement Gaussian\" 0))");
    assert_eq!(probe(&shared, &lisp, "(length (window-list))"), "1");
    does(&shared, &lisp, "(math-code-edit)");

    let template = read(&program);
    assert!(
        template.starts_with("import numpy as np\n"),
        "the block's body is what lands on disk: {template:?}"
    );
    assert!(
        template.contains("if __name__ == \"__main__\":"),
        "...all of it, runner included: {template:?}"
    );
    assert_eq!(
        probe(&shared, &lisp, "(length (window-list))"),
        "2",
        "the program opens beside the curriculum, in its own column"
    );
    wait_message(&shared, "the tangled message", |m| {
        m.contains("gauss_test.py") && m.contains("tangled")
    });

    // ...and **never again**. This is the keystroke you press to *look* at a
    // problem; if it could overwrite the file, it would cost somebody an
    // afternoon exactly once and they would never trust it again.
    std::fs::write(&program, "# an afternoon's work\nprint(42)\n").unwrap();
    does(&shared, &lisp, "(math-code-edit)");
    assert_eq!(
        read(&program),
        "# an afternoon's work\nprint(42)\n",
        "opening a problem a second time must not re-tangle over the program"
    );

    // --- the blocks that are not programs -------------------------------------
    //
    // `:tangle no` is a *decision* by the curriculum and arrives as the string
    // "no", which is why `math-src-block` keeps the header argument verbatim.
    does(&shared, &lisp, "(goto-char (search-forward \"nothing to see\" 0))");
    does(&shared, &lisp, "(math-code-edit)");
    wait_message(&shared, "the :tangle no refusal", |m| m.contains(":tangle no"));
    assert!(
        !dir.join("no").exists() && std::fs::read_dir(&dir).unwrap().count() == 1,
        "a `:tangle no` block writes nothing at all"
    );
    // Neither does a name that would climb out of the curriculum's directory. A
    // curriculum is generated text and must not be able to name `/etc/crontab`.
    assert_eq!(
        probe(&shared, &lisp, "(%math-code-tangle-name '(:tangle \"../../evil.py\"))"),
        "NIL",
    );
    assert_eq!(
        probe(&shared, &lisp, "(%math-code-tangle-name '(:tangle \"/etc/crontab\"))"),
        "NIL",
    );
    assert_eq!(
        probe(&shared, &lisp, "(%math-code-tangle-name '(:tangle \"gauss_test.py\"))"),
        "gauss_test.py",
    );
    // ...and a written problem has no block to refuse, which is a different
    // sentence again.
    does(&shared, &lisp, "(goto-char (search-forward \"Prove that every basis\" 0))");
    does(&shared, &lisp, "(math-code-edit)");
    wait_message(&shared, "the written-problem message", |m| {
        m.contains("written work")
    });

    // --- no python3 -----------------------------------------------------------
    //
    // Said clearly, once, and nothing else happens: no directory is made for an
    // environment that cannot be built, and no half-written venv is left behind.
    does(&shared, &lisp, "(goto-char (search-forward \"Implement Gaussian\" 0))");
    does(&shared, &lisp, "(math-code-env)");
    wait_message(&shared, "the no-python message", |m| {
        m.contains("zemacs-no-such-python-9f3c") && m.contains("$PATH")
    });
    assert!(!venv.exists(), "nothing may be created when there is no interpreter");
    assert_eq!(
        probe(&shared, &lisp, "(if *math-code-build* \"building\" \"idle\")"),
        "idle",
        "and no child is left in flight"
    );

    // --- the build really is asynchronous -------------------------------------
    //
    // The riskiest thing in the file, so it is proved with a real fork: a stub
    // standing in for `python3` that makes the directory `-m venv` would have
    // made and returns success for everything else. No pip and no network — what
    // is being tested is `:wait nil`, the poll, and the two seconds the image
    // must *not* spend waiting.
    //
    // Its own curriculum, in its own directory, so the environment it builds
    // cannot disturb the assertions above or below.
    let other = root.join("async.org");
    std::fs::write(&other, CURRICULUM).unwrap();
    let stub = root.join("python3-stub");
    std::fs::write(
        &stub,
        "#!/bin/sh\n\
         # Stands in for python3. `$2` is `venv` or `pip`; anything else succeeds.\n\
         sleep 1\n\
         if [ \"$2\" = venv ]; then\n\
           mkdir -p \"$3/bin\" && printf '#!/bin/sh\\nexit 0\\n' > \"$3/bin/python\"\n\
           chmod +x \"$3/bin/python\"\n\
         fi\n\
         exit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let quoted_other = format!("{:?}", other.display().to_string());
    does(
        &shared,
        &lisp,
        &format!(
            "(progn (setf *math-code-python* {:?}) (%math-code-build {quoted_other} '(\"numpy\")))",
            stub.display().to_string()
        ),
    );
    // The build is a second long and the image answered before it finished.
    // That *is* the requirement: a form that waits for pip costs every other
    // keystroke's Lisp for as long as it waits.
    assert_eq!(
        probe(&shared, &lisp, "(if *math-code-build* \"building\" \"idle\")"),
        "building",
        "the build must not be waited for"
    );
    // ...and it is noticed afterwards, by the poller that runs on cursor
    // movement. Called by hand here for the same reason `python-mode-hook` is:
    // nothing in a headless test moves the cursor on its own.
    let deadline = Instant::now() + PATIENCE;
    while probe(&shared, &lisp, "(progn (math-code-build-poll) (if *math-code-build* \"building\" \"idle\"))")
        == "building"
    {
        assert!(Instant::now() < deadline, "the stub build never finished");
        std::thread::sleep(Duration::from_millis(50));
    }
    wait_message(&shared, "the environment-ready message", |m| {
        m.contains("environment ready") && m.contains("numpy")
    });
    assert_eq!(
        std::fs::read_to_string(root.join("async/.venv/.zemacs-packages")).unwrap(),
        "numpy\n",
        "the record is written by the script, after the install returned"
    );
    assert_eq!(
        probe(&shared, &lisp, &format!("(if (math-code-ready-p {quoted_other}) \"yes\" \"no\")")),
        "yes",
    );
    // The log is a real file with the script's own output in it, which is what
    // the failure message points at.
    assert!(
        std::fs::read_to_string(root.join("async/.zemacs-setup.log"))
            .unwrap()
            .contains("environment ready"),
        "the build writes a log rather than filling a pipe nobody drains"
    );
    does(&shared, &lisp, "(setf *math-code-python* \"zemacs-no-such-python-9f3c\")");

    // --- what \"built\" means ---------------------------------------------------
    //
    // A venv is a directory with an interpreter in it, and the record of what is
    // installed lives *inside* it — so deleting `.venv` deletes the claim, and
    // the two can never disagree. Both are made by hand here: this test does not
    // get to spend forty seconds and a network on pip.
    std::fs::create_dir_all(venv.join("bin")).unwrap();
    std::fs::write(venv.join("bin/python"), "#!/bin/sh\nexit 0\n").unwrap();
    assert_eq!(
        probe(&shared, &lisp, &format!("(if (math-code-ready-p {quoted} '(\"numpy\")) \"yes\" \"no\")")),
        "no",
        "an interpreter with no packages in it is not a built environment"
    );
    std::fs::write(venv.join(".zemacs-packages"), "numpy\n").unwrap();
    assert_eq!(
        probe(&shared, &lisp, &format!("(if (math-code-ready-p {quoted} '(\"numpy\")) \"yes\" \"no\")")),
        "yes",
    );
    // Case-insensitively, because PyPI is.
    assert_eq!(
        probe(&shared, &lisp, &format!("(format nil \"~{{~a~^|~}}\" (math-code-missing {quoted} '(\"NumPy\" \"sympy\")))")),
        "sympy",
        "only what is actually missing is asked for; the rest accumulates"
    );

    // --- the script that builds it --------------------------------------------
    //
    // Read rather than run. Every path in it is single-quoted, which is what
    // makes a curriculum in `~/My Documents` build, and the record is appended
    // only after pip has returned — `set -e` is what makes that a promise.
    let script = probe(
        &shared,
        &lisp,
        &format!("(substitute #\\Space #\\Newline (math-code-setup-text {quoted} '(\"numpy\" \"sympy\")))"),
    );
    assert!(script.contains("set -e"), "{script}");
    assert!(
        script.contains(&format!("'zemacs-no-such-python-9f3c' -m venv '{}/'", venv.display())),
        "the venv is built with the configured interpreter: {script}"
    );
    assert!(
        script.contains(&format!("'{}/bin/python' -m pip install", venv.display())),
        "...and packages go in with the venv's own python, not the system one: {script}"
    );
    assert!(script.contains("'numpy' 'sympy'"), "{script}");
    assert!(
        script.contains(&format!(">> '{}/.zemacs-packages'", venv.display())),
        "the record is written last, so a failed install is not recorded: {script}"
    );
    // An environment with nothing to install must not run `pip install` with no
    // arguments — `set -e` would then throw away a venv that was just built.
    let empty = probe(
        &shared,
        &lisp,
        &format!("(if (search \"pip install\" (math-code-setup-text {quoted} nil)) \"yes\" \"no\")"),
    );
    assert_eq!(empty, "no");

    // --- the run gesture ------------------------------------------------------
    //
    // One key, one verb, one terminal. The verb is the entire interface between
    // this file and `crates/app/src/term.rs`, so it is pinned here the way
    // `ai.rs` pins the harnesses'.
    let script_path = dir.join(".zemacs-run-gauss_test.sh");
    assert_eq!(
        probe(
            &shared,
            &lisp,
            &format!("(math-code-run-verb {quoted} {:?})", program.display().to_string())
        ),
        // `rerun:` and not `run:`. Both name the session after the program, so
        // the buffer says whose output it is; the difference is what a *second*
        // press does. `run:` starts another session — right for an agent, since
        // two side by side is the point — and `rerun:` replaces the one of that
        // name, which is right for a program you are editing and running over
        // and over. With `run:` here, `*gauss_test*<2>`, `<3>` piled up in the
        // switcher, every one of them finished.
        format!("rerun:gauss_test:/bin/sh {}", script_path.display()),
        "the session is named after the program, so the buffer says whose output it is"
    );

    // ...and end to end, down the channel the app drains. `Term` needs the app,
    // so what arrives here is exactly what `term.rs` would parse — and nothing
    // is forked, because nothing in this test is servicing that channel.
    while rx.try_recv().is_ok() {}
    // The environment has to look built for the run to be allowed at all: a
    // program run against half a venv fails with an ImportError that blames the
    // program.
    std::fs::write(venv.join(".zemacs-packages"), "numpy\nsympy\n").unwrap();
    does(&shared, &lisp, "(math-code-run)");
    let verb = loop {
        match rx.recv_timeout(PATIENCE) {
            Ok(EditorCommand::Term(verb)) => break verb,
            Ok(_) => continue,
            Err(e) => panic!("no terminal verb arrived: {e}"),
        }
    };
    assert_eq!(verb, format!("rerun:gauss_test:/bin/sh {}", script_path.display()));

    let runner = read(&script_path);
    assert!(
        runner.contains(&format!("cd '{}/'", dir.display())),
        "the program runs in its own directory, whichever window the terminal was spawned from: {runner}"
    );
    assert!(
        runner.contains(&format!(
            "exec '{}/bin/python' '{}'",
            venv.display(),
            program.display()
        )),
        "...with the venv's interpreter, and `exec` so there is one process to interrupt: {runner}"
    );

    // A run with no environment yet does not reach the terminal at all.
    std::fs::remove_file(venv.join(".zemacs-packages")).unwrap();
    while rx.try_recv().is_ok() {}
    does(&shared, &lisp, "(math-code-run)");
    assert!(
        rx.try_recv().is_err(),
        "nothing is spawned against an environment that is not ready"
    );
    std::fs::write(venv.join(".zemacs-packages"), "numpy\nsympy\n").unwrap();

    // --- the same run, with point nowhere near it -----------------------------
    //
    // `math-code-run` starts from `(point)`, and a *scene* has none: a curriculum
    // rendered as a page has no cursor, and the buffer's point is wherever the
    // keyboard last left it — very likely another problem. So a click carries the
    // problem it was built for and calls the sibling, and this is that sibling
    // asked to run problem two while point sits in the preamble, inside no
    // problem at all.
    does(&shared, &lisp, "(goto-char 0)");
    assert_eq!(
        probe(&shared, &lisp, "(if (math-problem-at-point) \"yes\" \"no\")"),
        "no",
        "point has to be somewhere that would give the wrong answer for this to prove anything"
    );
    while rx.try_recv().is_ok() {}
    does(&shared, &lisp, "(math-code-run-problem (second (math-problems)))");
    let verb = loop {
        match rx.recv_timeout(PATIENCE) {
            Ok(EditorCommand::Term(verb)) => break verb,
            Ok(_) => continue,
            Err(e) => panic!("no terminal verb arrived: {e}"),
        }
    };
    assert_eq!(
        verb,
        format!("rerun:gauss_test:/bin/sh {}", script_path.display()),
        "the problem it was handed is the block that ran, not the one point is in"
    );
    // ...and the third problem is `:tangle no`, so the sibling refuses for the
    // block's own reason. The sentence itself is pinned above, where
    // `math-code-edit` is asked the same thing; what matters here is that being
    // handed a problem does not talk anything past a refusal.
    while rx.try_recv().is_ok() {}
    does(&shared, &lisp, "(math-code-run-problem (third (math-problems)))");
    assert!(rx.try_recv().is_err(), "a block that says do not write me out is not run");

    // The point-reading command is now a call to its sibling, so `SPC m x` from
    // the preamble still says the one sentence it always said. Read out of the
    // messages *since* the call, because the log is cumulative and a refusal
    // matched anywhere in it would prove nothing.
    let from = shared.lock().unwrap().messages.len();
    does(&shared, &lisp, "(math-code-run)");
    let said = shared.lock().unwrap().messages[from..].to_vec();
    assert!(
        said.iter().any(|m| m == "math: no problem at point"),
        "reading point is still what the key does: {said:#?}"
    );

    // --- the program's own buffer ---------------------------------------------
    //
    // Opening a curriculum's program turns on `math-code`, which is where its
    // keys live. The hook is what the app calls when a buffer enters
    // python-mode; calling it here is the one part of that path a headless test
    // cannot get for free.
    {
        let mut ed = shared.lock().unwrap();
        ed.load(&read(&program), Some(program.clone()), Some("python".into()));
    }
    does(&shared, &lisp, "(python-mode-hook)");
    assert_eq!(
        probe(&shared, &lisp, "(if (minor-mode-p 'math-code) \"on\" \"off\")"),
        "on",
        "a file in a curriculum's directory gets the curriculum's program keys"
    );
    // ...and from there the program knows its own curriculum, with nothing
    // remembered anywhere.
    assert_eq!(
        probe(&shared, &lisp, "(namestring (getf (%math-code-here) :org))"),
        org.display().to_string(),
    );
    // The keys themselves. `SPC m r` is the same in both buffers on purpose.
    assert_eq!(
        probe(
            &shared,
            &lisp,
            "(format nil \"~{~a~^|~}\" (sort (mapcar #'first (where-is \"math-code-run\")) #'string<))"
        ),
        "math-code|math-curriculum",
    );

    // ...and the way back. The curriculum is still in the window `math-code-edit`
    // split off from, so this goes *there* rather than opening a second copy of
    // it over the program — `%repl-show`'s rule, and the reason the split
    // survives a round trip.
    while rx.try_recv().is_ok() {}
    does(&shared, &lisp, "(math-code-back)");
    assert_eq!(probe(&shared, &lisp, "(buffer-name)"), "algebra.org");
    assert!(
        rx.try_recv().is_err(),
        "a curriculum already on screen is selected, not re-opened"
    );

    // A Python buffer that is *not* a curriculum's program is left alone, which
    // is the whole reason this is a minor mode rather than a change to
    // python-mode.
    {
        let mut ed = shared.lock().unwrap();
        ed.load("print(1)\n", Some(PathBuf::from("/tmp/zemacs-not-a-curriculum.py")), Some("python".into()));
    }
    does(&shared, &lisp, "(python-mode-hook)");
    assert_eq!(
        probe(&shared, &lisp, "(if (minor-mode-p 'math-code) \"on\" \"off\")"),
        "off",
    );
    // ...and the commands say so rather than guessing at a curriculum.
    does(&shared, &lisp, "(math-code-back)");
    wait_message(&shared, "the not-a-program message", |m| {
        m.contains("not a curriculum program")
    });

    // --- putting the template back --------------------------------------------
    //
    // The one destructive command, so it asks first — and the question is the
    // test: a reset that did not ask would be the re-tangling this whole design
    // refuses, wearing a different name.
    {
        let mut ed = shared.lock().unwrap();
        ed.load(CURRICULUM, Some(org.clone()), Some("org".into()));
        ed.apply(EditorCommand::SetMode(Mode::Normal));
    }
    does(&shared, &lisp, "(goto-char (search-forward \"Implement Gaussian\" 0))");
    lisp.eval("(math-code-reset)".into());
    wait(&shared, "the reset confirmation", |ed| {
        let p = ed.prompt.as_ref()?;
        (matches!(p.kind, PromptKind::Lisp { .. }) && p.label.contains("Reset gauss_test.py"))
            .then_some(())
    });
    assert_eq!(
        read(&program),
        "# an afternoon's work\nprint(42)\n",
        "nothing is written while the question is still on screen"
    );

    // The program is still a buffer from the section above, so this is the path
    // that rewrites the *buffer* — one `replace-region`, and therefore one undo,
    // which is the whole reason it is preferred to writing the file.
    let flat = |text: &str| text.replace('\n', " ").trim_end().to_string();
    let buffer = "(substitute #\\Space #\\Newline (with-current-buffer \"gauss_test.py\" (buffer-string)))";
    say_yes(&shared, &lisp);
    wait_message(&shared, "the reset to land", |m| m.contains("u undoes it"));
    assert_eq!(
        flat(&probe(&shared, &lisp, buffer)),
        flat(&template),
        "answering yes puts the curriculum's template back, exactly as tangled"
    );
    does(&shared, &lisp, "(with-current-buffer \"gauss_test.py\" (undo))");
    assert_eq!(
        flat(&probe(&shared, &lisp, buffer)),
        flat("# an afternoon's work\nprint(42)\n"),
        "the reset is a single edit, so one `u` takes the program back"
    );

    // ...and with no buffer of it open, the *file* is rewritten. That is the
    // case the question is really about: there is nothing to undo afterwards.
    does(&shared, &lisp, "(kill-buffer \"gauss_test.py\")");
    lisp.eval("(math-code-reset)".into());
    wait(&shared, "the second reset confirmation", |ed| {
        ed.prompt.as_ref().map(|p| p.label.contains("Reset gauss_test.py")).unwrap_or(false)
            .then_some(())
    });
    say_yes(&shared, &lisp);
    wait_message(&shared, "the file to be rewritten", |m| {
        m.contains("rewritten from the template")
    });
    assert_eq!(read(&program), template);
}

/// Answer the confirmation the way a user does: type the word and press Enter.
fn say_yes(shared: &Shared, lisp: &zemacs_lisp::Lisp) {
    for key in "yes".chars() {
        feed(shared, lisp, &[Key::Char(key)]);
    }
    feed(shared, lisp, &[Key::Enter]);
}
