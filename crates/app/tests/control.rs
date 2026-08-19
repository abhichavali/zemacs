//! `--control`, end to end: the binary, the real `init.lisp`, a pipe.
//!
//! Deliberately the whole process rather than a unit test of the op table.
//! Everything this mode is for lives in the seams — that the image loads before
//! `ready` is announced, that a Lisp value comes back as a `message` event and
//! not as a return, that stdout carries protocol frames and nothing else — and
//! a test that called the op function directly would check none of them. It is
//! also the check that fails if anyone puts a `println!` on the startup path.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// Booting ECL and loading the shipped config takes a second or two on a warm
/// machine and rather longer on a cold CI box.
const PATIENCE: Duration = Duration::from_secs(60);

/// The editor under test, killed on drop so a failed assertion cannot leave a
/// headless editor running.
struct Zemacs {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Drop for Zemacs {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Zemacs {
    fn start() -> Zemacs {
        let mut child = Command::new(env!("CARGO_BIN_EXE_zemacs"))
            .arg("--control")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited, so that a failure shows the editor's own complaint —
            // which is the whole point of stderr being where everything that is
            // not protocol goes.
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the zemacs binary is built by `cargo test`");
        let stdin = child.stdin.take().expect("piped");
        let stdout = BufReader::new(child.stdout.take().expect("piped"));
        Zemacs {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("the editor is still reading stdin");
        self.stdin.flush().expect("flush");
    }

    /// Read frames until `f` answers. Every frame is JSON, and a frame that is
    /// not is the failure this mode exists to make impossible.
    fn until<T>(&mut self, what: &str, mut f: impl FnMut(&serde_json::Value) -> Option<T>) -> T {
        let deadline = Instant::now() + PATIENCE;
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).expect("read stdout");
            assert!(n > 0, "the editor closed stdout while waiting for {what}");
            let frame: serde_json::Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdout carried something that is not a frame: {line:?} ({e})"));
            if let Some(v) = f(&frame) {
                return v;
            }
        }
    }
}

#[test]
fn control_mode_answers_over_a_pipe() {
    let mut z = Zemacs::start();

    // Nothing may be sent before this. `ready` is the promise that `init.lisp`
    // has finished loading, and a client that jumps the gun gets whichever
    // half of its config happened to be up.
    let init = z.until("ready", |f| {
        (f["event"] == "ready").then(|| f["init"].as_str().unwrap_or_default().to_string())
    });
    assert!(init.ends_with("init.lisp"), "ready named {init:?} as the init file");

    // The round trip the whole mode is for: a form goes in, and its effect
    // comes back out as an event rather than as a reply.
    z.send(r#"{"id":1,"op":"eval","form":"(message \"pong\")"}"#);
    z.until("the eval reply", |f| (f["id"] == 1).then_some(()));
    z.until("pong as an event", |f| {
        (f["event"] == "message" && f["text"] == "pong").then_some(())
    });

    // And the same fact, from the log rather than from the stream — because a
    // client that connected late, or that missed an event, has to be able to
    // ask.
    z.send(r#"{"id":2,"op":"messages","since":0}"#);
    let logged = z.until("the messages reply", |f| {
        (f["id"] == 2).then(|| f["result"]["messages"].as_array().cloned().unwrap_or_default())
    });
    assert!(
        logged.iter().any(|m| m == "pong"),
        "`pong` is not in the message log: {logged:?}"
    );

    z.send(r#"{"id":3,"op":"quit"}"#);
    z.until("the exit event", |f| (f["event"] == "exit").then_some(()));
    let status = z.child.wait().expect("wait");
    assert!(status.success(), "the editor exited {status}");
}

/// org's structure editing, in the real editor with a real buffer.
///
/// Here rather than in `crates/lisp/tests/org_lists.rs` because that harness
/// gets a bare `Editor` — its live buffer is the generated dashboard, which
/// refuses every edit — so it can only exercise the *pure* readers. What the
/// commands do to a buffer needs a buffer, and this mode is how you get one.
///
/// The two claims that no amount of testing the readers would have caught:
///
///   - A headline's checkbox and its TODO keyword are one state. Ticking the
///     box moves the keyword and cycling the keyword moves the box, so the two
///     can never be seen disagreeing.
///   - **Point stays where it was.** `replace-region` sets the cursor to the
///     start of what it replaced, and recounting a statistics cookie rewrites
///     an *ancestor's* line — so before this was guarded, `C-c C-c` on a
///     checkbox left the cursor up on the parent heading and the next key you
///     pressed edited that. Nothing in the resulting text records it.
#[test]
fn org_structure_edits_an_outline() {
    let dir = std::env::temp_dir().join(format!("zemacs_org_structure-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let path = dir.join("plan.org");
    std::fs::write(&path, "* Release [/]\n** TODO [ ] docs\n** [ ] ship\n").expect("write");

    let mut z = Zemacs::start();
    z.until("ready", |f| (f["event"] == "ready").then_some(()));

    let mut id = 0;
    let mut req = |z: &mut Zemacs, op: &str, args: &str| {
        id += 1;
        z.send(&format!(r#"{{"id":{id},"op":"{op}"{args}}}"#));
        z.until("a reply", |f| (f["id"] == id).then(|| f["result"].clone()))
    };

    /// Wait for the buffer to read as WANT, then assert it.
    ///
    /// `keys` answers as soon as the keystrokes are *fed*. The org commands they
    /// trigger — the TODO cycle, the checkbox, the cookie recount — run on the
    /// Lisp thread and land a queue turn later, so reading `text` on the very
    /// next request reads the buffer before the command has touched it. That is
    /// the same asynchrony the `find-file` below already waits out, and for the
    /// same reason it is waited out the same way: paced at 20ms rather than
    /// spun, so the poll does not compete with the work it is waiting for.
    ///
    /// Asserting straight after the press passed alone and failed about one run
    /// in four under a loaded parallel suite, reporting the *untouched* fixture
    /// — which is the signature of this race rather than of a wrong command.
    macro_rules! text_becomes {
        ($want:expr) => {{
            let want = serde_json::json!($want);
            let deadline = Instant::now() + PATIENCE;
            loop {
                let got = req(&mut z, "text", "")["text"].clone();
                if got == want {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the buffer never became {want}; it is {got}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }};
    }

    req(
        &mut z,
        "eval",
        &format!(r#","form":"(find-file \"{}\")""#, path.display()),
    );
    // `eval` is asynchronous, so the file is open when the editor says it is.
    //
    // Paced rather than spun. Unpaced, this sends `state` as fast as the pipe
    // will carry it and competes with the very work it is waiting for — which
    // is how it came to time out twice under a loaded parallel run while
    // passing every time on its own. A poll that starves its subject is a test
    // that reports load as a failure.
    let deadline = Instant::now() + PATIENCE;
    while req(&mut z, "state", "")["major_mode"] != "org-mode" {
        assert!(Instant::now() < deadline, "the org file never opened");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Onto `** TODO [ ] docs`, into the middle of the word so a cursor that
    // moves is a cursor that shows up below.
    req(&mut z, "keys", r#","keys":"<esc> g g j w w""#);
    let before = req(&mut z, "state", "");
    let (line, col) = (before["line"].clone(), before["col"].clone());
    assert_eq!(line, 2, "the fixture puts the cursor on the second line");

    // Tick it: the box and the keyword move together, and the parent counts.
    req(&mut z, "keys", r#","keys":"C-c C-c""#);
    let after = req(&mut z, "state", "");
    assert_eq!(
        (after["line"].clone(), after["col"].clone()),
        (line, col),
        "C-c C-c moved the cursor; the cookie pass rewrites the parent's line",
    );
    text_becomes!("* Release [1/2]\n** DONE [X] docs\n** [ ] ship\n");

    // ...and back, through the *other* name for the same state. `C-c C-t` off
    // DONE is the end of the cycle, so the keyword goes and the box clears.
    req(&mut z, "keys", r#","keys":"C-c C-t""#);
    text_becomes!("* Release [0/2]\n** [ ] docs\n** [ ] ship\n");

    // A sibling of a task is a task, with an empty box and no keyword.
    req(&mut z, "keys", r#","keys":"<esc> M-<ret>""#);
    req(&mut z, "keys", r#","keys":"t e s t s <esc>""#);
    text_becomes!("* Release [0/3]\n** [ ] docs\n** [ ] tests\n** [ ] ship\n");

    // `M-S-<ret>` is the same key with a state on it: the box comes across
    // empty and the keyword comes from `%org-heading-with`, so the two cannot
    // arrive disagreeing — and the new box makes the parent's cookie wrong,
    // which is why the command recounts before you have typed anything.
    req(&mut z, "keys", r#","keys":"<esc> g g j M-S-<ret>""#);
    req(&mut z, "keys", r#","keys":"a p i <esc>""#);
    text_becomes!("* Release [0/4]\n** [ ] docs\n** TODO [ ] api\n** [ ] tests\n** [ ] ship\n");

    // `M-S-<left>`/`M-S-<right>` take the descendants with them, which is the
    // whole of what `M-<left>`/`M-<right>` deliberately do not do. Four lines
    // rewritten in one press, bottom-up — a top-down pass would aim its second
    // edit at offsets its first had already moved.
    req(&mut z, "keys", r#","keys":"<esc> g g""#);
    let before = req(&mut z, "state", "");
    req(&mut z, "keys", r#","keys":"M-S-<right>""#);
    text_becomes!("** Release [0/4]\n*** [ ] docs\n*** TODO [ ] api\n*** [ ] tests\n*** [ ] ship\n");
    let after = req(&mut z, "state", "");
    // The same *character*, which is one column further along: a star goes in at
    // the front of the line, so everything after it moves, and that is what
    // Emacs does too — `org-demote` inserts and every position past the insert
    // shifts with it. This used to assert the same *column*, which is the same
    // thing only until you are typing: point kept column 1 and landed back on
    // the second star instead of the space it was on, so `M-<right>' in the
    // middle of a heading put the next letter inside the last word.
    assert_eq!(after["line"], before["line"], "the demote changed the line");
    assert_eq!(
        after["col"].as_i64().unwrap(),
        before["col"].as_i64().unwrap() + 1,
        "the character under point did not survive the demote",
    );

    // ...and back, by the same delta for every heading — a descendant that
    // moved further than its root would have flattened the shape.
    req(&mut z, "keys", r#","keys":"M-S-<left>""#);
    let restored = "* Release [0/4]\n** [ ] docs\n** TODO [ ] api\n** [ ] tests\n** [ ] ship\n";
    text_becomes!(restored);
    // Level 1 is the floor, and hitting it is a no-op rather than a `*`-less
    // line that org would stop reading as a heading at all.
    req(&mut z, "keys", r#","keys":"M-S-<left>""#);
    assert_eq!(req(&mut z, "text", "")["text"], restored);

    z.send(&format!(r#"{{"id":{},"op":"quit"}}"#, id + 1));
    z.until("the exit event", |f| (f["event"] == "exit").then_some(()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hovering a diagnostic's mark, in the real editor with a real pointer.
///
/// Here rather than in `crates/lisp/tests/` for `which_key.rs`'s reason, which
/// bites harder in this feature than in any other: a chain is not tested by
/// testing links, and **half of this chain is arithmetic between a font and a
/// rectangle**. A headless `Editor` has no renderer, so it has no gutter width,
/// no row height and no pane — which is to say a test up there could assert that
/// the message reaches the overlay and could not assert a single thing about
/// hovering it. The links:
///
///   1. a diagnostic arrives and `%lsp-draw-diagnostics` hangs a mark on the
///      line, now carrying `help-echo` — the message travelling *on the overlay*
///      rather than being looked up when asked;
///   2. the property survives the trip through `shim.c` and `%do` and lands on
///      `Overlay::help_echo`, which nothing draws;
///   3. a pixel resolves to that gutter row — the renderer's own arithmetic,
///      through `gutter_row` and `click_target`;
///   4. `Editor::help_echo_at` finds the overlay from the *line* rather than the
///      character, which is what makes the answer agree with where the mark is;
///   5. and the box is retired exactly: by leaving the mark sideways, by leaving
///      it vertically, and by a keystroke — the failure mode this feature has.
///
/// Driven through the **seam** rather than through a language server: setting
/// `*lsp-diagnostics*` and calling `*lsp-diagnostics-functions*` is byte for byte
/// what `%lsp-publish-diagnostics` does with a server's reply, and it is the
/// documented extension point. `pylsp` proves the socket in a way this cannot and
/// is not installed everywhere. A `.rs` file, deliberately: no server is
/// registered for `rust-mode`, so nothing can race the injected diagnostics — and
/// `rust-mode` is not in `set-no-gutter-modes`, so there is a gutter to point at.
#[test]
fn hovering_a_diagnostic_mark_shows_what_is_wrong() {
    const MESSAGE: &str = "error: expected `;`, found `}` [probe]";

    let dir = std::env::temp_dir().join(format!("zemacs_hover-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let path = dir.join("hover.rs");
    std::fs::write(&path, "fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\n").expect("write");

    let mut z = Zemacs::start();
    z.until("ready", |f| (f["event"] == "ready").then_some(()));

    let mut id = 0;
    let mut req = |z: &mut Zemacs, op: &str, args: &str| {
        id += 1;
        z.send(&format!(r#"{{"id":{id},"op":"{op}"{args}}}"#));
        z.until("a reply", |f| (f["id"] == id).then(|| f["result"].clone()))
    };

    let file = path.display().to_string();
    req(&mut z, "eval", &format!(r#","form":"(find-file \"{file}\")""#));
    let deadline = Instant::now() + PATIENCE;
    while req(&mut z, "state", "")["path"] != serde_json::json!(file) {
        assert!(Instant::now() < deadline, "the file never opened");
        std::thread::sleep(Duration::from_millis(20));
    }

    // The seam, exactly as a server's reply reaches it. Line 2, which is neither
    // the first row nor the last — a mark on the first line would pass against a
    // hit test that had lost track of the pane's top inset.
    //
    // Escaped by `serde_json` rather than by hand: the form has quotes and
    // backticks in it and lands inside a JSON string, and a hand-escaped one
    // simply produces `bad JSON` and a reply this test then waits sixty seconds
    // for.
    let inject = format!(
        r#"(progn (setf (gethash "{file}" *lsp-diagnostics*)
                        (list (list 2 0 1 "expected `;`, found `}}`" "probe")))
                  (dolist (f *lsp-diagnostics-functions*) (funcall f "{file}"))
                  (message "marks drawn"))"#
    );
    req(
        &mut z,
        "eval",
        &format!(r#","form":{}"#, serde_json::Value::from(inject)),
    );
    z.until("the marks", |f| {
        (f["event"] == "message" && f["text"] == "marks drawn").then_some(())
    });

    // Down the gutter, a pixel at a time, in the *real* window this build laid
    // out — no font metric is hard-coded here, because hard-coding one would be
    // asserting the arithmetic under test against itself.
    let mut rows: Vec<(i64, Option<String>)> = Vec::new();
    for y in (0..300).step_by(2) {
        let r = req(&mut z, "mouse", &format!(r#","x":4,"y":{y}"#));
        rows.push((
            r["line"].as_i64().unwrap_or(0),
            r["tooltip"].as_str().map(str::to_owned),
        ));
    }
    // Every row of line 2's gutter says what is wrong...
    let on_two: Vec<_> = rows.iter().filter(|(l, _)| *l == 2).collect();
    assert!(!on_two.is_empty(), "the sweep never crossed line 2: {rows:?}");
    assert!(
        on_two.iter().all(|(_, t)| t.as_deref() == Some(MESSAGE)),
        "a row of the marked line's gutter said nothing: {on_two:?}",
    );
    // ...and no other row says anything at all. This is the assertion that the
    // whole-line lookup has not become a whole-*buffer* one.
    assert!(
        rows.iter().filter(|(l, _)| *l != 2).all(|(_, t)| t.is_none()),
        "an unmarked line's gutter answered: {rows:?}",
    );

    let y = rows
        .iter()
        .position(|(l, t)| *l == 2 && t.is_some())
        .expect("a marked row") as i32
        * 2;
    // Sideways off the mark, onto the code the mark is about. The gutter is the
    // hit test, so the box goes — see the note in `TODO.org` about hovering the
    // offending *span*, which is a different overlay and is not built.
    let off = req(&mut z, "mouse", &format!(r#","x":600,"y":{y}"#));
    assert!(off["tooltip"].is_null(), "the box survived leaving the gutter: {off}");
    let back = req(&mut z, "mouse", &format!(r#","x":4,"y":{y}"#));
    assert_eq!(back["tooltip"], serde_json::json!(MESSAGE), "the box did not come back");

    // A keystroke takes it down, and it is `handle_key` that does — the pointer
    // has not moved, so nothing on the mouse's path could have noticed.
    req(&mut z, "keys", r#","keys":"j""#);
    let screen = req(&mut z, "screen", "")["screen"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !screen.contains("tooltip:"),
        "a keystroke left the hover box up:\n{screen}",
    );
    // ...and the *same pixel* brings it back, which is the half of the cache in
    // `App::hover` that is easy to leave out: the cheap answer has not changed,
    // so only noticing that the box went takes it seriously.
    let again = req(&mut z, "mouse", &format!(r#","x":4,"y":{y}"#));
    assert_eq!(
        again["tooltip"],
        serde_json::json!(MESSAGE),
        "re-hovering the same pixel after a keystroke showed nothing",
    );

    z.send(&format!(r#"{{"id":{},"op":"quit"}}"#, id + 1));
    z.until("the exit event", |f| (f["event"] == "exit").then_some(()));
    let _ = std::fs::remove_dir_all(&dir);
}
