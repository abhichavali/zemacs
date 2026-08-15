//! `runtime/modes/org-table.lisp` — a table laid out, and the three keys that
//! walk one.
//!
//! The feature is entirely Lisp over `line-string` and `replace-region`, so the
//! honest check is to boot the real image against a real [`Editor`], put a
//! ragged table in a buffer and read back the characters the rope actually
//! holds. Two things are worth proving beyond "it lines up": that the numeric
//! columns went to the right by org's vote rather than by position, and that
//! TAB, S-TAB and RET **fall through** to the outline commands they displaced
//! whenever point is not in a table — a key that only sometimes applies is the
//! part of this that could quietly eat the org bindings.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image,
//! so there is exactly one `spawn` per test binary and a new file is the only
//! way to add one.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Mode, Shared};

const PATIENCE: Duration = Duration::from_secs(30);
static NTH: AtomicUsize = AtomicUsize::new(0);

/// The main loop's one job that matters here. Core records that a mode hook is
/// *due* and never calls Lisp itself; the app drains `pending_hooks` and asks
/// the image to run each one. There is no app in a test, so this is it — and it
/// is what makes the buffer below actually enter `org-mode`.
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
                "timed out waiting for {what}\nbuffer:\n{}\nmessages:\n  {}",
                ed.buffer.text,
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

/// Evaluate `form` and wait for its value, printed with `~a`, to be `want`.
///
/// Numbered, unlike `xref.rs`' version, because the log is cumulative and this
/// file asks the same question repeatedly: without the counter the second
/// `(line-number)` of `4` would be satisfied by the first one and the wait would
/// prove nothing about the key pressed in between.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    let tag = NTH.fetch_add(1, Ordering::Relaxed);
    lisp.eval(format!("(message (format nil \"#{tag} ~a\" {form}))"));
    let want = format!("#{tag} {want}");
    wait(shared, form, |ed| {
        ed.messages.iter().any(|m| *m == want).then_some(())
    });
}

/// Wait for a message this file did not number — one a *command* produced,
/// which is how the fall-throughs are observed.
fn wait_message(shared: &Shared, want: &str) {
    wait(shared, want, |ed| {
        ed.messages.iter().any(|m| m == want).then_some(())
    });
}

/// The offset of point within its line, which is the only thing worth asserting
/// about where a table key left the cursor.
fn column(shared: &Shared, lisp: &zemacs_lisp::Lisp, want: &str) {
    says(shared, lisp, "(- (point) (line-start))", want);
}

/// Put `text` in the live buffer as an org file, cursor at 0.
///
/// A fresh path each time, for the reason `org_modern.rs` gives: `Editor::load`
/// switches to a buffer already holding that path rather than reloading it.
fn load(shared: &Shared, text: &str, nth: usize) {
    let mut ed = shared.lock().unwrap();
    ed.load(
        text,
        Some(PathBuf::from(format!("/tmp/zemacs_test_org_table_{nth}.org"))),
        Some("org".into()),
    );
    ed.apply(EditorCommand::SetMode(Mode::Normal));
    ed.apply(EditorCommand::MoveTo(0));
}

fn runtime(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .expect("the runtime directory ships with the source")
}

/// Ragged on purpose, and rougher than anything anyone would type by hand: the
/// rule row is the wrong width for its own table, which is the case that says
/// the rules are *regenerated* rather than padded.
const NOTES: &str = "\
* Fruit
| Name | Qty | Price |
|---+---|
| Apple | 3 | 1.20 |
| Pear | 12 | 0.85 |
tail
";

/// `Name` stays left because its column is words; `Qty` and `Price` go right
/// because two of each column's three cells parse as numbers — org's vote, in
/// which the header abstains by losing rather than by being special-cased.
const ALIGNED: &str = "\
* Fruit
| Name  | Qty | Price |
|-------+-----+-------|
| Apple |   3 |  1.20 |
| Pear  |  12 |  0.85 |
tail
";

#[test]
fn a_table_lays_itself_out_and_tab_walks_it() {
    let init =
        std::env::temp_dir().join(format!("zemacs_org_table_init-{}.lisp", std::process::id()));
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (message \"org-table test init loaded\")\n",
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
            .any(|m| m == "org-table test init loaded")
            .then_some(())
    });
    says(&shared, &lisp, "(if (fboundp 'org-table-align) \"yes\" \"no\")", "yes");

    // --- aligning -----------------------------------------------------------

    load(&shared, NOTES, 0);
    says(&shared, &lisp, "(major-mode)", "org-mode");

    // Line 4 is `| Apple | 3 | 1.20 |`; offset 2 is the `A`, which is cell 0.
    lisp.eval("(goto-char (+ (line-start 4) 2))".into());
    lisp.eval("(org-table-align)".into());
    // Point comes back to the cell it was in, at the front of it — the cell
    // survives a rewrite that changed every column's width, and an offset does
    // not.
    says(&shared, &lisp, "(line-number)", "4");
    column(&shared, &lisp, "2");
    assert_eq!(shared.lock().unwrap().buffer.text.to_string(), ALIGNED);

    // Idempotent, which is the property that makes it safe to run on every TAB:
    // a table that is already laid out is not rewritten at all, so there is no
    // undo step and no `after-change-hook` per press.
    lisp.eval("(org-table-align)".into());
    says(&shared, &lisp, "(line-number)", "4");
    assert_eq!(shared.lock().unwrap().buffer.text.to_string(), ALIGNED);

    // --- TAB ----------------------------------------------------------------

    // Along the row. `3` sits at the far end of its field because the column is
    // numeric, and that is where point lands — which is where you would be
    // typing the next figure.
    lisp.eval("(org-table-tab)".into());
    says(&shared, &lisp, "(line-number)", "4");
    column(&shared, &lisp, "12");

    lisp.eval("(org-table-tab)".into());
    column(&shared, &lisp, "17");

    // Past the last cell of the row: the first cell of the next one.
    lisp.eval("(org-table-tab)".into());
    says(&shared, &lisp, "(line-number)", "5");
    column(&shared, &lisp, "2");

    // Three more presses walk the last row and then run out of table, which is
    // where TAB grows one rather than refusing.
    lisp.eval("(org-table-tab)".into());
    lisp.eval("(org-table-tab)".into());
    lisp.eval("(org-table-tab)".into());
    says(&shared, &lisp, "(line-number)", "6");
    column(&shared, &lisp, "2");
    says(
        &shared,
        &lisp,
        "(string= (line-string 6) \"|       |     |       |\")",
        "T",
    );
    // The new row is laid out to the widths the table already had — an empty
    // cell abstains from the numeric vote, so a fresh row cannot flip a column
    // back to the left.
    says(&shared, &lisp, "(string= (line-string 7) \"tail\")", "T");

    // --- S-TAB and RET ------------------------------------------------------

    // Back off the front of the new row: the *last* cell of the row above.
    lisp.eval("(org-table-backtab)".into());
    says(&shared, &lisp, "(line-number)", "5");
    column(&shared, &lisp, "17");

    // ...and down again, staying in the column, which is what RET means here.
    lisp.eval("(org-table-return)".into());
    says(&shared, &lisp, "(line-number)", "6");
    column(&shared, &lisp, "21");

    // --- rows and columns ---------------------------------------------------

    load(&shared, NOTES, 1);
    lisp.eval("(goto-char (+ (line-start 4) 2))".into());

    lisp.eval("(org-table-move-column-right)".into());
    says(
        &shared,
        &lisp,
        "(string= (line-string 2) \"| Qty | Name  | Price |\")",
        "T",
    );
    // The alignment is recomputed rather than carried: the column of counts is
    // still right-aligned now that it is first, and the column of words is
    // still left.
    says(
        &shared,
        &lisp,
        "(string= (line-string 4) \"|   3 | Apple |  1.20 |\")",
        "T",
    );
    // Point followed the column it moved.
    column(&shared, &lisp, "8");

    lisp.eval("(org-table-move-column-left)".into());
    says(
        &shared,
        &lisp,
        "(string= (line-string 2) \"| Name  | Qty | Price |\")",
        "T",
    );

    // An empty column is one character wide and left-aligned — a column of
    // nothing is not a column of numbers.
    lisp.eval("(org-table-insert-column)".into());
    says(
        &shared,
        &lisp,
        "(string= (line-string 2) \"|   | Name  | Qty | Price |\")",
        "T",
    );
    lisp.eval("(org-table-delete-column)".into());
    says(
        &shared,
        &lisp,
        "(string= (line-string 2) \"| Name  | Qty | Price |\")",
        "T",
    );

    // Above, which is where org puts it.
    lisp.eval("(org-table-insert-row)".into());
    says(
        &shared,
        &lisp,
        "(string= (line-string 4) \"|       |     |       |\")",
        "T",
    );
    says(
        &shared,
        &lisp,
        "(string= (line-string 5) \"| Apple |   3 |  1.20 |\")",
        "T",
    );
    lisp.eval("(org-table-delete-row)".into());
    says(
        &shared,
        &lisp,
        "(string= (line-string 4) \"| Apple |   3 |  1.20 |\")",
        "T",
    );
    says(&shared, &lisp, "(string= (line-string 6) \"tail\")", "T");

    // The last column stays, and so does the last row: a table with neither is
    // a run of rules with nothing to put point in, and there is no way back out
    // of one.
    lisp.eval("(dotimes (i 4) (org-table-delete-column))".into());
    wait_message(&shared, "the last column stays");
    says(&shared, &lisp, "(string= (line-string 2) \"| Price |\")", "T");
    lisp.eval("(dotimes (i 5) (org-table-delete-row))".into());
    wait_message(&shared, "the last row stays");
    says(&shared, &lisp, "(string= (line-string 2) \"| Price |\")", "T");

    // --- falling through ----------------------------------------------------
    //
    // The half that could quietly eat org's bindings. Each key is claimed for
    // the whole mode and has to hand itself on when point is not in a table,
    // because there is no way to decline a keystroke once a binding has taken
    // it.

    load(&shared, NOTES, 2);
    lisp.eval("(goto-char (line-start 6))".into()); // `tail`
    lisp.eval("(org-table-align)".into());
    wait_message(&shared, "not in a table");

    lisp.eval("(goto-char (line-start 1))".into()); // `* Fruit`
    lisp.eval("(org-table-return)".into());
    wait_message(&shared, "no link at point"); // `org-open-at-point`

    lisp.eval("(org-table-tab)".into());
    wait_message(&shared, "folded"); // `org-cycle`

    let _ = std::fs::remove_file(&init);
}
