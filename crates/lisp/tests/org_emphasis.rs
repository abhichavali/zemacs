//! `C-b`/`C-i` while typing, and what a click on the modeline does.
//!
//! Two features that have nothing to do with each other and one thing in
//! common: both are the *shipped config's*, so neither can be tested against a
//! synthetic init. `C-b` is bound in the Insert state by `runtime/init.lisp`
//! and the command it names checks the major mode for itself; `%modeline-click`
//! dispatches on the templates `runtime/library.lisp` built the strip out of. A
//! test with three hand-written `define-key` calls would pass with the feature
//! dead in the editor, which is the mistake `which_key.rs` documents.
//!
//! Deliberately a single `#[test]`, like every file beside it: `cl_boot`
//! initialises a process-wide image, so there is one `spawn` per test binary.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Key, Mode, Shared};

const PATIENCE: Duration = Duration::from_secs(20);

fn wait<T>(shared: &Shared, what: &str, f: impl Fn(&Editor) -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(v) = f(&shared.lock().unwrap()) {
            return v;
        }
        if Instant::now() >= deadline {
            let ed = shared.lock().unwrap();
            let seen = &ed.messages[ed.messages.len().saturating_sub(8)..];
            panic!("timed out waiting for {what}; status={:?} last={seen:#?}", ed.status);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Press `key` the way the application does — see `which_key.rs`, which is
/// where this spelling comes from.
fn press(shared: &Shared, lisp: &zemacs_lisp::Lisp, key: Key) {
    let cmds = shared.lock().unwrap().handle_key(key);
    for cmd in cmds {
        match cmd {
            EditorCommand::CallLisp(form) => lisp.eval(form),
            other => {
                shared.lock().unwrap().apply(other);
            }
        }
    }
}

/// Wait for the buffer's whole text to be `want`. The commands a Lisp binding
/// produces come back through the queue, so every assertion here is a wait.
fn text_is(shared: &Shared, what: &str, want: &str) {
    wait(shared, what, |ed| (ed.buffer.text.to_string() == want).then_some(()));
}

fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, tag: &str, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"{tag}=~a\" {form}))"));
    let want = format!("{tag}={want}");
    wait(shared, &format!("{form} => {want}"), |ed| {
        ed.messages.iter().any(|m| *m == want).then_some(())
    });
}

#[test]
fn org_emphasis_chords_and_a_click_on_the_strip() {
    let init = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp"),
    )
    .expect("runtime/init.lisp must exist");

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    wait(&shared, "init.lisp to finish loading", |ed| {
        ed.messages.iter().any(|m| m.contains("is driving the editor")).then_some(())
    });
    // The bindings are `%do` commands and reach the editor asynchronously.
    wait(&shared, "the Insert-state bindings", |ed| {
        ed.keymap.contains_key(&(Mode::Insert, "C-b".into())).then_some(())
    });

    // --- `C-b` with nothing selected opens an empty pair --------------------
    {
        let mut ed = shared.lock().unwrap();
        ed.load("", Some(PathBuf::from("/tmp/zemacs_org_emphasis.org")), None);
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::InsertText("hello ".into()));
    }
    lisp.eval("(org-mode)".into());
    wait(&shared, "org-mode", |ed| (ed.buffer.major_mode == "org-mode").then_some(()));

    press(&shared, &lisp, Key::Ctrl('b'));
    text_is(&shared, "C-b to open a bold pair", "hello **");
    // ...and point sits *between* the two stars, which is the whole point of
    // pressing it while typing rather than after selecting.
    says(&shared, &lisp, "bold-point", "(point)", "7");
    // So the next thing typed lands inside them.
    press(&shared, &lisp, Key::Char('x'));
    text_is(&shared, "the character to land inside the pair", "hello *x*");

    // ...and the same for italics, from the end of what is there.
    lisp.eval("(goto-char (point-max))".into());
    says(&shared, &lisp, "at-end", "(point)", "9");
    press(&shared, &lisp, Key::Ctrl('i'));
    text_is(&shared, "C-i to open an italic pair", "hello *x*//");
    says(&shared, &lisp, "italic-point", "(point)", "10");

    // --- what the markers go around, and what they come back off ------------
    //
    // Everything below is `%org-emphasis-span` and the toggle in
    // `%org-emphasize`, which both spellings of the command now share. The
    // buffer is set from Lisp rather than reloaded so that it stays the org
    // buffer the commands check for; the newline is spelled out because a `\n`
    // in a Common Lisp string is the letter `n`.
    let two_lines = r#"(concatenate 'string "  alpha beta  " (string #\Newline) "second")"#;
    lisp.eval(format!("(replace-region 0 (point-max) {two_lines})"));
    text_is(&shared, "the two-line document", "  alpha beta  \nsecond");

    // `V` selects to the newline and carries the line's indent, and org renders
    // a marker with either against it as a literal asterisk. So the span is the
    // line's *text*: without the trim the closing `*` landed on the next line.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.cursor = 0;
        ed.apply(EditorCommand::SetMode(Mode::VisualLine));
    }
    lisp.eval("(org-bold)".into());
    text_is(&shared, "V then bold to wrap the text and not the newline", "  *alpha beta*  \nsecond");
    // ...and it is over, so the stale selection goes: `d` next would otherwise
    // delete a run that has moved two characters.
    says(&shared, &lisp, "state-after-bold", "(evil-state)", "normal");

    // Bold on text that is already bold takes it off. The markers are outside
    // what is selected here — which is what selecting the word between them is
    // — and `**alpha beta**` is what this used to answer.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.cursor = 3;
        ed.apply(EditorCommand::SetMode(Mode::Visual));
        ed.buffer.cursor = 12;
    }
    lisp.eval("(org-bold)".into());
    text_is(&shared, "bold on bold to unbold", "  alpha beta  \nsecond");

    // With nothing selected at all — which is every `SPC m b`, since the leader
    // is pressed in Normal — the word point is in.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.cursor = 9; // the `e` of `beta`
    }
    lisp.eval("(org-bold)".into());
    text_is(&shared, "bold with no selection to take the word", "  alpha *beta*  \nsecond");

    // The other half of the toggle: the markers inside the selection.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.cursor = 8;
        ed.apply(EditorCommand::SetMode(Mode::Visual));
        ed.buffer.cursor = 13;
    }
    lisp.eval("(org-bold)".into());
    text_is(&shared, "selecting the markers too to unbold", "  alpha beta  \nsecond");

    // A selection that crosses a line break gets its first line. Emphasis is a
    // within-line construct, so the alternative is markup that renders nowhere.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.cursor = 2;
        ed.apply(EditorCommand::SetMode(Mode::Visual));
        ed.buffer.cursor = 18; // into `second`
    }
    lisp.eval("(org-bold)".into());
    text_is(&shared, "a two-line selection to bold one line", "  *alpha beta*  \nsecond");

    // `C-b`'s spelling shares all of it — the two used to be separate copies of
    // "wrap the region", and only one of them can be the one that is right.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.cursor = 3;
        ed.apply(EditorCommand::SetMode(Mode::Visual));
        ed.buffer.cursor = 12;
    }
    lisp.eval("(org-emphasis-bold)".into());
    text_is(&shared, "the typing chord to unbold as well", "  alpha beta  \nsecond");

    // Standing in the whitespace there is no word, and the answer is a message
    // rather than a stray pair of asterisks in the margin.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.cursor = 13; // the second trailing space
    }
    lisp.eval("(org-italic)".into());
    wait(&shared, "bold in the margin to decline", |ed| {
        ed.messages.iter().any(|m| m == "nothing to emphasise").then_some(())
    });
    assert_eq!(shared.lock().unwrap().buffer.text.to_string(), "  alpha beta  \nsecond");

    // Offsets are characters on both sides of the shim, and the span is built
    // out of them: with bytes anywhere in it the markers land inside the `ö`.
    lisp.eval(r#"(replace-region 0 (point-max) "héllo wörld")"#.into());
    text_is(&shared, "the accented document", "héllo wörld");
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer.cursor = 7; // the `ö`
    }
    lisp.eval("(org-code)".into());
    text_is(&shared, "the markers to land on character boundaries", "héllo ~wörld~");

    // --- ...and it is org's key, not the editor's ---------------------------
    //
    // A state binding is global, so the guard is in the command. In a `.rs`
    // file `C-b` must leave the buffer alone — which is exactly what an unbound
    // Ctrl chord in Insert already did, so this costs nothing anywhere else.
    {
        let mut ed = shared.lock().unwrap();
        ed.load("fn main() {}", Some(PathBuf::from("/tmp/zemacs_org_emphasis.rs")), Some("rust".into()));
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.buffer.cursor = 2;
    }
    lisp.eval("(rust-mode)".into());
    wait(&shared, "rust-mode", |ed| (ed.buffer.major_mode == "rust-mode").then_some(()));
    press(&shared, &lisp, Key::Ctrl('b'));
    // Nothing to wait for, so the assertion has to be that nothing arrives: one
    // round trip through the image, then look.
    says(&shared, &lisp, "rust-quiet", "(and (derived-mode-p 'org-mode) t)", "NIL");
    assert_eq!(shared.lock().unwrap().buffer.text.to_string(), "fn main() {}");

    // --- a click on the strip ----------------------------------------------
    //
    // The renderer answers a template and the text it expanded to; this is the
    // half that decides what that means. Every template the shipped strip is
    // built from, so a segment added to `default-modeline` without an arm here
    // shows up as a click that does nothing.
    says(&shared, &lisp, "code-plus", "(%modeline-code \" %+\")", "+");
    says(&shared, &lisp, "code-pos", "(%modeline-code \"%l:%c\")", "l");
    says(&shared, &lisp, "code-none", "(%modeline-code \"  \")", "NIL");
    // `%%` is a literal per cent and must not be read as a code — a segment
    // spelled `100%%` has nothing clickable in it.
    says(&shared, &lisp, "code-escape", "(%modeline-code \"100%%\")", "NIL");

    // The mode segment says which state you are in, and the position segment
    // spells out what `12:4` is 12 of. Neither touches the buffer.
    lisp.eval(r#"(%modeline-click "%M  " "Rust")"#.into());
    wait(&shared, "the mode segment to answer", |ed| {
        ed.messages.iter().any(|m| m.starts_with("rust-mode")).then_some(())
    });
    lisp.eval(r#"(%modeline-click "%l:%c" "1:3")"#.into());
    wait(&shared, "the position segment to answer", |ed| {
        ed.messages.iter().any(|m| m.starts_with("line 1 of ")).then_some(())
    });

    // The dot is the one arm that *does* something, and with nothing to save it
    // says so rather than writing the file. The buffer above was loaded and
    // never typed into, so this is the unmodified case by construction — which
    // is the branch worth pinning, since the other one writes to the disk.
    lisp.eval(r#"(%modeline-click " %+" "")"#.into());
    wait(&shared, "the dot to decline", |ed| {
        ed.messages.iter().any(|m| m == "nothing to save").then_some(())
    });
}
