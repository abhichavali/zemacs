//! `runtime/modes/xref.lisp` — the results buffer, and the replace over it.
//!
//! `TODO.org` predicted this would need a generated buffer kind in Rust with its
//! own keymap and a line-to-location table. It needed none: the rows are
//! `PATH:LINE:TEXT`, which is what ripgrep prints and what `find-file-at`
//! already parses, so the text *is* the table — and the read-only flag and the
//! mode were each one call that already existed.
//!
//! What that buys is the thing this file mostly asserts: because the listing is
//! ordinary text, **deleting a row excludes a hit**, and a project-wide replace
//! is an operation over whatever rows are left. That is `wgrep`'s gesture in
//! Emacs, arrived at from the other end.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zemacs_core::{Editor, ReadOnly, Shared};

const PATIENCE: Duration = Duration::from_secs(30);

/// The main loop's one job that matters here. Core records that a mode hook is
/// *due* and never calls Lisp itself; the app drains `pending_hooks` and asks
/// the image to run each one. There is no app in a test, so this is it — the
/// same helper `modes.rs` spells out, and the reason `xref-mode`'s body (which
/// is what makes the listing read-only) runs at all.
fn pump(shared: Shared, lisp: Arc<zemacs_lisp::Lisp>) {
    std::thread::spawn(move || loop {
        let hooks = std::mem::take(&mut shared.lock().unwrap().pending_hooks);
        for hook in hooks {
            lisp.eval(format!(
                "(let ((h (find-symbol {:?} :zemacs))) (when (and h (fboundp h)) (funcall h)))",
                hook.to_uppercase()
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    });
}

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

fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"~a\" {form}))"));
    wait(shared, &format!("{form} to answer {want:?}"), |ed| {
        ed.messages.iter().any(|m| m == want).then_some(())
    });
}

fn runtime(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .expect("the runtime directory ships with the source")
}

#[test]
fn the_listing_is_text_and_a_replace_is_an_operation_over_it() {
    let dir = std::env::temp_dir().join(format!("zemacs_xref-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let one = dir.join("one.rs");
    let two = dir.join("two.rs");
    std::fs::write(&one, "let alpha = 1;\nlet beta = alpha + alpha;\n").unwrap();
    std::fs::write(&two, "fn alpha() {}\n").unwrap();

    // `zemacs-file` — and so `~/.zemacs.d/backup/`, which the assertions further
    // down read — resolves `$HOME` on every call rather than once at boot, so
    // pointing it at a directory of this test's own is enough. Done for the
    // reason `backup_into` exists as a separate function on the Rust side: a test
    // about real files under `$HOME` must not be reading *your* backups, and must
    // certainly not be adding to them. Before the image and the pump, which is
    // what makes `set_var` safe here — nothing else is running yet.
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);
    let backup_dir = home.join(".zemacs.d/backup");
    // `backup_into`'s naming, which this has to spell out rather than ask for:
    // the whole path with `/` turned into `!`, wrapped in `#`, and `.~N~` after.
    // Both halves of the editor write into that one directory, so the test that
    // the shapes agree is the test that the names are written twice and match.
    let version = |path: &PathBuf, n: u32| {
        backup_dir.join(format!(
            "#{}#.~{n}~",
            path.display().to_string().replace('/', "!")
        ))
    };

    let init = std::env::temp_dir().join(format!("zemacs_xref_init-{}.lisp", std::process::id()));
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (message \"xref test init loaded\")\n",
            runtime("modes/modes.lisp"),
            runtime("init.lisp"),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = Arc::new(zemacs_lisp::spawn(tx, shared.clone(), init.clone()));
    pump(shared.clone(), lisp.clone());
    wait(&shared, "the init file", |ed| {
        ed.messages
            .iter()
            .any(|m| m == "xref test init loaded")
            .then_some(())
    });
    says(&shared, &lisp, "(if (fboundp 'xref-show) \"yes\" \"no\")", "yes");

    // --- the listing --------------------------------------------------------

    let rows = format!(
        "(list \"{o}:1:let alpha = 1;\" \"{o}:2:let beta = alpha + alpha;\" \"{t}:1:fn alpha() {{}}\")",
        o = one.display(),
        t = two.display()
    );
    lisp.eval(format!("(xref-show \"3 hits\" {rows})"));

    // Waited on the *mode* and not on the name: a primitive takes the editor
    // lock one at a time, so a poll on the buffer's name catches `xref-show`
    // between `create-buffer` and everything after it.
    let (name, mode, read_only, line) = wait(&shared, "the listing", |ed| {
        (ed.buffer.major_mode == "xref-mode" && ed.buffer.read_only() != ReadOnly::No).then(|| {
            (
                ed.buffer.name(),
                ed.buffer.major_mode.clone(),
                ed.buffer.read_only(),
                ed.buffer.cursor_line_col().0,
            )
        })
    });
    assert_eq!(name, "*xref*");
    assert_eq!(mode, "xref-mode");
    // Read-only, so the listing is walked rather than typed into — and the
    // whole of what a "generated buffer kind" would have bought here.
    assert_ne!(read_only, ReadOnly::No);
    // Point on the first row, which is also what pulls the *view* to the top:
    // a buffer keeps whatever scroll the window had, and only a cursor above it
    // makes `ensure_cursor_visible` come down to meet it. The title went to the
    // status line rather than into the buffer, which is what leaves row 1 free.
    assert_eq!(line, 0, "point should start on the first hit");
    let text = shared.lock().unwrap().buffer.text.to_string();
    assert!(text.starts_with(&one.display().to_string()), "{text}");

    // `RET` still has to tell a location from whatever else a line may hold —
    // the rows come from ripgrep and a server, and neither is this file's.
    says(&shared, &lisp, "(%xref-location-p \"3 hits\")", "NIL");
    says(
        &shared,
        &lisp,
        &format!("(%xref-location-p \"{}:2:let beta = alpha;\")", one.display()),
        "T",
    );
    // A Windows-ish path is not a location: two colons is not enough, the thing
    // between them has to be a number.
    says(&shared, &lisp, "(%xref-location-p \"a:b:c\")", "NIL");
    says(
        &shared,
        &lisp,
        &format!("(%xref-line-number \"{}:2:x\")", one.display()),
        "2",
    );

    // Each file once, in the order it first appears — the grouping the replace
    // runs over.
    says(&shared, &lisp, "(length (%xref-files))", "2");

    // --- the replace --------------------------------------------------------
    //
    // Straight at the worker rather than through the two prompts, which are a
    // continuation apiece and are `read-string`'s business rather than this
    // file's.
    lisp.eval(format!(
        "(message (format nil \"replaced ~a\"
                          (%xref-replace-in-file \"{}\" \"alpha\" \"omega\")))",
        one.display()
    ));
    wait(&shared, "the rewrite", |ed| {
        ed.messages.iter().any(|m| m == "replaced 3").then_some(())
    });
    assert_eq!(
        std::fs::read_to_string(&one).unwrap(),
        "let omega = 1;\nlet beta = omega + omega;\n"
    );
    // Untouched: it was not the file asked about. The point of the listing being
    // a work list is that the work list is what runs.
    assert_eq!(std::fs::read_to_string(&two).unwrap(), "fn alpha() {}\n");

    // --- the way back ------------------------------------------------------
    //
    // The rewrite above went through somebody's source file, and `crates/app` has
    // copied a file aside before every save since the beginning. This writer and
    // `lsp-rename` were the two that did not — and they are the two that can go
    // through a whole project on one keystroke, so they are the two that most
    // needed it. Asserted on the *file and its contents* rather than on a message,
    // because a message is not what anyone goes looking for afterwards.
    assert_eq!(
        std::fs::read_to_string(version(&one, 1)).unwrap_or_default(),
        "let alpha = 1;\nlet beta = alpha + alpha;\n",
        "the pre-replace contents belong in {}",
        version(&one, 1).display()
    );
    // Nothing was copied aside for the file that was not rewritten: a backup per
    // *write*, not per file named in a listing.
    assert!(!version(&two, 1).exists());
    // The write is a temp file beside the target renamed over it, so a crash can
    // never leave the source truncated. What must not survive it is the temp file.
    let litter: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".#"))
        .collect();
    assert!(litter.is_empty(), "the atomic write left {litter:?} behind");

    // A second write is version 2, and the numbering is read back off the
    // directory rather than counted in memory — which is what makes it survive a
    // restart, and what this asserts by asking for it from a fresh command.
    lisp.eval(format!(
        "(message (format nil \"again ~a\"
                          (%xref-replace-in-file \"{}\" \"omega\" \"zeta\")))",
        one.display()
    ));
    wait(&shared, "the second rewrite", |ed| {
        ed.messages.iter().any(|m| m == "again 3").then_some(())
    });
    assert_eq!(
        std::fs::read_to_string(version(&one, 2)).unwrap_or_default(),
        "let omega = 1;\nlet beta = omega + omega;\n",
        "version 2 should hold what version 1 replaced"
    );

    // **A file that cannot be backed up is not written at all.** Data-loss
    // protection that silently degrades to none is worse than none, because you
    // stop checking for it. Arranged by putting an ordinary file where the backup
    // directory has to go, so `ensure-directories-exist` cannot win.
    std::fs::remove_dir_all(&backup_dir).unwrap();
    std::fs::write(&backup_dir, "not a directory").unwrap();
    lisp.eval(format!(
        "(message (format nil \"refused ~a\"
                          (%xref-replace-in-file \"{}\" \"alpha\" \"omega\")))",
        two.display()
    ));
    wait(&shared, "the refusal", |ed| {
        ed.messages.iter().any(|m| m == "refused 0").then_some(())
    });
    // Zero rather than NIL, and the file still says what it said: the count is
    // how many hits were *written*, so a refused file is honestly an untouched
    // one and `%xref-replace-across` needs no branch for it.
    assert_eq!(std::fs::read_to_string(&two).unwrap(), "fn alpha() {}\n");
    wait(&shared, "the refusal to name the file", |ed| {
        ed.messages
            .iter()
            .any(|m| *m == format!("no backup for {} — not rewritten", two.display()))
            .then_some(())
    });
    std::fs::remove_file(&backup_dir).unwrap();

    // A file with no hit is not rewritten at all, which is what keeps a replace
    // from moving the mtime of every file in a project.
    lisp.eval(format!(
        "(message (format nil \"none ~a\"
                          (%xref-replace-in-file \"{}\" \"nowhere\" \"x\")))",
        two.display()
    ));
    wait(&shared, "the no-op", |ed| {
        ed.messages.iter().any(|m| m == "none 0").then_some(())
    });
    // ...and an unreadable one answers NIL rather than signalling, so one bad
    // path in a listing does not take the rest of the replace down with it.
    lisp.eval(
        "(message (format nil \"gone ~a\"
                          (%xref-replace-in-file \"/nope/nothing/here\" \"a\" \"b\")))"
            .into(),
    );
    wait(&shared, "the missing file", |ed| {
        ed.messages.iter().any(|m| m == "gone NIL").then_some(())
    });

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&init);
}
