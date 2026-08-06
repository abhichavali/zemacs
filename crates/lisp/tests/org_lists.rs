//! `M-RET` in org: another item, with the same bullet or the next number.
//!
//! `%org-list-prefix` is hand-written string parsing — ECL ships no regexp
//! engine — so the grammar it accepts is worth pinning down. It is also the half
//! of the feature that can go wrong quietly: a bullet it fails to see makes the
//! key do nothing, and one it sees where there is none puts a bullet in the
//! middle of a paragraph.
//!
//! Loads the real `runtime/init.lisp` rather than a stub, because the binding is
//! part of the claim: `insert_key` consults only the `insert` keymap, so a
//! command bound to `org-mode` alone would be dead exactly while you are typing
//! the list.
//!
//! Deliberately one `#[test]`: `cl_boot` initialises a process-wide image, so
//! there is exactly one `spawn` per test binary.

use std::path::Path;
use std::time::{Duration, Instant};

use zemacs_core::Shared;

const PATIENCE: Duration = Duration::from_secs(30);

fn wait_message(shared: &Shared, from: usize, what: &str, pred: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        {
            let ed = shared.lock().unwrap();
            if let Some(m) = ed.messages.get(from..).and_then(|m| m.iter().find(|m| pred(m))) {
                return m.clone();
            }
            if Instant::now() >= deadline {
                let seen = &ed.messages[ed.messages.len().saturating_sub(12)..];
                panic!("timed out waiting for {what}; last messages {seen:#?}");
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Ask the image a question and get the answer back as a message. Everything the
/// image does is asynchronous, so a read is a wait. Transcribed from `ai.rs`.
fn probe(lisp: &zemacs_lisp::Lisp, shared: &Shared, tag: &str, form: &str) -> String {
    let from = shared.lock().unwrap().messages.len();
    lisp.eval(format!(
        "(message (format nil \"{tag} ~a\" (handler-case {form} (error (e) (format nil \"ERROR ~a\" e)))))"
    ));
    let prefix = format!("{tag} ");
    let line = wait_message(shared, from, tag, move |m| m.starts_with(&prefix));
    line[tag.len() + 1..].to_string()
}

#[test]
fn meta_return_continues_a_list_and_leaves_prose_alone() {
    let init = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp"),
    )
    .expect("runtime/init.lisp must exist");

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    wait_message(&shared, 0, "init.lisp to finish loading", |m| {
        m.contains("is driving the editor")
    });

    // NIL prints as NIL, so every "not a list item" answer is the same string
    // and a bullet that stopped being recognised shows up as one.
    let prefix = |line: &str| {
        probe(
            &lisp,
            &shared,
            "prefix",
            &format!("(%org-list-prefix {line:?})"),
        )
    };

    // --- the bullets --------------------------------------------------------
    assert_eq!(prefix("- milk"), "- ");
    assert_eq!(prefix("+ milk"), "+ ");
    // Indentation is carried, or a nested item's continuation would jump back
    // out to the top level.
    assert_eq!(prefix("  - milk"), "  - ");
    assert_eq!(prefix("    + milk"), "    + ");
    // An item with nothing in it yet is still an item.
    assert_eq!(prefix("- "), "- ");

    // --- the numbers --------------------------------------------------------
    //
    // Counting on is the whole point: `1.` is followed by `2.`, and the closing
    // paren form counts too because org accepts both.
    assert_eq!(prefix("1. milk"), "2. ");
    assert_eq!(prefix("9. milk"), "10. ");
    assert_eq!(prefix("12) milk"), "13) ");
    assert_eq!(prefix("   3. milk"), "   4. ");

    // --- what is not a list -------------------------------------------------
    //
    // A heading, which is `*` in column 0 — the case that makes `*` conditional
    // on indentation rather than simply another bullet.
    assert_eq!(prefix("* Heading"), "NIL");
    assert_eq!(prefix("*** Deep heading"), "NIL");
    // ...and the same character indented *is* a bullet, which is org's rule.
    assert_eq!(prefix("  * milk"), "  * ");
    // Prose, and prose that starts with the punctuation but has no space after
    // it: `-5 degrees` is a sentence, not a list.
    assert_eq!(prefix("just some prose"), "NIL");
    assert_eq!(prefix("-5 degrees outside"), "NIL");
    assert_eq!(prefix("1.5 metres"), "NIL");
    assert_eq!(prefix(""), "NIL");
    assert_eq!(prefix("   "), "NIL");
    // A bullet with nothing after it at all: no space, so not an item.
    assert_eq!(prefix("-"), "NIL");

    // --- the binding --------------------------------------------------------
    //
    // Both keymaps, and the `insert` one is the load-bearing half: it is the
    // mode you are in while writing the list. A binding is looked up by the
    // token `Key::MetaEnter` spells, so this also pins the spelling.
    let bound = probe(
        &lisp,
        &shared,
        "bound",
        r#"(format nil "~{~a~^ ~}"
             (sort (loop for (mode keys cmd) in (key-bindings)
                         when (string= keys "M-<ret>")
                           collect (format nil "~a=~a" mode cmd))
                   #'string<))"#,
    );
    assert_eq!(bound, "insert=org-meta-return org-mode=org-meta-return");
}
