//! `M-RET` in org: another one of whatever this line is.
//!
//! The readers in `runtime/modes/org-structure.lisp` are hand-written string
//! parsing — ECL ships no regexp engine — so the grammar they accept is worth
//! pinning down. They are also the half of the feature that goes wrong quietly:
//! a bullet not seen makes the key do nothing, and one seen where there is none
//! puts a bullet in the middle of a paragraph.
//!
//! Each answers a *prefix* rather than a boolean, which is what makes
//! `org-meta-return` a single `or` over the three — so the thing to pin is the
//! string each hands back, and the order they are asked in.
//!
//! Loads the real `runtime/init.lisp` rather than a stub, because the bindings
//! are part of the claim: `insert_key` consults only the `insert` keymap, so a
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

    // --- headings -----------------------------------------------------------
    //
    // The arm `M-RET` used to be missing entirely: on a heading it reported "not
    // on a list item" and did nothing. A sibling carries the level, so `***`
    // begets `***` rather than a top-level heading in the middle of a section.
    let heading = |line: &str| {
        probe(
            &lisp,
            &shared,
            "heading",
            &format!("(%org-heading-prefix {line:?})"),
        )
    };
    assert_eq!(heading("* Alpha"), "* ");
    assert_eq!(heading("*** Deep"), "*** ");
    // A task begets a task, with an empty box — but never the keyword, which is
    // Emacs' split between `M-RET' and `M-S-RET'.
    assert_eq!(heading("* [ ] ship"), "* [ ] ");
    assert_eq!(heading("* [X] ship"), "* [ ] ");
    assert_eq!(heading("** TODO [X] ship"), "** [ ] ");
    assert_eq!(heading("* TODO ship"), "* ");
    // org's own rule: stars at column 0 *followed by a space*. Without the space
    // test `**bold**` opening a line would beget a level-2 heading.
    assert_eq!(heading("**bold** word"), "NIL");
    assert_eq!(heading("  * indented"), "NIL");
    assert_eq!(heading("- item"), "NIL");
    assert_eq!(heading(""), "NIL");

    // --- checkboxes ---------------------------------------------------------
    //
    // Asked *before* the plain list reader in `org-meta-return`, because every
    // checkbox item is also a list item and the plain answer would drop the box.
    // The new box is always empty, whatever this one holds: the item you are
    // about to write is a thing still to do.
    let checkbox = |line: &str| {
        probe(
            &lisp,
            &shared,
            "checkbox",
            &format!("(%org-checkbox-prefix {line:?})"),
        )
    };
    assert_eq!(checkbox("- [ ] milk"), "- [ ] ");
    assert_eq!(checkbox("- [X] milk"), "- [ ] ");
    assert_eq!(checkbox("- [-] milk"), "- [ ] ");
    assert_eq!(checkbox("  + [X] milk"), "  + [ ] ");
    // Counts on and keeps the box, which needs both readers to agree.
    assert_eq!(checkbox("3. [X] milk"), "4. [ ] ");
    // A list item with no box is not a checkbox item...
    assert_eq!(checkbox("- milk"), "NIL");
    // ...and a cookie that is not a list item is prose. `the [x] column` is a
    // sentence, and ticking it would be an edit nobody asked for.
    assert_eq!(checkbox("the [x] column"), "NIL");
    assert_eq!(checkbox("* [X] heading"), "NIL");

    // --- an item with nothing in it yet -------------------------------------
    //
    // The way *out* of a list. `M-RET` on one of these drops the bullet instead
    // of laying another empty one under it, so the key that started the list can
    // also finish it — and the reader has to say no to everything that merely
    // looks empty, or a press would eat a line somebody was still writing.
    let empty = |line: &str| {
        probe(
            &lisp,
            &shared,
            "emptyitem",
            &format!("(and (%org-empty-item-p {line:?}) t)"),
        )
    };
    assert_eq!(empty("- "), "T");
    assert_eq!(empty("  + "), "T");
    assert_eq!(empty("3. "), "T");
    // A box is part of the bullet, not content — an unwritten task is unwritten.
    assert_eq!(empty("- [ ] "), "T");
    assert_eq!(empty("  - [X]  "), "T");
    // ...and anything after it is.
    assert_eq!(empty("- milk"), "NIL");
    assert_eq!(empty("- [ ] milk"), "NIL");
    // An empty *heading* is still a heading: `M-RET` on one gives another.
    assert_eq!(empty("* "), "NIL");
    assert_eq!(empty(""), "NIL");
    assert_eq!(empty("   "), "NIL");

    // --- M-S-RET's prefix ---------------------------------------------------
    //
    // In a list it is another item carrying a box, not a headline: `M-S-RET` in
    // the middle of a checklist used to abandon the list and start a section.
    let task = |line: &str| {
        probe(
            &lisp,
            &shared,
            "taskprefix",
            &format!("(%org-todo-heading-prefix {line:?})"),
        )
    };
    assert_eq!(task("- milk"), "- [ ] ");
    assert_eq!(task("- [X] milk"), "- [ ] ");
    assert_eq!(task("   3. milk"), "   4. [ ] ");
    // Off a list it is a heading with the keyword `%org-heading-with` writes.
    assert_eq!(task("* Alpha"), "* TODO ");
    assert_eq!(task("*** [X] ship"), "*** TODO [ ] ");

    // --- a heading's box, and where it may sit ------------------------------
    //
    // At the front of the heading's text, after a TODO keyword if there is one,
    // and nowhere else. The last two cases are the ones that matter: a cookie
    // further along a heading is prose, and letting `C-c C-c' tick it would be a
    // wrong answer that edits the file.
    let hbox = |line: &str| {
        probe(
            &lisp,
            &shared,
            "hbox",
            &format!("(%org-heading-box {line:?} (%org-line-level {line:?}))"),
        )
    };
    assert_eq!(hbox("* [ ] ship"), "(2 . 5)");
    assert_eq!(hbox("*** [X] ship"), "(4 . 7)");
    assert_eq!(hbox("* TODO [ ] ship"), "(7 . 10)");
    assert_eq!(hbox("* DONE [X] ship"), "(7 . 10)");
    assert_eq!(hbox("* Fix the [ ] renderer"), "NIL");
    assert_eq!(hbox("* Tasks [2/3]"), "NIL");
    assert_eq!(hbox("* ship"), "NIL");
    assert_eq!(hbox("* TODO ship"), "NIL");

    // --- the tie ------------------------------------------------------------
    //
    // `%org-heading-with' is the only writer of either name, and takes the state
    // *once*: DONE decides the box and decides which keyword, so no call to it
    // can leave the two disagreeing. KEYWORD-P is the separate question of
    // whether the heading says so in words at all.
    let with = |line: &str, done: &str, kw: &str| {
        probe(
            &lisp,
            &shared,
            "with",
            &format!("(%org-heading-with {line:?} (%org-line-level {line:?}) {done} {kw})"),
        )
    };
    // Both names move together, in both directions.
    assert_eq!(with("* TODO [ ] ship", "t", "t"), "* DONE [X] ship");
    assert_eq!(with("* DONE [X] ship", "nil", "t"), "* TODO [ ] ship");
    // A box with no keyword is the whole state, and stays that way.
    assert_eq!(with("* [ ] ship", "t", "nil"), "* [X] ship");
    assert_eq!(with("* [X] ship", "nil", "nil"), "* [ ] ship");
    // Dropping the wording keeps the state readable in the box that remains.
    assert_eq!(with("* DONE [X] ship", "t", "nil"), "* [X] ship");
    // A keyword with no box, and a box with no keyword, each on their own.
    assert_eq!(with("* TODO ship", "t", "t"), "* DONE ship");
    assert_eq!(with("* ship", "nil", "t"), "* TODO ship");
    // Everything it was not asked about is carried across untouched — the
    // deeper stars, the cookie, the text after it.
    assert_eq!(with("*** TODO [ ] a [1/2] b", "t", "t"), "*** DONE [X] a [1/2] b");
    // No trailing space when the keyword is the whole heading. That space would
    // survive every later press of the key.
    assert_eq!(with("** TODO", "t", "t"), "** DONE");
    assert_eq!(with("** DONE", "nil", "nil"), "** ");

    // --- statistics cookies -------------------------------------------------
    //
    // What counts as one, which is the half that can go wrong quietly: `[ ]` and
    // `[X]` must not, or ticking a box would rewrite the box.
    let cookie = |body: &str| {
        probe(
            &lisp,
            &shared,
            "cookie",
            &format!("(%org-cookie-body-p {body:?})"),
        )
    };
    assert_eq!(cookie("2/3"), "T");
    assert_eq!(cookie("67%"), "T");
    // org's empty forms — you write `[/]` and it fills itself in.
    assert_eq!(cookie("/"), "T");
    assert_eq!(cookie("%"), "T");
    // Checkboxes are not cookies.
    assert_eq!(cookie(" "), "NIL");
    assert_eq!(cookie("X"), "NIL");
    assert_eq!(cookie("-"), "NIL");
    // ...and neither is a link target or a citation.
    assert_eq!(cookie("fn:1"), "NIL");
    assert_eq!(cookie("2/x"), "NIL");
    assert_eq!(cookie(""), "NIL");

    // Everything above is a *pure* reader, and that is a property of this
    // harness rather than a preference: `spawn` here gets a bare `Editor`, whose
    // live buffer is the generated dashboard and refuses every edit, and the
    // commands that would open a real one need an app loop that is not running.
    // So what a command *does to a buffer* is tested end to end over the control
    // protocol — see `org_structure_edits_an_outline` in `crates/app/tests`.

    // --- the bindings -------------------------------------------------------
    //
    // `M-RET` is in both keymaps, and the `insert` one is the load-bearing half:
    // it is the mode you are in while writing the list. A binding is looked up
    // by the token `Key::MetaEnter` spells, so this also pins the spelling.
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

    // Every non-leader key org-mode claims for itself, which is worth pinning
    // whole rather than one addition at a time: these are bound in the *mode*
    // keymap and a mode keymap is not consulted from Insert, so the list is
    // exactly the set of keys that behave differently in an org buffer while you
    // are in Normal state — and each of them displaces something. `M-<left>` is
    // word-wise motion everywhere else, `C-c C-c` is `eval-dwim`, `<tab>` is
    // `fold-dwim`. Binding any of them globally is the thing not to do.
    //
    // Three of them are *dispatchers* rather than the command they name, and
    // that is what `org-table.lisp` costs: `<tab>`, `<backtab>` and `<ret>` are
    // table keys in a table and hand themselves straight on to `org-cycle`,
    // `org-global-cycle` and `org-open-at-point` everywhere else. There is no
    // way to decline a keystroke once a binding has swallowed it, so a key that
    // only sometimes applies has to be claimed whole and forwarded — which is
    // why the fall-through is pinned in `org_table.rs` rather than assumed.
    let org_keys = probe(
        &lisp,
        &shared,
        "orgkeys",
        r#"(format nil "~{~a~^ ~}"
             (sort (loop for (mode keys cmd) in (key-bindings)
                         when (and (string= mode "org-mode")
                                   (search "org-" (string cmd))
                                   (not (search "SPC" keys)))
                           collect (format nil "~a=~a" keys cmd))
                   #'string<))"#,
    );
    assert_eq!(
        org_keys,
        "<backtab>=org-table-backtab <ret>=org-table-return <tab>=org-table-tab \
         C-c C-c=org-ctrl-c-ctrl-c C-c C-l=org-insert-link C-c C-t=org-todo \
         C-c R=org-latex-preview-clear C-c l=org-store-link C-c r=org-latex-preview \
         M-<left>=org-do-promote M-<ret>=org-meta-return M-<right>=org-do-demote \
         M-S-<left>=org-promote-subtree M-S-<ret>=org-insert-todo-heading \
         M-S-<right>=org-demote-subtree M-v=org-paste-image",
    );
}
