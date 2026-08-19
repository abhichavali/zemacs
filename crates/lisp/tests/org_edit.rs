//! Whole org editing sessions, played end to end against the real image.
//!
//! Every other org file in this directory tests one module: `org_lists.rs` the
//! readers, `org_table.rs` the table, `org_modern.rs` the overlays, `fold.rs`
//! the folds. This one plays the sessions that cross between them — an outline
//! typed from nothing, a checkbox that moves a cookie two lines up, a table
//! keystroke landing on an outline key, a fold that is still a fold after the
//! heading under it changed level. Those are the bugs nobody owns, because each
//! module is right on its own and the seam between two of them belongs to
//! neither.
//!
//! Boots the real `runtime/init.lisp` rather than a hand-picked subset, because
//! load *order* is half of what is being checked: `org-table.lisp` displaces
//! `<tab>`, `<ret>` and `<backtab>` from `org-fold.lisp` and `org-structure.lisp`
//! and hands them back by hand, and only the shipped `*runtime-modules*` puts
//! the three files in the order where that works.
//!
//! Two of the sections here **measure rather than assert**, and say so where
//! they do. Undo granularity is the big one: `rs_replace_region` emits a
//! `Checkpoint` of its own and `Editor::checkpoint` only collapses two snapshots
//! when the text between them did not change, so a command built out of N
//! `replace-region' calls costs N presses of `u' — and there is no way from the
//! image to say "these edits are one edit". Nothing here asserts a number for
//! that; the counts are printed so the worst offenders can be ranked, and they
//! will change the day a grouping primitive lands.
//!
//! Deliberately one `#[test]`: `cl_boot` initialises a process-wide image, so
//! there is exactly one `spawn` per test binary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Mode, Shared};

const PATIENCE: Duration = Duration::from_secs(60);
static NTH: AtomicUsize = AtomicUsize::new(0);

/// The main loop's one job that matters here. Core records that a mode hook is
/// *due* and never calls Lisp itself; the app drains `pending_hooks` and asks
/// the image to run each one. There is no app in a test, so this is it — and it
/// is what puts the buffers below into `org-mode`. Copied from `org_table.rs`.
fn pump(shared: Shared, lisp: Arc<zemacs_lisp::Lisp>) {
    std::thread::spawn(move || {
        loop {
            let hooks = std::mem::take(&mut shared.lock().unwrap().pending_hooks);
            for hook in hooks {
                lisp.eval(format!(
                    "(let ((h (find-symbol {:?} :zemacs))) (when (and h (fboundp h)) (funcall h)))",
                    hook.to_uppercase()
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
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

/// Ask the image a question and read the answer back as a message.
///
/// Numbered **and** read only from the messages that arrived after the question
/// was asked. Either guard alone lets a probe pass vacuously — the log is
/// cumulative and this file asks `(line-number)` of a dozen different buffers —
/// and that has burned people here before.
fn ask(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str) -> String {
    let tag = NTH.fetch_add(1, Ordering::Relaxed);
    let from = shared.lock().unwrap().messages.len();
    lisp.eval(format!(
        "(message (format nil \"#{tag} ~a\" \
           (handler-case {form} (error (e) (format nil \"ERROR ~a\" e)))))"
    ));
    let prefix = format!("#{tag} ");
    let line = wait(shared, form, |ed| {
        ed.messages
            .get(from..)
            .and_then(|m| m.iter().find(|m| m.starts_with(&prefix)))
            .cloned()
    });
    line[prefix.len()..].to_string()
}

/// The buffer's text, once every form queued before this one has run.
///
/// The question is the barrier and is the whole reason this is not a bare
/// `lock()`: Lisp is not synchronous with this thread, so reading the rope
/// without one reads whatever was there before the command was dispatched.
fn text(shared: &Shared, lisp: &zemacs_lisp::Lisp) -> String {
    let _ = ask(shared, lisp, "(point)");
    shared.lock().unwrap().buffer.text.to_string()
}

/// Every fold overlay in the buffer, as `(start end)` pairs.
fn folds(shared: &Shared, lisp: &zemacs_lisp::Lisp) -> String {
    ask(
        shared,
        lisp,
        "(format nil \"~{~a~^ ~}\" \
           (mapcar (lambda (o) (list (overlay-start o) (overlay-end o))) \
                   (folds-in (point-min) (point-max))))",
    )
}

fn runtime(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .expect("the runtime directory ships with the source")
}

/// Put `body` in the live buffer as an org file, cursor at 0, Normal state.
///
/// A fresh path every time, for the reason `org_modern.rs` gives: `Editor::load`
/// switches to a buffer already holding that path rather than reloading it. The
/// pid is in the name because two `cargo test` runs at once used to collide.
fn load(shared: &Shared, body: &str, nth: usize) {
    let mut ed = shared.lock().unwrap();
    ed.load(
        body,
        Some(PathBuf::from(format!(
            "/tmp/zemacs_org_edit_{}_{nth}.org",
            std::process::id()
        ))),
        Some("org".into()),
    );
    ed.apply(EditorCommand::SetMode(Mode::Normal));
    ed.apply(EditorCommand::MoveTo(0));
}

/// How many presses of `u` it takes to get back to `before`. Answers `limit + 1`
/// when it is still not back — which reads as "more than anyone would press"
/// rather than hanging.
fn undo_steps(shared: &Shared, lisp: &zemacs_lisp::Lisp, before: &str, limit: usize) -> usize {
    for n in 0..=limit {
        if text(shared, lisp) == before {
            return n;
        }
        lisp.eval("(undo)".into());
    }
    limit + 1
}

#[test]
fn an_org_session_survives_its_own_features_meeting() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = Arc::new(zemacs_lisp::spawn(tx, shared.clone(), runtime("init.lisp")));
    pump(shared.clone(), lisp.clone());
    wait(&shared, "init.lisp to finish loading", |ed| {
        ed.messages
            .iter()
            .any(|m| m.contains("is driving the editor"))
            .then_some(())
    });

    // Every count this file measures rather than asserts, collected here so the
    // report is one table rather than a dozen lines scattered through the log.
    let mut undos: Vec<(&str, usize)> = Vec::new();

    // ========================================================================
    // 1. An outline written from nothing
    //
    // The session everyone has: a heading, a sibling, one of them a level
    // deeper, a paragraph under it, a TODO, tick it, untick it. Asserted on the
    // buffer text after every keystroke rather than on a reader's answer,
    // because the readers already have `org_lists.rs` and what goes wrong here
    // is where the *edit* lands.
    // ========================================================================

    load(&shared, "", 0);
    assert_eq!(ask(&shared, &lisp, "(major-mode)"), "org-mode");

    lisp.eval("(org-meta-return)".into());
    // On the blank line rather than under it — an empty buffer's one line *is*
    // where the heading goes, and opening a line first would leave a stray blank
    // above every outline ever started this way.
    assert_eq!(text(&shared, &lisp), "* ");
    assert_eq!(ask(&shared, &lisp, "(evil-state)"), "insert");

    lisp.eval("(insert \"Alpha\")".into());
    lisp.eval("(org-meta-return)".into());
    lisp.eval("(insert \"Beta\")".into());
    assert_eq!(text(&shared, &lisp), "* Alpha\n* Beta");

    lisp.eval("(org-do-demote)".into());
    assert_eq!(text(&shared, &lisp), "* Alpha\n** Beta");

    // Body text, then `C-c C-t` *from the body* — which is where a hand
    // actually is when it decides the entry is a task.
    lisp.eval("(goto-char (point-max))".into());
    lisp.eval("(insert (format nil \"~%some body\"))".into());
    lisp.eval("(org-todo)".into());
    assert_eq!(text(&shared, &lisp), "* Alpha\n** TODO Beta\nsome body");
    lisp.eval("(org-todo)".into());
    assert_eq!(text(&shared, &lisp), "* Alpha\n** DONE Beta\nsome body");
    // ...and off the end of the cycle, back to a heading with no state at all
    // and no trailing space where the keyword was.
    lisp.eval("(org-todo)".into());
    assert_eq!(text(&shared, &lisp), "* Alpha\n** Beta\nsome body");

    // Where point ends up after a demote, which used to be a finding rather
    // than an assertion: `%org-keeping-point' restores a line and a *column*,
    // and adding a star makes the line one character longer — so point kept its
    // column and lost its character, and `M-<right>' while typing the end of a
    // heading put the next letter before the last one. `%org-restar-point'
    // moves it by the star instead, so the *character* is what survives. Both
    // are read, because keeping the column is what the wrong answer looks like.
    load(&shared, "* Beta\n", 1);
    lisp.eval("(goto-char (+ (line-start 1) 6))".into());
    let col_before = ask(&shared, &lisp, "(- (point) (line-start))");
    let char_before = ask(&shared, &lisp, "(subseq (line-string) (1- (- (point) (line-start))))");
    lisp.eval("(org-do-demote)".into());
    let col_after = ask(&shared, &lisp, "(- (point) (line-start))");
    let char_after = ask(&shared, &lisp, "(subseq (line-string) (1- (- (point) (line-start))))");
    assert_eq!(char_after, char_before, "the character before point survives a demote");
    assert_eq!(
        (col_before.as_str(), col_after.as_str()),
        // 5 and not 6: Normal state may not sit past the last character of a
        // line, so the `goto-char' above is clamped onto the `a' of `Beta'.
        ("5", "6"),
        "...by moving with the star rather than staying in its column"
    );

    // ========================================================================
    // 2. A list with checkboxes, and the cookie two lines up
    // ========================================================================

    load(&shared, "* Tasks [/]\n- [ ] milk\n- [ ] eggs\n", 2);
    lisp.eval("(goto-char (line-start 1))".into());
    // `C-c C-c' on a heading with no box of its own is the recount, which is
    // what fills an author's `[/]' in.
    lisp.eval("(org-ctrl-c-ctrl-c)".into());
    assert_eq!(
        text(&shared, &lisp),
        "* Tasks [0/2]\n- [ ] milk\n- [ ] eggs\n"
    );

    let before = text(&shared, &lisp);
    lisp.eval("(goto-char (+ (line-start 2) 6))".into());
    lisp.eval("(org-toggle-checkbox)".into());
    assert_eq!(
        text(&shared, &lisp),
        "* Tasks [1/2]\n- [X] milk\n- [ ] eggs\n"
    );
    // Point stayed on the word it was on, which is the whole reason
    // `%org-keeping-point' exists: the cookie pass rewrites the heading *above*
    // this line, and `replace-region' leaves point wherever it last wrote.
    assert_eq!(ask(&shared, &lisp, "(line-number)"), "2");
    undos.push((
        "C-c C-c ticking a box under a cookie",
        undo_steps(&shared, &lisp, &before, 6),
    ));

    // ...and back, which is the half that proves the cookie is recounted rather
    // than incremented.
    load(&shared, "* Tasks [1/2]\n- [X] milk\n- [ ] eggs\n", 3);
    lisp.eval("(goto-char (+ (line-start 2) 6))".into());
    lisp.eval("(org-toggle-checkbox)".into());
    assert_eq!(
        text(&shared, &lisp),
        "* Tasks [0/2]\n- [ ] milk\n- [ ] eggs\n"
    );

    load(&shared, "* Tasks [1/2]\n- [X] milk\n- [ ] eggs\n", 4);
    let before = text(&shared, &lisp);
    lisp.eval("(goto-char (+ (line-start 2) 6))".into());
    lisp.eval("(org-meta-return)".into());
    assert_eq!(
        text(&shared, &lisp),
        "* Tasks [1/3]\n- [X] milk\n- [ ] \n- [ ] eggs\n",
        "a new item under a ticked one carries the bullet and an empty box, and \
         the cookie above it counts the box that just arrived"
    );
    undos.push((
        "M-RET under a checkbox item with a cookie above",
        undo_steps(&shared, &lisp, &before, 6),
    ));

    // A nested sublist. org counts the *top level* of the list a cookie heads;
    // this counts every box in the subtree, so a sub-item is a third of the
    // parent's progress rather than part of its own item's.
    load(
        &shared,
        "* Tasks [/]\n- [ ] top\n  - [X] deep one\n  - [ ] deep two\n",
        5,
    );
    lisp.eval("(goto-char (line-start 1))".into());
    lisp.eval("(org-update-statistics-cookies)".into());
    eprintln!(
        "FINDING a cookie over a nested list counts every box in the subtree: {:?} \
         (org counts the top-level items alone, so this is `[0/1]' there)",
        text(&shared, &lisp).lines().next().unwrap_or("")
    );

    // Plain M-RET down a prose outline, for the baseline: no box, no cookie
    // pass, and the cheapest shape this key has.
    load(&shared, "* One\n", 6);
    let before = text(&shared, &lisp);
    lisp.eval("(goto-char (line-start 1))".into());
    lisp.eval("(org-meta-return)".into());
    undos.push(("M-RET with no checkbox", undo_steps(&shared, &lisp, &before, 6)));

    // ========================================================================
    // 3. Restructuring — one keystroke, several lines
    // ========================================================================

    load(&shared, "* One\n** Two\n*** Three\n** Four\ntail\n", 7);
    let before = text(&shared, &lisp);
    lisp.eval("(goto-char (line-start 1))".into());
    lisp.eval("(org-demote-subtree)".into());
    assert_eq!(
        text(&shared, &lisp),
        "** One\n*** Two\n**** Three\n*** Four\ntail\n",
        "every headline moves by the same delta, and the prose does not move at all"
    );
    undos.push((
        "M-S-<right> over a subtree of four headlines",
        undo_steps(&shared, &lisp, &before, 12),
    ));

    load(&shared, "* One\ntail\n", 8);
    let before = text(&shared, &lisp);
    lisp.eval("(goto-char (line-start 1))".into());
    lisp.eval("(org-do-demote)".into());
    undos.push((
        "M-<right> on one headline",
        undo_steps(&shared, &lisp, &before, 6),
    ));

    load(&shared, "* Tasks [0/1]\n- [ ] milk\n", 9);
    let before = text(&shared, &lisp);
    lisp.eval("(goto-char (line-start 1))".into());
    lisp.eval("(org-todo)".into());
    undos.push((
        "C-c C-t on a heading whose cookie is unaffected",
        undo_steps(&shared, &lisp, &before, 6),
    ));

    // ...and the same key where the keyword *is* the box: cycling to DONE ticks
    // it, which moves the cookie above, which is a second edit.
    load(&shared, "* Tasks [0/1]\n** TODO [ ] ship\n", 30);
    let before = text(&shared, &lisp);
    lisp.eval("(goto-char (line-start 2))".into());
    lisp.eval("(org-todo)".into());
    assert_eq!(
        text(&shared, &lisp),
        "* Tasks [1/1]\n** DONE [X] ship\n",
        "the keyword and the box are one state, and the cookie above counts it"
    );
    undos.push((
        "C-c C-t where DONE ticks a box a cookie counts",
        undo_steps(&shared, &lisp, &before, 6),
    ));

    // Two cookies moving at once, which is what a recount over a whole file
    // costs: one `replace-region' apiece.
    load(
        &shared,
        "* A [0/1]\n- [X] one\n* B [0/1]\n- [X] two\n",
        31,
    );
    let before = text(&shared, &lisp);
    lisp.eval("(goto-char (line-start 1))".into());
    lisp.eval("(org-update-statistics-cookies)".into());
    assert_eq!(text(&shared, &lisp), "* A [1/1]\n- [X] one\n* B [1/1]\n- [X] two\n");
    undos.push((
        "a recount that moves two cookies",
        undo_steps(&shared, &lisp, &before, 6),
    ));

    // ========================================================================
    // 4. Fold, then edit
    //
    // The seam with the sharpest edge on it. A fold is an overlay over a *range*
    // and every outline command changes what the range is; a cursor may not sit
    // on a line a fold hides, and `clamp_cursor' silently walks it back to the
    // fold's head at the end of every `apply'. Neither file knows about the
    // other.
    // ========================================================================

    load(&shared, "* One\nbody one\n** Two\nbody two\n* Three\n", 10);
    lisp.eval("(goto-char (line-start 1))".into());
    lisp.eval("(org-cycle)".into());
    assert_eq!(folds(&shared, &lisp), "(0 30)", "TAB closes the whole subtree");

    // A fold is a range and the level is what decides that range, so a demote
    // used to leave the fold reaching past the subtree it belonged to: `** Two'
    // and its body ended up hidden under `** One''s fold with no headline of
    // their own drawn — invisible, and reachable only by pressing TAB again.
    // The reheading opens the fold it invalidates, so there is none left to be
    // wrong.
    lisp.eval("(org-do-demote)".into());
    assert_eq!(folds(&shared, &lisp), "", "a demote opens the fold it invalidates");
    assert_eq!(
        ask(&shared, &lisp, "(%org-subtree-end 1)"),
        "15",
        "...and the subtree it left behind is the shorter one"
    );
    // TAB still works from there, which is the half that says the fold was
    // opened rather than merely forgotten about.
    lisp.eval("(org-cycle)".into());
    assert_eq!(folds(&shared, &lisp), "(0 15)", "and TAB folds the new extent");
    // Two presses and not three: the demote left `** One' with `body one' and
    // no child headline, and the CHILDREN state of the cycle is a fold per
    // child. With none to make, there is nothing between closed and open.
    lisp.eval("(org-cycle)".into());
    assert_eq!(folds(&shared, &lisp), "", "TAB always finds its way back to open");

    // Typing at a closed fold, which is the one that used to lose your
    // keystrokes and is worth a pin rather than a probe. `%org-open-line-below'
    // opens the new line *inside* the fold; a cursor may not sit on a hidden
    // line, so `clamp_cursor' dragged point back to the fold's head and every
    // character after it landed in front of the heading you started from
    // (`New* One\n* \nbody one...'). Both keys are that one function, so both
    // were wrong and both are fixed by it opening the fold first.
    for (nth, key, want) in [
        (11, "(org-meta-return)", "* One\n* New\nbody one\n* Two\n"),
        (12, "(org-insert-todo-heading)", "* One\n* TODO New\nbody one\n* Two\n"),
    ] {
        load(&shared, "* One\nbody one\n* Two\n", nth);
        lisp.eval("(goto-char (line-start 1))".into());
        lisp.eval("(org-cycle)".into());
        lisp.eval(key.into());
        lisp.eval("(insert \"New\")".into());
        assert_eq!(text(&shared, &lisp), want, "{key} on a closed heading");
    }

    // ========================================================================
    // 5. Markup while typing
    //
    // Type `*bold*', put the cursor back in it, take the closing marker off,
    // put it back. The overlay has to retire when the run stops being a run and
    // come back computed from the text rather than restored from a copy — and
    // no real character may be left hidden at any point.
    // ========================================================================

    load(&shared, "Some *bold* text.\n", 13);
    lisp.eval("(org-mode-hook)".into());
    lisp.eval("(goto-char (point-max))".into());
    lisp.eval("(org-modern-refresh)".into());
    let shown = |shared: &Shared, lisp: &zemacs_lisp::Lisp| {
        ask(
            shared,
            lisp,
            "(format nil \"~{~a~^ ~}\" \
               (mapcar (lambda (o) (list (overlay-start o) (overlay-end o) \
                                         (overlay-get o 'display))) \
                       (%org-modern-overlays)))",
        )
    };
    assert_eq!(shown(&shared, &lisp), "(5 11 bold)");

    lisp.eval("(delete-region 10 11)".into());
    lisp.eval("(after-change-hook)".into());
    assert_eq!(text(&shared, &lisp), "Some *bold text.\n");
    // Nothing drawn over a run that is no longer one — an overlay left here
    // would be hiding a `*' the author is in the middle of retyping.
    assert_eq!(
        shown(&shared, &lisp),
        "",
        "half an emphasis run draws nothing"
    );

    lisp.eval("(insert-at 10 \"*\")".into());
    lisp.eval("(after-change-hook)".into());
    assert_eq!(text(&shared, &lisp), "Some *bold* text.\n");
    // Point is inside the run after the insert, so it is revealed, not drawn —
    // which is org-appear's whole job and is what makes editing inside one safe.
    assert_eq!(shown(&shared, &lisp), "(5 11 NIL)");
    lisp.eval("(goto-char (point-max))".into());
    lisp.eval("(org-modern-appear)".into());
    assert_eq!(
        shown(&shared, &lisp),
        "(5 11 bold)",
        "and the glyph comes back recomputed once the cursor leaves"
    );

    // ========================================================================
    // 6. A link
    // ========================================================================

    load(
        &shared,
        "* Contents\n[[*Vectors][the first unit]]\n* Vectors\nbody\n",
        14,
    );
    lisp.eval("(goto-char (+ (line-start 2) 4))".into());
    lisp.eval("(org-open-at-point)".into());
    assert_eq!(ask(&shared, &lisp, "(line-number)"), "3", "`[[*Heading]]' lands on it");

    // Edit the description: the target is the other half and must still resolve.
    lisp.eval(
        "(replace-region (+ (line-start 2) 12) (+ (line-start 2) 26) \"unit one\")".into(),
    );
    assert_eq!(
        text(&shared, &lisp),
        "* Contents\n[[*Vectors][unit one]]\n* Vectors\nbody\n"
    );
    lisp.eval("(goto-char (+ (line-start 2) 4))".into());
    lisp.eval("(org-open-at-point)".into());
    assert_eq!(ask(&shared, &lisp, "(line-number)"), "3");

    // ...and through the key that is actually bound, which in an org buffer is
    // the *table* dispatcher and only reaches the link by falling through.
    lisp.eval("(goto-char (+ (line-start 2) 4))".into());
    lisp.eval("(org-table-return)".into());
    assert_eq!(ask(&shared, &lisp, "(line-number)"), "3");

    // A link inside a table cell is the one place the fall-through cannot help.
    // `org-table-return' asks `%org-table-bounds' first and a table row always
    // wins, so the link is there, readable, and unreachable from the keyboard.
    // Asserted on the *structure* rather than on where RET landed, because the
    // where depends on the table module being well (see below).
    load(
        &shared,
        "* Contents\n| unit | link |\n| one  | [[*Vectors][v]] |\n* Vectors\n",
        15,
    );
    lisp.eval("(goto-char (+ (line-start 3) 10))".into());
    assert_eq!(
        ask(&shared, &lisp, "(if (%org-link-at-point) t nil)"),
        "T",
        "there is a link under point"
    );
    assert_eq!(
        ask(&shared, &lisp, "(if (%org-table-bounds) t nil)"),
        "T",
        "...and a table under it, which is what `org-table-return' asks first"
    );
    // A link inside a table cell cannot be followed with `<ret>' — that key is
    // `org-table-return', which sees a table and never reaches
    // `org-open-at-point'. Not a bug so much as a key that is spoken for, and
    // there is a way out: `SPC m o' runs the opener directly, from anywhere in
    // an org buffer. Pinned, because it is the *only* way out of a cell.
    assert_eq!(
        ask(
            &shared,
            &lisp,
            "(format nil \"~{~a~^ ~}\" \
               (sort (loop for (mode keys cmd) in (key-bindings) \
                           when (and (string= mode \"org-mode\") (string= keys \"SPC m o\")) \
                             collect (format nil \"~a\" cmd)) #'string<))"
        ),
        "org-open-at-point"
    );

    // ========================================================================
    // 7. A table, typed and walked
    // ========================================================================

    load(&shared, "|a|b|\n", 16);
    lisp.eval("(goto-char 1)".into());
    lisp.eval("(org-table-tab)".into());
    assert_eq!(
        text(&shared, &lisp),
        "| a | b |\n",
        "the first TAB lays out what you typed"
    );
    lisp.eval("(org-table-tab)".into());
    lisp.eval("(org-table-tab)".into());
    assert_eq!(
        text(&shared, &lisp),
        "| a | b |\n|   |   |\n",
        "TAB off the last cell grows a row, which is how a table is written"
    );

    // An accented cell and a Japanese one. Both disagree with a *byte* count;
    // only the second also disagrees with a screen column, and the two have to
    // be laid out by different measures for the bars to land in one place.
    load(
        &shared,
        "| Name | Qty |\n|---|---|\n| café | 3 |\n| ab | 12 |\n",
        17,
    );
    lisp.eval("(goto-char (+ (line-start 3) 2))".into());
    lisp.eval("(org-table-align)".into());
    assert_eq!(
        text(&shared, &lisp),
        "| Name | Qty |\n|------+-----|\n| café |   3 |\n| ab   |  12 |\n"
    );

    load(
        &shared,
        "| Name | Qty |\n|---|---|\n| 日本語 | 3 |\n| ab | 12 |\n",
        18,
    );
    lisp.eval("(goto-char (+ (line-start 3) 2))".into());
    lisp.eval("(org-table-align)".into());
    // Asserted as "every row is the same width *on screen*" rather than as the
    // exact string, because that is the claim — `日本語' is three characters and
    // six columns, so a layout that lines up cannot be one that pads by
    // `length'. `char-cells' is the renderer's own measure, which is what makes
    // this the same question the eye asks.
    assert_eq!(
        ask(
            &shared,
            &lisp,
            "(let ((w (loop for l from 1 to 4 \
                            collect (loop for c across (line-string l) sum (char-cells c))))) \
               (if (apply #'= w) \"even\" (format nil \"ragged ~a\" w)))"
        ),
        "even",
        "a table with a CJK cell still lines up:\n{}",
        text(&shared, &lisp)
    );
    // ...and the identity the whole file rests on, which holds either way:
    // `line-string' answers characters and `replace-region' takes them, so an
    // index into a line is an index into the buffer.
    assert_eq!(
        ask(&shared, &lisp, "(length (line-string 3))"),
        ask(&shared, &lisp, "(- (line-end 3) (line-start 3))"),
        "a line's length in characters is its length in offsets"
    );

    load(&shared, "* Fruit\n| Name | Qty |\n|---|---|\n| Pear | 12 |\ntail\n", 19);
    let before = text(&shared, &lisp);
    lisp.eval("(goto-char (+ (line-start 4) 3))".into());
    lisp.eval("(org-table-align)".into());
    // The counter-example that proves the measurement is measuring something
    // real: `org-table.lisp' funnels every command through one
    // `replace-region', so a table command *is* one press of `u'.
    let n = undo_steps(&shared, &lisp, &before, 6);
    undos.push(("org-table-align (one replace-region by design)", n));
    assert_eq!(n, 1, "a table command is one undo step");

    // M-RET inside a table. Every reader in `org-structure.lisp' says "not a
    // heading and not an item", so the fallback writes a headline — into the
    // middle of the table, which cuts it in two.
    load(&shared, "* Fruit\n| Name | Qty |\n|------+-----|\n| Pear |  12 |\ntail\n", 20);
    lisp.eval("(goto-char (+ (line-start 4) 3))".into());
    lisp.eval("(org-meta-return)".into());
    let after = text(&shared, &lisp);
    if after.contains("|  12 |\n* ") {
        eprintln!(
            "FINDING M-RET inside a table writes a headline into it:\n{after}\
             — `org-ctrl-c-ctrl-c' already opens by asking `%org-table-bounds' \
             and `org-meta-return' does not."
        );
    }
    load(&shared, "* Fruit\n| Name | Qty |\n|------+-----|\n| Pear |  12 |\ntail\n", 21);
    lisp.eval("(goto-char (+ (line-start 4) 3))".into());
    lisp.eval("(org-insert-todo-heading)".into());
    let after = text(&shared, &lisp);
    if after.contains("|  12 |\n* TODO") {
        eprintln!("FINDING M-S-RET inside a table does the same:\n{after}");
    }

    // ========================================================================
    // 8. Non-ASCII throughout
    //
    // `line-string' answers characters and `replace-region' takes them, so an
    // index into a line is an index into the buffer. Everything below is that
    // claim, applied where a byte count and a character count differ.
    // ========================================================================

    load(&shared, "* Café ☕\n- [ ] naïve\n* 日本\n", 22);
    assert_eq!(
        ask(&shared, &lisp, "(length (line-string 1))"),
        ask(&shared, &lisp, "(- (line-end 1) (line-start 1))")
    );
    assert_eq!(ask(&shared, &lisp, "(%org-line-level (line-string 1))"), "1");

    lisp.eval("(goto-char (line-start 1))".into());
    lisp.eval("(org-do-demote)".into());
    assert_eq!(text(&shared, &lisp), "** Café ☕\n- [ ] naïve\n* 日本\n");
    lisp.eval("(goto-char (+ (line-start 2) 6))".into());
    lisp.eval("(org-toggle-checkbox)".into());
    assert_eq!(text(&shared, &lisp), "** Café ☕\n- [X] naïve\n* 日本\n");

    // A link whose target *and* description are non-ASCII, read from a column
    // that is past several multi-byte characters.
    load(&shared, "Prose with a [[https://x/é][café]] link.\n", 23);
    lisp.eval("(goto-char (+ (line-start 1) 15))".into());
    assert_eq!(
        ask(&shared, &lisp, "(%org-link-at-point)"),
        "(https://x/é . café)"
    );

    // An emoji in a list item: one character, two cells, and the cookie above it
    // must not care.
    load(&shared, "* Tasks [/]\n- [ ] 🎉 party\n- [X] ☕ coffee\n", 24);
    lisp.eval("(goto-char (line-start 1))".into());
    lisp.eval("(org-ctrl-c-ctrl-c)".into());
    lisp.eval("(goto-char (+ (line-start 2) 6))".into());
    lisp.eval("(org-toggle-checkbox)".into());
    assert_eq!(
        text(&shared, &lisp),
        "* Tasks [2/2]\n- [X] 🎉 party\n- [X] ☕ coffee\n"
    );

    // ========================================================================
    // 9. The mundane edges
    // ========================================================================

    // A file with no trailing newline, point at the very end.
    load(&shared, "* Only line, no newline", 25);
    lisp.eval("(goto-char (point-max))".into());
    lisp.eval("(org-meta-return)".into());
    assert_eq!(text(&shared, &lisp), "* Only line, no newline\n* ");

    // Point at `point-max' in a file that *does* end in a newline is on the
    // blank line the rope counts and vim does not. Reported rather than
    // asserted: the blank-line branch is new and which answer is right here —
    // another bullet, or a heading because the line you are on is empty — is a
    // judgement the owner of that file should make, not this one.
    load(&shared, "- item\n", 26);
    lisp.eval("(goto-char (point-max))".into());
    lisp.eval("(org-meta-return)".into());
    eprintln!(
        "NOTE M-RET with point at point-max after `- item\\n' gives {:?} \
         (the blank last line is not a list item, so the bullet is not carried)",
        text(&shared, &lisp)
    );

    // A very long line, which is where an accidental O(n^2) or a clamp would
    // show. 4000 characters is a pasted paragraph, not a pathology.
    let long = format!("* {}\n", "x".repeat(4000));
    load(&shared, &long, 27);
    lisp.eval("(goto-char (line-start 1))".into());
    lisp.eval("(org-do-demote)".into());
    assert_eq!(ask(&shared, &lisp, "(%org-line-level (line-string 1))"), "2");
    assert_eq!(ask(&shared, &lisp, "(length (line-string 1))"), "4003");

    // A buffer that is one blank line and nothing else: every reader has to
    // answer NIL and the fallback has to be a top-level heading.
    load(&shared, "\n", 28);
    lisp.eval("(goto-char 0)".into());
    lisp.eval("(org-meta-return)".into());
    assert_eq!(text(&shared, &lisp), "* \n");

    // ========================================================================
    // 10. The keys that are bound twice
    //
    // Three keys are claimed by two files each and the winner has to be the
    // dispatcher, because there is no way to decline a keystroke once a binding
    // has swallowed it. Only `org-mode' is checked: `org-frozen-mode' inherits
    // and is somebody else's to declare.
    // ========================================================================

    let bound = |shared: &Shared, lisp: &zemacs_lisp::Lisp, key: &str| {
        ask(
            shared,
            lisp,
            &format!(
                "(format nil \"~{{~a~^ ~}}\" \
                   (sort (loop for (mode keys cmd) in (key-bindings) \
                               when (and (string= mode \"org-mode\") (string= keys {key:?})) \
                                 collect (format nil \"~a\" cmd)) #'string<))"
            ),
        )
    };
    assert_eq!(bound(&shared, &lisp, "<tab>"), "org-table-tab");
    assert_eq!(bound(&shared, &lisp, "<backtab>"), "org-table-backtab");
    assert_eq!(bound(&shared, &lisp, "<ret>"), "org-table-return");
    assert_eq!(bound(&shared, &lisp, "M-<ret>"), "org-meta-return");
    // ...and the insert-state half, which is the load-bearing one: a mode keymap
    // is never consulted from Insert, so `M-RET' bound for `org-mode' alone would
    // be dead exactly while you are typing the list.
    assert_eq!(
        ask(
            &shared,
            &lisp,
            "(format nil \"~{~a~^ ~}\" \
               (sort (loop for (mode keys cmd) in (key-bindings) \
                           when (and (string= mode \"insert\") (string= keys \"M-<ret>\")) \
                             collect (format nil \"~a\" cmd)) #'string<))"
        ),
        "org-meta-return"
    );

    // Each dispatcher falls through when point is not in a table, which is the
    // half that could quietly eat org's own bindings.
    load(&shared, "* One\nbody\n* Two\n", 29);
    lisp.eval("(goto-char (line-start 1))".into());
    lisp.eval("(org-table-tab)".into());
    assert_eq!(folds(&shared, &lisp), "(0 10)", "<tab> off a table still cycles");
    lisp.eval("(org-table-backtab)".into());
    assert_eq!(
        folds(&shared, &lisp),
        "",
        "<backtab> off a table is still the global cycle"
    );

    // ========================================================================
    // The measurement
    // ========================================================================

    // One `u' per command, and it is asserted rather than reported because it
    // used to be four: `rs_replace_region' checkpoints per call, so a command
    // that wrote N times was N steps and walked back through documents nobody
    // ever wrote — one press after ticking a box left `[0/2]' over an `[X]'.
    // `with-undo-group' (library.lisp) takes the checkpoint once for the whole
    // command; this is the list of everything that needed it, so a command that
    // grows a second edit and forgets the wrapper shows up here.
    eprintln!("\n--- presses of `u' to take one org command back ---");
    for (what, n) in &undos {
        eprintln!("  {n}  {what}");
    }
    let noisy: Vec<_> = undos.iter().filter(|(_, n)| *n != 1).collect();
    assert!(noisy.is_empty(), "these take more than one `u`: {noisy:?}");
}
