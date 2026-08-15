//! The text boundary: **Lisp holds characters**, on both sides of the shim.
//!
//! `λ` is one Lisp character whether it arrived in a `load`ed file, in a form
//! Rust queued for evaluation, or in the answer to `(buffer-string)`. UTF-8
//! exists only in the `char *` between the two languages, encoded by `dup_utf8`
//! and decoded by `utf8_string` in `crates/lisp/src/shim.c`, and nothing in
//! `runtime/` knows it is there.
//!
//! It was bytes until recently — every reader answered a base string of UTF-8
//! bytes, `%byte-index` and `%char-index` converted between the two counts, and
//! `utf8-text` in `modes.lisp` decoded at each of the dozen places text left a
//! buffer. This file asserted that model, byte for byte, and so pinned its one
//! genuine failure in place: ECL's reader parses a byte string as Latin-1, so
//! `(message "λ")` from a keybinding reached the status line as `Î»` while the
//! same form in a file was fine.
//!
//! **Every assertion here is byte-exact against a known sample, and that is the
//! whole point of the file.** A test asserting "some character in the answer is
//! non-ASCII" passes on double-encoded text — `Ã©` is two non-ASCII characters —
//! so it would have watched the bug ship. What is compared below is a Rust
//! `String` that came out of the image against a Rust literal in this file, and
//! the mojibake is computed from the sample and asserted *absent*.
//!
//! **The two halves have to move together, and the file is arranged to say so.**
//! A form arriving from Rust and text arriving from the editor are two different
//! doors, and decoding one alone makes the strings incomparable — which is what
//! the `search-forward` block in the middle is for. It looks for a literal that
//! came through one door in a buffer that came through the other, and it is the
//! assertion that fails on a half-done fix. Nothing else would: `search` simply
//! answers NIL, so the failure is silent everywhere except here.
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
/// sample and cannot be quietly typo'd into something that matches. It is what
/// every assertion below is asserted *against* — being right and being
/// `twice_encoded(SAMPLE)` are the two answers this boundary has ever given.
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

/// A buffer to look a Greek letter up in. `γ` is two bytes and one character,
/// and its offset is 4 either way you count the ASCII in front of it.
const GREEK: &str = "let \u{3b3} = 1\n";

#[test]
fn text_crossing_the_shim_is_characters_in_both_directions() {
    // `modes.lisp` alone first, then the two modes that read documents. Half the
    // tests in this directory load a mode file against a bare image and never
    // read a config, which is why this one does too.
    let init = std::env::temp_dir()
        .join(format!("zemacs_test_utf8_init-{}.lisp", std::process::id()));
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
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
            !seen.iter().any(|m| m.contains("error")),
            "the runtime files must load cleanly; got {seen:#?}"
        );
    }

    // --- door one: a form arriving from Rust --------------------------------
    //
    // The reproducer, and the smallest statement of the whole bug: this exact
    // form from a keybinding put `Î»` in the status line, because the source
    // crossed as a base string and ECL's reader read its two UTF-8 bytes as two
    // Latin-1 characters. Nothing is loaded and nothing is in a buffer — the
    // path is Rust, the shim, READ, and `message` straight back out.
    let lambda = asks(&shared, &lisp, "\"\u{3bb}\"");
    assert_eq!(lambda, "\u{3bb}", "a literal in an eval'd form is what it says");
    assert_ne!(
        lambda,
        twice_encoded("\u{3bb}"),
        "and specifically not the Latin-1 reading of its own encoding"
    );
    says(&shared, &lisp, "(length \"\u{3bb}\")", "1");
    says(&shared, &lisp, "(char-code (char \"\u{3bb}\" 0))", "955");
    // Four bytes as well as two, since the arithmetic differs per length.
    says(&shared, &lisp, "(char-code (char \"\u{1d53d}\" 0))", "120125");
    // The whole sample through the same door, byte-exact. Quoted by hand rather
    // than with `{:?}`: Rust's debug format spells a combining accent `\u{301}`,
    // which is not an escape Lisp's reader knows.
    let round_trip = asks(&shared, &lisp, &format!("\"{SAMPLE}\""));
    assert_eq!(round_trip, SAMPLE, "source in, the same source out");

    // --- door two: text the editor answered ---------------------------------
    load(&shared, SAMPLE, 0);
    let line = asks(&shared, &lisp, "(line-string 1)");
    assert_eq!(line, SAMPLE, "the line comes back saying what the file says");
    assert_ne!(
        line,
        twice_encoded(SAMPLE),
        "and not its own UTF-8 read one byte at a time, which is what it was"
    );
    assert_ne!(SAMPLE, twice_encoded(SAMPLE), "the sample is not ASCII by accident");

    // Lengths, which is the half of the bug that is not about glyphs: a scene
    // measures every string in a real font, so bytes are three characters wide
    // where the document has one and every wrap is computed from that.
    says(
        &shared,
        &lisp,
        "(length (line-string 1))",
        &SAMPLE.chars().count().to_string(),
    );
    // The editor's offsets and Lisp's indices are now the same integers, which
    // is the property that let `%byte-index` and `%char-index` be deleted. This
    // is exactly the assertion that fails if `f_query` stops decoding.
    says(&shared, &lisp, "(= (length (buffer-string)) (point-max))", "T");

    // --- the two doors meeting, which is the point of the whole change ------
    //
    // A pattern read out of a form, looked for in text read out of the editor.
    // Decode one door and not the other and this answers NIL — which it did,
    // measured and not guessed, and which is why the fix could not be local.
    load(&shared, GREEK, 3);
    says(&shared, &lisp, "(search-forward \"\u{3b3}\" 0)", "4");
    says(&shared, &lisp, "(search-backward \"\u{3b3}\" (point-max))", "4");
    // ...and the offset means what `goto-char` means by it.
    says(
        &shared,
        &lisp,
        "(progn (goto-char (search-forward \"\u{3b3}\" 0)) (point))",
        "4",
    );
    // The other direction over the same boundary: a character typed in from a
    // form lands in the buffer as itself and is found there afterwards.
    says(
        &shared,
        &lisp,
        "(progn (goto-char (point-max)) (insert \"\u{2014}\u{1d53d}\") \
                (format nil \"~a ~a\" (search-forward \"\u{1d53d}\" 0) (point-max)))",
        &format!("{} {}", GREEK.chars().count() + 1, GREEK.chars().count() + 2),
    );

    // --- org-modern: what a substitution draws ------------------------------
    //
    // Both paths that hand buffer text back as a `display` string. Before this
    // these came out as `cafÃ©` and `dÃ©jÃ  vu`, which is the accented link
    // description reported from a real document.
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
    // were, and a decode applied a layer too low would have moved them. `:begin`
    // is the first star of the unit heading, which is the line after a two-line
    // preamble and a blank.
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
