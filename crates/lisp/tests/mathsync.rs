//! `runtime/modes/mathsync.lisp`: the folder-watcher, without the network.
//!
//! What is worth pinning here is not the API call — that is one `urllib` request
//! in a Python script — but the three things around it that are easy to get
//! quietly wrong:
//!
//!   * **shell quoting.** The child is `/bin/sh` running a generated script, and
//!     a macOS screenshot is called `Screenshot 2026-06-09 at 9.31.55 PM.png` —
//!     spaces, and a narrow no-break space before the `PM`. An unquoted path is
//!     not a bug, it is command injection, so the quoting is tested against
//!     names built to break it.
//!   * **the queue is a directory.** `done/` must not be offered, subdirectories
//!     must not be offered, and the oldest image must come first.
//!   * **the handoff.** A finished transcription goes into the register `p`
//!     pastes from, and its image moves to `done/` — an image left in place is
//!     transcribed again on the next scan, which is a second paid API call.
//!
//! No key, no network and no `python3` are involved: `%mathsync-finish` is given
//! the output file the child would have written and asked to do the rest.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::path::Path;
use std::time::{Duration, Instant};

use zemacs_core::{Editor, Shared};

const PATIENCE: Duration = Duration::from_secs(20);

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

/// Evaluate `form` and wait for its value to be `want`.
///
/// `tag` is load-bearing: `messages` accumulates, so an untagged assertion can
/// match an identical line an earlier step left behind and pass without the form
/// ever running.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, tag: &str, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"{tag}=~a\" {form}))"));
    let want = format!("{tag}={want}");
    wait_message(shared, &format!("{form} => {want}"), |m| m == want);
}

#[test]
fn the_queue_is_a_directory_and_the_answer_lands_in_the_register() {
    let init = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp"),
    )
    .expect("runtime/init.lisp must exist");

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    wait_message(&shared, "init.lisp to finish loading", |m| {
        m.contains("is driving the editor")
    });

    // The module loaded at all. `load-runtime-modules` catches a broken file and
    // reports it as a message, so without this the whole test would fail later
    // with "undefined function" instead of saying what actually went wrong.
    {
        let ed = shared.lock().unwrap();
        let complaint = ed.messages.iter().find(|m| m.contains("mathsync.lisp")).cloned();
        assert!(complaint.is_none(), "mathsync.lisp must load: {complaint:?}");
    }
    // ...and the script it shells out to ships beside the runtime, which is the
    // one thing a `runtime-file` lookup cannot recover from at the moment of use.
    says(&shared, &lisp, "script",
         "(and (probe-file (runtime-file \"mathsync_transcribe.py\")) t)", "T");

    // --- shell quoting ------------------------------------------------------
    //
    // A plain name is wrapped and nothing else happens to it.
    says(&shared, &lisp, "q-plain", "(%mathsync-quote \"/tmp/a.png\")", "'/tmp/a.png'");
    // Spaces stay inside the one word.
    says(&shared, &lisp, "q-space", "(%mathsync-quote \"/tmp/a b.png\")", "'/tmp/a b.png'");
    // The one that matters: a quote in the name must not end the word. The
    // `'\''` dance closes, escapes and reopens.
    says(&shared, &lisp, "q-quote", r#"(%mathsync-quote "/tmp/it's.png")"#,
         r#"'/tmp/it'\''s.png'"#);
    // ...and a name built to be a command is inert, because `;` and `$()` are
    // ordinary characters inside single quotes.
    says(&shared, &lisp, "q-inject",
         r#"(%mathsync-quote "/tmp/x.png; rm -rf ~")"#, r#"'/tmp/x.png; rm -rf ~'"#);

    // --- what counts as an image -------------------------------------------
    says(&shared, &lisp, "img-png", "(and (%mathsync-image-p #p\"/t/a.png\") t)", "T");
    says(&shared, &lisp, "img-upper", "(and (%mathsync-image-p #p\"/t/a.PNG\") t)", "T");
    says(&shared, &lisp, "img-txt", "(and (%mathsync-image-p #p\"/t/a.txt\") t)", "NIL");
    // A directory has no `pathname-name`, which is how `done/` drops out without
    // this predicate ever having to know its name.
    says(&shared, &lisp, "img-dir", "(and (%mathsync-image-p #p\"/t/done/\") t)", "NIL");

    // --- the queue ----------------------------------------------------------
    let dir = std::env::temp_dir().join(format!("zemacs_mathsync_test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("done")).unwrap();
    // Written oldest-first, with the mtimes forced apart so the sort has
    // something unambiguous to order by.
    for (i, name) in ["first.png", "second.jpg"].iter().enumerate() {
        std::fs::write(dir.join(name), b"not really an image").unwrap();
        let t = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + i as u64 * 60);
        filetime_set(&dir.join(name), t);
    }
    // Two things that must not be offered: a non-image, and one already done.
    std::fs::write(dir.join("notes.txt"), b"prose").unwrap();
    std::fs::write(dir.join("done").join("old.png"), b"done already").unwrap();

    lisp.eval(format!("(setf *mathsync-dir* {:?})", dir.display().to_string()));
    says(&shared, &lisp, "queue-count", "(length (%mathsync-images))", "2");
    // Oldest first, so a backlog is read in the order it was photographed.
    says(&shared, &lisp, "queue-order",
         "(format nil \"~{~a~^,~}\" (mapcar #'file-namestring (%mathsync-images)))",
         "first.png,second.jpg");

    // --- the modeline note --------------------------------------------------
    //
    // Empty is the ordinary state and drops the whole `%N` segment.
    // Read off the editor rather than back out of Lisp: the note's whole purpose
    // is to be a field the renderer draws, so that field is the thing to assert.
    let note_is = |want: &str| {
        wait(&shared, &format!("the modeline note to be {want:?}"), |ed| {
            (ed.modeline_note == want).then_some(())
        })
    };
    lisp.eval("(setf *mathsync-waiting* 3 *mathsync-ready* nil *mathsync-job* nil)".into());
    lisp.eval("(%mathsync-note)".into());
    note_is("MathSync 3");
    // Empty is the ordinary state, and an empty `%N` drops its whole segment —
    // so a quiet watcher costs no room on the strip.
    lisp.eval("(setf *mathsync-waiting* 0)(%mathsync-note)".into());
    note_is("");

    // --- the handoff --------------------------------------------------------
    //
    // Exactly what the child leaves behind: its stdout in the output file, and a
    // job plist naming the image it was reading.
    std::fs::write("/tmp/zemacs-mathsync.out", "\\[ x^2 + y^2 = z^2 \\]\n").unwrap();
    let _ = std::fs::remove_file("/tmp/zemacs-mathsync.err");
    lisp.eval(format!(
        "(setf *mathsync-job* (list :process nil :image #p{:?}))",
        dir.join("first.png").display().to_string()
    ));
    lisp.eval("(%mathsync-finish)".into());

    // The transcription is in the register `p` pastes from...
    says(&shared, &lisp, "register", "(car (register))", "\\[ x^2 + y^2 = z^2 \\]");
    // ...the job is over, and the tick is up.
    says(&shared, &lisp, "job-clear", "*mathsync-job*", "NIL");
    says(&shared, &lisp, "ready", "(and *mathsync-ready* t)", "T");
    // ...and the image is gone, so the next scan does not offer it again and
    // spend a second API call on it. Deleted rather than archived: the queue is
    // the folder, the picture has done its job once the LaTeX is in the
    // register, and a `done/` that grows for ever is a second folder to clean
    // out by hand.
    assert!(
        !dir.join("first.png").exists(),
        "a transcribed image must leave the queue"
    );
    assert!(
        !dir.join("done").join("first.png").exists(),
        "and must not be archived unless asked"
    );
    says(&shared, &lisp, "queue-after", "(length (%mathsync-images))", "1");

    // --- ...unless you asked to keep it -------------------------------------
    //
    // The photograph is the only copy of what the model was reading, so the old
    // behaviour is still one variable away.
    std::fs::write(dir.join("third.png"), b"x").unwrap();
    std::fs::write("/tmp/zemacs-mathsync.out", "\\( k \\)\n").unwrap();
    lisp.eval("(setf *mathsync-keep* t)".into());
    lisp.eval(format!(
        "(setf *mathsync-job* (list :process nil :image #p{:?}))",
        dir.join("third.png").display().to_string()
    ));
    lisp.eval("(%mathsync-finish)".into());
    says(&shared, &lisp, "kept-register", "(car (register))", "\\( k \\)");
    assert!(
        dir.join("done").join("third.png").exists(),
        "with *mathsync-keep* on, a transcribed image moves into done/"
    );
    assert!(!dir.join("third.png").exists(), "and still leaves the queue");
    lisp.eval("(setf *mathsync-keep* nil)".into());

    // --- and a failure says why ---------------------------------------------
    //
    // The script writes one line to stderr for every failure it has, and a 404 on
    // the model slug is the one a config typo produces.
    std::fs::write("/tmp/zemacs-mathsync.out", "").unwrap();
    std::fs::write(
        "/tmp/zemacs-mathsync.err",
        "openrouter HTTP 404 (google/nope): no such model\n",
    )
    .unwrap();
    lisp.eval(format!(
        "(setf *mathsync-job* (list :process nil :image #p{:?}))",
        dir.join("second.jpg").display().to_string()
    ));
    lisp.eval("(%mathsync-finish)".into());
    wait_message(&shared, "the failure to be reported", |m| {
        m == "mathsync: openrouter HTTP 404 (google/nope): no such model"
    });
    // A failed image stays put, so pressing the key again retries it.
    assert!(
        dir.join("second.jpg").exists(),
        "a failed image must stay in the queue to be retried"
    );

    // --- a job finishes wherever you are standing ---------------------------
    //
    // `SPC n m' is bound in every buffer, so a transcription is routinely
    // started in the curriculum and waited for in the `.py' tangled out of it.
    // The poll used to be gated whole on the mode, which is on in org buffers
    // only — so the child exited, nothing noticed, and `*mathsync-job*' stayed
    // set for the rest of the session with every later `SPC n m' answering
    // "still reading" about a process that was long gone.
    //
    // A NIL process handle is what `%mathsync-finish` is given above and is
    // treated as "no longer askable", which is the same branch a real exited
    // child takes.
    std::fs::write("/tmp/zemacs-mathsync.out", "\\( a \\)\n").unwrap();
    let _ = std::fs::remove_file("/tmp/zemacs-mathsync.err");
    lisp.eval("(fundamental-mode)".into());
    says(&shared, &lisp, "not-org", "(and (minor-mode-p 'mathsync) t)", "NIL");
    lisp.eval(format!(
        "(setf *mathsync-job* (list :process nil :image #p{:?}))",
        dir.join("second.jpg").display().to_string()
    ));
    lisp.eval("(mathsync-poll)".into());
    says(&shared, &lisp, "job-off-mode", "*mathsync-job*", "NIL");
    says(&shared, &lisp, "register-off-mode", "(car (register))", "\\( a \\)");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Set a file's mtime without pulling in a crate for it.
fn filetime_set(path: &Path, when: std::time::SystemTime) {
    let secs = when
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // `touch -t` wants a local-time stamp; `-d @epoch` is not portable to BSD
    // touch, so this uses the one spelling macOS and Linux agree on.
    let stamp = std::process::Command::new("touch")
        .arg("-t")
        .arg(epoch_to_touch(secs))
        .arg(path)
        .status();
    assert!(stamp.map(|s| s.success()).unwrap_or(false), "touch must work");
}

/// `YYYYMMDDhhmm` in UTC, which is what BSD and GNU `touch -t` both accept.
fn epoch_to_touch(secs: u64) -> String {
    let days = secs / 86_400;
    let rest = secs % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}{m:02}{d:02}{:02}{:02}", rest / 3600, (rest % 3600) / 60)
}

/// Howard Hinnant's `civil_from_days`, which is the standard way to do this
/// without a date crate.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
