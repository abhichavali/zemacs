//! The IDE surface over LSP: hover, symbols, and the one command that writes.
//!
//! `lsp.lisp` had go-to-definition and nothing else you would call an IDE. The
//! jumps that joined it — declaration, type definition, implementation — are one
//! table and are not interesting; what is interesting, and what this pins, is
//! the two places with arithmetic in them:
//!
//! * **`%lsp-offset-in`**, which turns an LSP position into an offset in a
//!   document that is a *string read from disk* rather than the live buffer, and
//!   in UTF-16 units rather than characters;
//! * **`%lsp-apply-edits`**, which applies a `WorkspaceEdit`'s ranges back to
//!   front, because every one of them is expressed against the document as it
//!   was and applying the first would move the rest.
//!
//! Plus `%lsp-hover-text`, which is not arithmetic but is three shapes a server
//! may send where a `jget` would only handle one.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::path::PathBuf;
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
fn a_rename_lands_back_to_front_and_a_hover_reads_every_shape() {
    let dir = std::env::temp_dir().join(format!("zemacs_lsp_ide-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("rename.rs");
    std::fs::write(&file, "let alpha = 1;\nlet beta = alpha + alpha;\n").unwrap();

    let init = std::env::temp_dir().join(format!("zemacs_lsp_ide_init-{}.lisp", std::process::id()));
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (message \"lsp ide test init loaded\")\n",
            runtime("modes/modes.lisp"),
            runtime("rpc.lisp"),
            runtime("lsp.lisp"),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init.clone());
    wait(&shared, "the init file", |ed| {
        ed.messages
            .iter()
            .any(|m| m == "lsp ide test init loaded")
            .then_some(())
    });

    // --- positions in a document that is a string ---------------------------
    //
    // `let alpha = 1;\n` is fifteen characters, so line 1 starts at 15.
    let text = "\"let alpha = 1;\" \"let beta = alpha;\"";
    let doc = format!("(format nil \"~a~%~a~%\" {text})");
    says(&shared, &lisp, &format!("(%lsp-offset-in {doc} 0 0)"), "0");
    says(&shared, &lisp, &format!("(%lsp-offset-in {doc} 0 4)"), "4");
    says(&shared, &lisp, &format!("(%lsp-offset-in {doc} 1 0)"), "15");
    says(&shared, &lisp, &format!("(%lsp-offset-in {doc} 1 4)"), "19");
    // A column past the end of its line stops there rather than running into
    // the next one — a server may name one past the last character.
    says(&shared, &lisp, &format!("(%lsp-offset-in {doc} 0 999)"), "14");
    // ...and a line past the end of the document is the end of the document —
    // 15 for the first line and 18 for the second, both counting their newline.
    says(&shared, &lisp, &format!("(%lsp-offset-in {doc} 99 0)"), "33");
    // UTF-16 columns, not characters: `é` is one character and one unit, but an
    // emoji is one character and *two* — a column after it has to come back to
    // the character that follows, not to the middle of it.
    says(
        &shared,
        &lisp,
        "(%lsp-offset-in (format nil \"a~ab\" (code-char #x1F600)) 0 3)",
        "2",
    );

    // --- the rename itself --------------------------------------------------
    //
    // Two edits on one line, given in *ascending* order, which is the order a
    // server sends them and the order that is wrong to apply in: the first
    // replacement is shorter than what it replaces, so applying it first would
    // leave the second edit's range pointing two characters past its symbol.
    let edits = "(list (jobj \"newText\" \"w\"
                             \"range\" (jobj \"start\" (jobj \"line\" 1 \"character\" 11)
                                             \"end\"   (jobj \"line\" 1 \"character\" 16)))
                       (jobj \"newText\" \"w\"
                             \"range\" (jobj \"start\" (jobj \"line\" 1 \"character\" 19)
                                             \"end\"   (jobj \"line\" 1 \"character\" 24))))";
    lisp.eval(format!(
        "(message (format nil \"edits ~a\" (%lsp-apply-edits \"{}\" {edits})))",
        file.display()
    ));
    wait(&shared, "the rename", |ed| {
        ed.messages.iter().any(|m| m == "edits 2").then_some(())
    });
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "let alpha = 1;\nlet beta = w + w;\n",
        "the edits were applied front to back and the second one slipped"
    );
    // An unreadable file answers NIL rather than signalling, so one bad path in
    // a workspace edit does not take the rest of the rename down with it.
    says(&shared, &lisp, "(%lsp-apply-edits \"/nope/gone\" nil)", "NIL");

    // --- hover's three shapes -----------------------------------------------
    says(
        &shared,
        &lisp,
        "(%lsp-hover-text (jobj \"contents\" \"plain\"))",
        "plain",
    );
    says(
        &shared,
        &lisp,
        "(%lsp-hover-text (jobj \"contents\" (jobj \"kind\" \"markdown\" \"value\" \"marked\")))",
        "marked",
    );
    // An array of MarkedString, which is the shape that made this a function
    // rather than a `jget`: the parts are joined rather than the first one won.
    says(
        &shared,
        &lisp,
        "(%lsp-hover-text (jobj \"contents\" (jarr \"one\" (jobj \"value\" \"two\"))))",
        "one

two",
    );
    says(&shared, &lisp, "(%lsp-hover-text (jobj \"contents\" nil))", "NIL");

    // --- symbols ------------------------------------------------------------
    //
    // Nested `DocumentSymbol`s carry their parent's name in front of them, which
    // is what makes a method findable by typing the class it is on.
    let symbols = "(jarr (jobj \"name\" \"Widget\" \"kind\" 5
                              \"range\" (jobj \"start\" (jobj \"line\" 0 \"character\" 0))
                              \"children\" (jarr (jobj \"name\" \"draw\" \"kind\" 6
                                                       \"range\" (jobj \"start\" (jobj \"line\" 3 \"character\" 2))))))";
    says(
        &shared,
        &lisp,
        &format!("(length (%lsp-symbol-rows {symbols} \"/tmp/w.rs\"))"),
        "2",
    );
    says(
        &shared,
        &lisp,
        &format!("(car (second (%lsp-symbol-rows {symbols} \"/tmp/w.rs\")))"),
        "Widget.draw  [method]",
    );
    // The destination rides with the label, and is what `find-file-at` parses.
    says(
        &shared,
        &lisp,
        &format!("(cdr (second (%lsp-symbol-rows {symbols} \"/tmp/w.rs\")))"),
        "/tmp/w.rs:4:",
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&init);
}
