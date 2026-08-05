//! The buffer boundary: `utf8-text` in `runtime/modes/modes.lisp`, and the two
//! modes that were still crossing it without one.
//!
//! Lisp holds buffer text as UTF-8 **bytes**, one Lisp character per byte, while
//! everything going the other way — `message`, an overlay's `display`, a scene's
//! `run` — is encoded as UTF-8 per *character* on its way out. Text taken out of
//! a buffer and handed straight back is therefore encoded twice, and `café`
//! comes home as `cafÃ©`.
//!
//! **Every assertion here is byte-exact against a known sample, and that is the
//! whole point of the file.** A test asserting "some character in the answer is
//! non-ASCII" passes on double-encoded text — `Ã©` is two non-ASCII characters —
//! so it would have watched this bug ship. What is compared below is a Rust
//! `String` that came out of the image against a Rust literal in this file, and
//! the *undecoded* answer is asserted too: it must equal the mojibake computed
//! byte for byte from the sample, which is what says the decode is doing work
//! rather than the sample being ASCII by accident.
//!
//! Nothing here is spelled as a non-ASCII literal inside an `eval`ed form. A
//! form evaluated from Rust arrives as source in the same byte model, so `"—"`
//! written into one is already three characters and a test using it would be
//! asserting against mojibake it introduced itself. Non-ASCII goes in through
//! the *buffer*, which is where it comes from in real life, and comes back out
//! through `message`, which encodes properly.
//!
//! Deliberately a single `#[test]`, as in every file beside it: `cl_boot`
//! initialises a process-wide Lisp image, so there is exactly one `spawn` per
//! test binary and a new file is the only way to add one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Mode, Shared};

const PATIENCE: Duration = Duration::from_secs(20);
static NTH: AtomicUsize = AtomicUsize::new(0);

fn wait<T>(shared: &Shared, what: &str, f: impl Fn(&Editor) -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(v) = f(&shared.lock().unwrap()) {
            return v;
        }
        if Instant::now() >= deadline {
            let ed = shared.lock().unwrap();
            let seen = &ed.messages[ed.messages.len().saturating_sub(8)..];
            panic!("timed out waiting for {what}; last messages {seen:#?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_message(shared: &Shared, what: &str, pred: impl Fn(&str) -> bool) {
    wait(shared, what, |ed| {
        ed.messages.iter().any(|m| pred(m)).then_some(())
    });
}

/// Evaluate `form` and answer its value, printed with `~a`, as the editor
/// received it.
///
/// Numbered, because the log is cumulative and two questions about the same
/// sample have the same answer: without the counter the second wait would be
/// satisfied by the first one's message and prove nothing.
fn asks(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str) -> String {
    let tag = NTH.fetch_add(1, Ordering::Relaxed);
    let head = format!("#{tag} ");
    lisp.eval(format!("(message (format nil \"#{tag} ~a\" {form}))"));
    wait(shared, form, |ed| {
        ed.messages
            .iter()
            .find(|m| m.starts_with(&head))
            .map(|m| m[head.len()..].to_string())
    })
}

fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    let got = asks(shared, lisp, form);
    assert_eq!(got, want, "{form}");
}

/// The mojibake `sample` becomes when it is encoded a second time: every byte of
/// its UTF-8 read as a character in its own right.
///
/// Computed rather than written out, so the expectation cannot drift from the
/// sample and cannot be quietly typo'd into something that matches.
fn twice_encoded(sample: &str) -> String {
    sample.bytes().map(|b| b as char).collect()
}

fn runtime(file: &str) -> String {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime").join(file))
        .unwrap_or_else(|e| panic!("runtime/{file} must exist: {e}"))
        .display()
        .to_string()
}

/// Put `text` in the live buffer, cursor at 0.
///
/// A fresh path every time, for the reason `org_modern.rs` gives: `Editor::load`
/// switches to a buffer already holding that path rather than reloading it.
fn load(shared: &Shared, text: &str, nth: usize) {
    let mut ed = shared.lock().unwrap();
    ed.load(
        text,
        Some(PathBuf::from(format!("/tmp/zemacs_test_utf8_{nth}.org"))),
        Some("org".into()),
    );
    ed.apply(EditorCommand::SetMode(Mode::Normal));
    ed.apply(EditorCommand::MoveTo(0));
}

/// The display strings of the substituting overlays, in the renderer's order.
fn displays(ed: &Editor) -> Vec<String> {
    ed.buffer
        .overlays()
        .iter()
        .filter_map(|o| o.display.clone())
        .collect()
}

/// One of each sequence length UTF-8 has, so a decoder that got the leading-byte
/// arithmetic right for two bytes and wrong for four cannot pass.
///
/// `é` is two bytes, `—` three, `𝔽` four, and the ASCII around them is what
/// proves the fast path did not eat anything.
const SAMPLE: &str = "cafe\u{301} \u{2014} \u{e9}tude \u{1d53d} end";

/// A curriculum whose every readable string is non-ASCII: the `#+TITLE:` that
/// `math-progress` puts in the status line, a heading that becomes `:title`, and
/// a property value that becomes part of a problem plist.
const CURRICULUM: &str = "\
#+ZEMACS_CURRICULUM: 1
#+TITLE: \u{c1}lgebra \u{2014} Vectores

* Unidad \u{2014} n\u{fa}meros
  :PROPERTIES:
  :ID: unit-1
  :ZEMACS_NOTE: caf\u{e9}
  :END:

** Problema
   :PROPERTIES:
   :ZEMACS_PROBLEM: written
   :END:
";

/// Org markup whose *drawn* text is non-ASCII on both paths that hand text back:
/// a link's description and an emphasis run's body.
const MARKUP: &str = "\
* T\u{ed}tulo
Some *caf\u{e9}* text and a [[https://x/a/b][d\u{e9}j\u{e0} vu]].
";

#[test]
fn buffer_text_handed_back_to_the_editor_is_decoded_first() {
    // `modes.lisp` alone first, and the message in between is the assertion that
    // the decoder is *where every mode can reach it*. Half the tests in this
    // directory load a mode file against a bare image and never read a config,
    // so "it works under a full `init.lisp`" — which `org_frozen.rs` proves —
    // is not the same statement.
    let init = std::env::temp_dir().join("zemacs_test_utf8_init.lisp");
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(message (format nil "decoder: ~a" (if (fboundp 'utf8-text) "yes" "no")))
(load {:?} :verbose nil :print nil)
(load {:?} :verbose nil :print nil)
(message "utf8 test init loaded")
"#,
            runtime("modes/modes.lisp"),
            runtime("modes/org-modern.lisp"),
            runtime("modes/math.lisp"),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init.clone());

    wait_message(&shared, "the runtime files to load", |m| {
        m == "utf8 test init loaded"
    });
    {
        let seen = shared.lock().unwrap().messages.clone();
        assert!(
            seen.iter().any(|m| m == "decoder: yes"),
            "`utf8-text' must come with `modes.lisp', before any mode file: {seen:#?}"
        );
        assert!(
            !seen.iter().any(|m| m.contains("error")),
            "the runtime files must load cleanly; got {seen:#?}"
        );
    }

    // --- the decoder itself, byte for byte ----------------------------------
    load(&shared, SAMPLE, 0);
    let decoded = asks(&shared, &lisp, "(utf8-text (line-string 1))");
    assert_eq!(decoded, SAMPLE, "the line comes back saying what the file says");

    // ...and the same line without the decode, which is the failure this exists
    // to name. Asserted exactly, not merely as "different": a test happy with
    // any difference would also be happy with a decoder that dropped the
    // accented characters instead of double-encoding them.
    let raw = asks(&shared, &lisp, "(line-string 1)");
    assert_eq!(
        raw,
        twice_encoded(SAMPLE),
        "undecoded buffer text is its own UTF-8 read one byte at a time"
    );
    assert_ne!(raw, decoded, "the sample must not be ASCII by accident");

    // Lengths, which is the half of the bug that is not about glyphs: a scene
    // measures every string in a real font, so bytes are three characters wide
    // where the document has one and every wrap is computed from that.
    says(
        &shared,
        &lisp,
        "(length (utf8-text (line-string 1)))",
        &SAMPLE.chars().count().to_string(),
    );
    says(
        &shared,
        &lisp,
        "(length (line-string 1))",
        &SAMPLE.len().to_string(),
    );

    // ASCII is the identity — the same object, not a copy. The fast path is what
    // makes it affordable to call this on every line of a document.
    says(&shared, &lisp, r#"(let ((s "abc")) (if (eq s (utf8-text s)) t nil))"#, "T");
    // A truncated or stray sequence is passed through one character at a time
    // rather than dropped or replaced: a renderer showing nothing is a page that
    // silently disagrees with the file behind it.
    says(&shared, &lisp, "(length (utf8-text (string (code-char 200))))", "1");
    says(&shared, &lisp, "(length (utf8-text (string (code-char 128))))", "1");

    // --- org-modern: what a substitution draws ------------------------------
    //
    // Both paths that hand buffer text back as a `display` string. Before the
    // decode these came out as `cafÃ©` and `dÃ©jÃ  vu`, which is the accented
    // link description reported from a real document.
    load(&shared, MARKUP, 1);
    lisp.eval("(progn (goto-char (point-max)) (org-mode-hook))".into());
    says(&shared, &lisp, "(if (minor-mode-p 'org-modern) t nil)", "T");
    let drawn = wait(&shared, "the substitutions", |ed| {
        let d = displays(ed);
        (d.len() == 3).then_some(d)
    });
    assert_eq!(
        drawn,
        vec!["\u{25cf}", "caf\u{e9}", "d\u{e9}j\u{e0} vu"],
        "a bullet, an emphasis run's body, and a link's description"
    );

    // --- math: what a reader puts in a plist --------------------------------
    load(&shared, CURRICULUM, 2);
    // `#+TITLE:` straight into the status line, which is `math-progress` whole.
    // Watched as a message rather than asked for as a value: the message *is*
    // the thing under test, and reading the log back through `messages` would
    // put a second encode between the bug and the assertion.
    lisp.eval("(math-progress)".into());
    let progress = "\u{c1}lgebra \u{2014} Vectores \u{2014} 0/1 problem done";
    wait_message(&shared, progress, |m| m == progress);
    // A heading's text, which is what `math-units' hands to anything drawing a
    // curriculum.
    says(
        &shared,
        &lisp,
        "(getf (first (math-units)) :title)",
        "Unidad \u{2014} n\u{fa}meros",
    );
    // ...and a property *value*, which is free text however ASCII the schema's
    // own properties happen to be.
    says(
        &shared,
        &lisp,
        "(%math-prop (first (%math-headings)) \"ZEMACS_NOTE\")",
        "caf\u{e9}",
    );
    // The offsets are unchanged by any of it: they are characters and always
    // were, and a decode that had been applied a layer too low would have moved
    // them. `:begin` is the first star of the unit heading, which is the line
    // after a two-line preamble and a blank.
    let unit_begin: usize = CURRICULUM
        .find("* Unidad")
        .map(|b| CURRICULUM[..b].chars().count())
        .expect("the sample has a unit heading");
    says(
        &shared,
        &lisp,
        "(getf (first (math-units)) :begin)",
        &unit_begin.to_string(),
    );

    let _ = std::fs::remove_file(&init);
}
