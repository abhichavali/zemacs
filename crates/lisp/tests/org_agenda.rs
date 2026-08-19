//! `runtime/modes/org-agenda.lisp` — every unfinished thing, in one list.
//!
//! The agenda is a scanner and nothing else: the listing, its keymap and the
//! jump are `runtime/modes/xref.lisp`, which was built for `lsp-find-references`
//! and turns out to be the shape a list of headlines wants too. So what is worth
//! pinning is the *parsing* — which is where org's own rules are subtle enough
//! to get wrong quietly.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::path::PathBuf;
use std::sync::Arc;
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

/// Numbered, because this file asks the same shape of question repeatedly and an
/// un-numbered wait would be satisfied by an earlier answer.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, tag: usize, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"{tag}=~a\" {form}))"));
    let want = format!("{tag}={want}");
    wait(shared, &format!("{form} to answer {want:?}"), |ed| {
        ed.messages.iter().any(|m| *m == want).then_some(())
    });
}

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

fn runtime(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .expect("the runtime directory ships with the source")
}

#[test]
fn the_agenda_lists_what_is_unfinished_and_reads_orgs_own_rules() {
    let dir = std::env::temp_dir().join(format!("zemacs_agenda-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("notes.org");
    std::fs::write(
        &file,
        "* Project :work:\n\
         ** TODO Write the report :urgent:\n\
         ** DONE Ship it\n\
         ** TODO Review PRs\n\
         * A plain heading with no keyword\n\
         **bold at the start of a line**\n\
         * Home :home:\n\
         ** TODO Fix the sink\n\
         ** TODO [#A] Call the plumber\n\
         ** TODO Écrire le résumé :maison:\n",
    )
    .unwrap();

    let init = std::env::temp_dir().join(format!("zemacs_agenda_init-{}.lisp", std::process::id()));
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n\
             (load {:?} :verbose nil :print nil)\n\
             (load {:?} :verbose nil :print nil)\n\
             (message \"agenda test init loaded\")\n",
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
            .any(|m| m == "agenda test init loaded")
            .then_some(())
    });

    // --- what counts as a headline ------------------------------------------
    //
    // Stars *and then a space*. `**bold**` at the start of a line is emphasis,
    // and reading it as a level-two heading is the classic way to get a
    // scanner's answer full of nonsense.
    says(&shared, &lisp, 1, "(%org-agenda-heading \"**bold** here\")", "NIL");
    says(&shared, &lisp, 2, "(first (%org-agenda-heading \"** TODO Write it\"))", "2");
    says(&shared, &lisp, 3, "(second (%org-agenda-heading \"** TODO Write it\"))", "TODO");
    says(&shared, &lisp, 4, "(third (%org-agenda-heading \"** TODO Write it\"))", "Write it");
    // A word that is not in the vocabulary is part of the heading, not a
    // keyword — org's own rule, and what stops `Doneness of the proof` reading
    // as a finished task.
    says(&shared, &lisp, 5, "(second (%org-agenda-heading \"* Doneness of it\"))", "NIL");
    says(&shared, &lisp, 6, "(third (%org-agenda-heading \"* Doneness of it\"))", "Doneness of it");

    // --- tags ----------------------------------------------------------------
    says(&shared, &lisp, 7, "(%org-agenda-tags \"Write it :urgent:\")", "(urgent)");
    says(&shared, &lisp, 8, "(%org-agenda-tags \"Two of them :a:b:\")", "(a b)");
    says(&shared, &lisp, 9, "(%org-agenda-tags \"nothing here\")", "NIL");
    // The trap: a colon that ends a *word* is not a tag list, and one in the
    // middle of a sentence is not either.
    says(&shared, &lisp, 10, "(%org-agenda-tags \"Ship it:\")", "NIL");
    says(&shared, &lisp, 11, "(%org-agenda-tags \"See: below\")", "NIL");
    // Org's separator is `[ \t]`, and a file that came from Windows puts a
    // `#\Return` after the run — both used to read as "no tags at all", which
    // is the whole of a tagged file going quiet.
    says(&shared, &lisp, 12,
        "(%org-agenda-tags (format nil \"Write it~c:urgent:\" (code-char 9)))", "(urgent)");
    says(&shared, &lisp, 13,
        "(%org-agenda-tags (format nil \"Write it :urgent:~c\" (code-char 13)))", "(urgent)");

    // --- inheritance ----------------------------------------------------------
    //
    // Org's rule, and the one the tag filter is useless without: a tag on
    // `* Work` is on every task under it. The row that matters is the *task* —
    // the heading that spells the word is the one row nobody wants.
    let work = "(lambda (k tags h) (declare (ignore k h)) \
                 (member \"work\" tags :test #'string-equal))";
    says(&shared, &lisp, 14,
        &format!("(second (%org-agenda-scan \"f\" \
                  (format nil \"* Work :work:~%** TODO Write it~%\") {work}))"),
        "f:2:** Write it");
    // ...and it closes at the right place: a *sibling* of the tagged heading is
    // out from under it, and so is everything below that sibling.
    says(&shared, &lisp, 15,
        &format!("(length (%org-agenda-scan \"f\" \
                  (format nil \"* Work :work:~%* Home~%** TODO Fix the sink~%\") {work}))"),
        "1");
    // `#+FILETAGS:` is the same rule at level 0 — nothing can close it.
    says(&shared, &lisp, 16,
        &format!("(first (%org-agenda-scan \"f\" \
                  (format nil \"#+FILETAGS: :work:~%* TODO Write it~%\") {work}))"),
        "f:2:* Write it");

    // --- priority -------------------------------------------------------------
    //
    // Read from after the `PATH:LINE:` prefix, so a directory with a `[#` in its
    // name is not a priority; no cookie sorts as `B`, which is org's default and
    // what puts `[#C]` *below* the undecided majority rather than above it.
    says(&shared, &lisp, 17, "(%org-agenda-priority \"/tmp/[#z]/n.org:2:* [#A] Do it\")", "A");
    says(&shared, &lisp, 18, "(%org-agenda-priority \"/tmp/n.org:2:* Do it\")", "B");

    // --- and the list itself -------------------------------------------------

    // Named in `*org-agenda-files*` rather than opened: `find-file` is one of
    // the handful of commands that need the *app* layer — there is no
    // filesystem in core — and a headless test has nobody to drain that queue.
    // Which is also the path worth testing: an agenda file you have not opened
    // is read from disk, and that is how all but one of them ever are.
    // First, the two ways a config is wrong. One bare string rather than a list
    // is what everybody writes and used to be a type error inside `loop`; `~/`
    // is what the variable's own docstring writes and used to be read as a
    // directory called `~`. Both arrive here as "cannot read /home/.../...",
    // and the leading slash is the half that proves the expansion.
    let missing = format!("zemacs-agenda-nope-{}.org", std::process::id());
    lisp.eval(format!("(setf *org-agenda-files* \"~/{missing}\")"));
    lisp.eval("(org-todo-list)".into());
    wait(&shared, "the unreadable file to be named", |ed| {
        ed.messages
            .iter()
            .any(|m| m.starts_with("org: cannot read /") && m.ends_with(&missing))
            .then_some(())
    });

    lisp.eval(format!(
        "(setf *org-agenda-files* (list {:?}))",
        file.display().to_string()
    ));
    lisp.eval("(org-todo-list)".into());
    let text = wait(&shared, "the agenda", |ed| {
        (ed.buffer.name() == "*xref*").then(|| ed.buffer.text.to_string())
    });

    let rows: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(rows.len(), 5, "{rows:#?}");
    // Unfinished only: `DONE Ship it` is out, and so is every headline with no
    // keyword — a heading is not a task.
    assert!(rows.iter().all(|r| !r.contains("Ship it")), "{rows:#?}");
    assert!(rows.iter().all(|r| !r.contains("plain heading")), "{rows:#?}");
    assert!(rows.iter().any(|r| r.ends_with("** Write the report :urgent:")), "{rows:#?}");
    assert!(rows.iter().any(|r| r.ends_with("** Fix the sink")), "{rows:#?}");
    // The stars go back on, so the listing reads as an outline — depth is most
    // of what a headline means — and the row is still `PATH:LINE:TEXT`, which
    // is what `RET` in the listing parses.
    let first = rows[0];
    assert!(first.starts_with(&file.display().to_string()), "{first}");
    // `[#A]` was written last and is listed first. Everything else keeps the
    // order it was written in, which is what makes the sort *stable* rather
    // than merely sorted — so the second row is still the file's first task.
    assert!(first.ends_with("** [#A] Call the plumber"), "{first}");
    assert!(rows[1].contains(":2:"), "the line number rides along: {}", rows[1]);
    // Characters, not bytes, on both sides of the read: `file-length` counts the
    // file in bytes and `read-sequence` fills the string in characters, which is
    // why the reader trims to what it actually read. An accented heading is
    // where the two used to be confused, and it would show up as a truncated
    // row and a line number counted off the wrong text.
    let accented = format!("{}:10:** Écrire le résumé :maison:", file.display());
    assert!(rows.contains(&accented.as_str()), "{rows:#?}");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&init);
}
