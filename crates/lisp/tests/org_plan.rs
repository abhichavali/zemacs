//! Headless proof of the two document-keeping gestures: dates and links.
//!
//! `org-schedule`/`org-deadline` write a planning line under a headline, and
//! `org-store-link`/`org-insert-link` are the two halves of writing a link. Both
//! are new, and both have the shape that wants a test rather than a reading: a
//! date has to come out with the *right weekday* beside it, and a headline that
//! already carries one planning keyword has to keep it when the other arrives.
//!
//! The dates in here are real — 2026-08-17 genuinely is a Monday — so a weekday
//! computed the wrong way round fails rather than merely looking odd.
//!
//! Deliberately a single `#[test]`, as in every file beside it: `cl_boot`
//! initialises a process-wide Lisp image, so there is one `spawn` per binary.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Mode, Shared};

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

fn wait_message(shared: &Shared, what: &str, pred: impl Fn(&str) -> bool) {
    wait(shared, what, |ed| ed.messages.iter().any(|m| pred(m)).then_some(()));
}

/// Evaluate `form` and wait for its value to be `want`.
///
/// `tag` is load-bearing, not decoration: `messages` accumulates, so an untagged
/// assertion matches whatever identical line an earlier step already left in the
/// list and passes without the form ever being evaluated.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, tag: &str, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"{tag}=~a\" {form}))"));
    let want = format!("{tag}={want}");
    wait_message(shared, &format!("{form} => {want}"), |m| m == want);
}

#[test]
fn dates_land_under_the_headline_and_links_are_stored_and_pasted() {
    // The *shipped* config, as `org_lists.rs` does it: `org-structure.lisp` needs
    // `org-fold.lisp`, which needs `define-leader` out of `library.lisp`, and a
    // hand-written init listing four modules only re-derives the load order that
    // `*runtime-modules*` already states. Loading the real one also means the new
    // bindings below are the ones a running editor actually has.
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

    // --- the date reader, with no buffer involved ---------------------------
    //
    // 2026-08-17 really is a Monday and 2026-09-01 really is a Tuesday, so a
    // weekday derived the wrong way round is a failure and not a curiosity.
    says(&shared, &lisp, "stamp", "(%org-timestamp 2026 8 17)", "<2026-08-17 Mon>");
    says(&shared, &lisp, "stamp2", "(%org-timestamp 2026 9 1)", "<2026-09-01 Tue>");
    says(&shared, &lisp, "iso", "(%org-parse-iso \"2026-08-17\")", "(2026 8 17)");
    // Shape, not spelling: a month of 13 is a typo and refused.
    says(&shared, &lisp, "iso-bad", "(%org-parse-iso \"2026-13-01\")", "NIL");
    says(&shared, &lisp, "iso-junk", "(%org-parse-iso \"tomorrow\")", "NIL");
    // `+3d` counts from *today*, so the assertion is the arithmetic rather than
    // a fixed answer: three days on from today is what today's date plus three
    // decodes to, whenever this test happens to run.
    says(
        &shared,
        &lisp,
        "offset",
        "(equal (%org-date-offset \"+3d\")
                (multiple-value-bind (s mi h d mo y)
                    (decode-universal-time (+ (get-universal-time) (* 3 86400)))
                  (declare (ignore s mi h))
                  (list y mo d)))",
        "T",
    );
    says(&shared, &lisp, "offset-bad", "(%org-date-offset \"+3q\")", "NIL");
    // An empty answer is today; a cancelled prompt is handled by the caller.
    says(&shared, &lisp, "empty-today", "(equal (%org-parse-date \"\") (%org-today))", "T");

    // --- planning lines are told apart from prose ---------------------------
    says(&shared, &lisp, "plan-yes",
         "(and (%org-planning-line-p \"SCHEDULED: <2026-08-17 Mon>\") t)", "T");
    // The *first* word is the test, so a sentence mentioning a deadline is prose.
    says(&shared, &lisp, "plan-no",
         "(and (%org-planning-line-p \"I have a DEADLINE: soon\") t)", "NIL");
    says(&shared, &lisp, "plan-stamps",
         "(%org-planning-stamps \"DEADLINE: <2026-09-01 Tue> SCHEDULED: <2026-08-17 Mon>\")",
         "((DEADLINE . <2026-09-01 Tue>) (SCHEDULED . <2026-08-17 Mon>))");

    // --- and they land under the right headline -----------------------------
    {
        let mut ed = shared.lock().unwrap();
        ed.load("", Some(PathBuf::from("/tmp/zemacs_org_plan_test.org")), None);
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::InsertText(
            "* TODO Renew the passport\nsome body text\n* Another\n".into(),
        ));
        ed.apply(EditorCommand::MoveTo(30)); // inside the first task's body
        ed.apply(EditorCommand::SetMode(Mode::Normal));
    }
    lisp.eval("(org-mode)".into());
    wait(&shared, "org-mode", |ed| (ed.buffer.major_mode == "org-mode").then_some(()));

    // From the *body* of the task, not its headline: this is the case that makes
    // the command usable, since that is where the cursor actually is.
    lisp.eval("(%org-plan-put (%org-headline-above) \"SCHEDULED\" \"<2026-08-17 Mon>\")".into());
    says(&shared, &lisp, "sched", "(line-string 2)", "SCHEDULED: <2026-08-17 Mon>");

    // A second keyword joins the first rather than replacing the line, and comes
    // out in org's order — DEADLINE before SCHEDULED, whichever was added first.
    lisp.eval("(%org-plan-put (%org-headline-above) \"DEADLINE\" \"<2026-09-01 Tue>\")".into());
    says(&shared, &lisp, "both", "(line-string 2)",
         "DEADLINE: <2026-09-01 Tue> SCHEDULED: <2026-08-17 Mon>");

    // ...and re-scheduling replaces its own stamp and keeps the other one.
    lisp.eval("(%org-plan-put (%org-headline-above) \"SCHEDULED\" \"<2026-08-24 Mon>\")".into());
    says(&shared, &lisp, "reschedule", "(line-string 2)",
         "DEADLINE: <2026-09-01 Tue> SCHEDULED: <2026-08-24 Mon>");
    // The body survived all three: a planning line is inserted and rewritten,
    // never written over the text under it.
    says(&shared, &lisp, "body-kept", "(line-string 3)", "some body text");

    // --- links --------------------------------------------------------------
    //
    // Stored from the body again, and the target is the whole post-stars text
    // including the keyword — because that is the string `%org-heading-position'
    // compares against, so this is a link that actually resolves.
    lisp.eval("(org-store-link)".into());
    says(&shared, &lisp, "stored", "(car *org-stored-link*)", "*TODO Renew the passport");
    // ...and it resolves: the reader on the other side finds the headline.
    says(&shared, &lisp, "resolves",
         "(and (%org-heading-position \"TODO Renew the passport\") t)", "T");

    // Pasting one, with and without a description. Asserted by searching the
    // buffer rather than by reading a line: in Normal state the cursor is
    // clamped to the last *character* of a line, so where an `insert' lands is a
    // question about the clamp and not about the link — see `%org-open-line-below',
    // which exists because of that exact trap. The shape of what was written is
    // what this is pinning.
    lisp.eval("(goto-char (point-max))(%org-put-link \"*Another\" \"the other one\" nil)".into());
    says(&shared, &lisp, "pasted",
         "(and (search \"[[*Another][the other one]]\" (buffer-string)) t)", "T");
    // A link with no description is `[[target]]', not `[[target][]]' — org's own
    // spelling, and the one `%org-link-at-point' reads back.
    lisp.eval("(%org-put-link \"*Another\" nil nil)".into());
    says(&shared, &lisp, "pasted-bare",
         "(and (search \"[[*Another]]\" (buffer-string)) t)", "T");

    // --- the keys the config actually ended up with -------------------------
    //
    // `SPC m l` used to be bound twice — demote in `org-structure.lisp` and
    // latex-preview here — and the runtime modules load last, so the preview
    // spelling never once ran. Pinned in both directions so it cannot silently
    // come back.
    says(&shared, &lisp, "k-demote",
         r#"(second (first (where-is "org-do-demote")))"#, "M-<right>");
    says(&shared, &lisp, "k-latex",
         r#"(and (member "SPC m e" (mapcar #'second (where-is "org-latex-preview"))
                         :test #'string=) t)"#, "T");
    says(&shared, &lisp, "k-store",
         r#"(and (member "SPC m y" (mapcar #'second (where-is "org-store-link"))
                         :test #'string=) t)"#, "T");
    says(&shared, &lisp, "k-sched",
         r#"(and (member "SPC m s" (mapcar #'second (where-is "org-schedule"))
                         :test #'string=) t)"#, "T");

    // --- the outline keys, from where the cursor actually is ----------------
    //
    // `C-c C-t` and `M-<left>` used to answer "not on a headline" anywhere but
    // the headline's own line, which is the one line of an entry you are almost
    // never standing on: you press them while writing the body. `M-S-<left>`
    // already reached the headline above through `%org-headline-above`, so the
    // two halves of the same key pair disagreed. Emacs' `org-todo` and
    // `org-promote' both open with `org-back-to-heading' and mean this.
    {
        let mut ed = shared.lock().unwrap();
        ed.load("", Some(PathBuf::from("/tmp/zemacs_org_struct_test.org")), None);
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::InsertText("* Alpha\nbody text\n".into()));
        ed.apply(EditorCommand::MoveTo(10)); // inside `body text`
        ed.apply(EditorCommand::SetMode(Mode::Normal));
    }
    lisp.eval("(org-mode)".into());
    lisp.eval("(org-todo)".into());
    says(&shared, &lisp, "todo-from-body", "(line-string 1)", "* TODO Alpha");
    // ...and it left the cursor in the body rather than up on the line it wrote.
    says(&shared, &lisp, "todo-point", "(line-number)", "2");
    lisp.eval("(org-do-demote)".into());
    says(&shared, &lisp, "demote-from-body", "(line-string 1)", "** TODO Alpha");
    says(&shared, &lisp, "demote-body-kept", "(line-string 2)", "body text");

    // --- and the way out of a list ------------------------------------------
    //
    // `M-RET` on an item with nothing in it drops the bullet instead of laying
    // a fourth empty one under the third, and a second press writes the heading
    // *on* the blank line rather than opening yet another one below it. Both
    // are asserted through `line-count` as well as the text, because the failure
    // in each direction is a line that should not be there.
    {
        let mut ed = shared.lock().unwrap();
        ed.load("", Some(PathBuf::from("/tmp/zemacs_org_struct_list.org")), None);
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::InsertText("- milk\n- \n".into()));
        ed.apply(EditorCommand::MoveTo(8)); // on the empty item
        ed.apply(EditorCommand::SetMode(Mode::Normal));
    }
    lisp.eval("(org-mode)".into());
    lisp.eval("(org-meta-return)".into());
    says(&shared, &lisp, "list-ended", "(line-string 2)", "");
    says(&shared, &lisp, "list-ended-lines", "(line-count)", "3");
    says(&shared, &lisp, "list-kept", "(line-string 1)", "- milk");
    lisp.eval("(org-meta-return)".into());
    says(&shared, &lisp, "blank-in-place", "(line-string 2)", "* ");
    says(&shared, &lisp, "blank-no-new-line", "(line-count)", "3");

    // --- and on a headline that is folded shut ------------------------------
    //
    // A fold runs from the headline's own line to the end of its subtree, so
    // the end of that line — where `%org-open-line-below' writes — is *inside*
    // it. The new heading went in where nobody could see it, and `clamp_cursor'
    // then pulled point back to the fold's head, so the next thing typed landed
    // in front of the headline you started from: `New* one'. Both halves are
    // pinned here, because opening the fold without moving point right would
    // still leave you typing in the wrong place.
    {
        let mut ed = shared.lock().unwrap();
        ed.load("", Some(PathBuf::from("/tmp/zemacs_org_struct_fold.org")), None);
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::InsertText("* one\nbody\n* two\n".into()));
        ed.apply(EditorCommand::MoveTo(0)); // on `* one`
        ed.apply(EditorCommand::SetMode(Mode::Normal));
    }
    lisp.eval("(org-mode)".into());
    lisp.eval("(fold-dwim)".into());
    says(&shared, &lisp, "fold-made", "(if (folded-p (line-start 1)) \"yes\" \"no\")", "yes");
    lisp.eval("(org-meta-return)".into());
    says(&shared, &lisp, "fold-opened", "(if (folded-p (line-start 1)) \"yes\" \"no\")", "no");
    says(&shared, &lisp, "fold-new-heading", "(line-string 2)", "* ");
    says(&shared, &lisp, "fold-body-kept", "(line-string 3)", "body");
    // The one that actually bit: what you type next.
    lisp.eval("(insert \"New\")".into());
    says(&shared, &lisp, "fold-typed-here", "(line-string 2)", "* New");
    says(&shared, &lisp, "fold-headline-intact", "(line-string 1)", "* one");
}
