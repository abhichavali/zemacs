//! Headless proof of `runtime/modes/org-modern.lisp` — org's markup drawn
//! instead of typed, and revealed again under the cursor.
//!
//! The feature is entirely Lisp over `make-overlay` and the `display` property,
//! so the only honest way to check it is to boot the real image against a real
//! [`Editor`] and read back the overlays the editor actually holds. That is
//! also the only way to prove the *glyphs* survive: `◉` is a non-ASCII literal
//! in a `load`ed file, which arrives in the image as an extended string and has
//! to be UTF-8 encoded on its way through the shim.
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
            panic!(
                "timed out waiting for {what}; status={:?} overlays={:?} last={seen:#?}",
                ed.status,
                displays(&ed),
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_message(shared: &Shared, what: &str, pred: impl Fn(&str) -> bool) {
    wait(shared, what, |ed| {
        ed.messages.iter().any(|m| pred(m)).then_some(())
    });
}

/// Evaluate `form` and wait for its value, printed with `~a`, to be `want`.
///
/// Numbered, because the log is cumulative: without the counter a second
/// `NIL` would be satisfied by the first one and the wait would prove nothing.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    let tag = NTH.fetch_add(1, Ordering::Relaxed);
    lisp.eval(format!("(message (format nil \"#{tag} ~a\" {form}))"));
    let want = format!("#{tag} {want}");
    wait_message(shared, form, |m| m == want);
}

/// Every overlay the editor holds, as `(start, end, display)`, in the order the
/// renderer resolves them.
///
/// Takes the `Editor` rather than the `Shared`: `wait` runs its predicate with
/// the lock held, and a second `lock()` in there is a deadlock, not a wait.
/// The overlays that *substitute* — the ones carrying a display string.
///
/// Filtered, because org-modern also makes overlays that only claim an
/// attribute: a heading's text takes `bold` and nothing else, since weight is a
/// per-run property and the heading's own overlay covers only the stars it
/// replaces with a bullet. Those are not substitutions and counting them here
/// would make "one glyph per substitution" a statement about something else.
fn displays(ed: &Editor) -> Vec<(usize, usize, Option<String>)> {
    ed.buffer
        .overlays()
        .iter()
        .filter(|o| o.display.is_some())
        .map(|o| (o.start, o.end, o.display.clone()))
        .collect()
}

/// True when some overlay covering `at` claims bold.
fn bold_at(ed: &Editor, at: usize) -> bool {
    ed.buffer
        .overlays()
        .iter()
        .any(|o| o.start <= at && at < o.end && o.bold == Some(true))
}

/// The display string of the one overlay starting at `at`, or `None`.
fn display_at(ed: &Editor, at: usize) -> Option<String> {
    ed.buffer
        .overlays()
        .iter()
        .find(|o| o.start == at)
        .and_then(|o| o.display.clone())
}

fn runtime(file: &str) -> String {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime").join(file))
        .unwrap_or_else(|e| panic!("runtime/{file} must exist: {e}"))
        .display()
        .to_string()
}

/// Put `text` in the live buffer as an org file, cursor at 0.
///
/// A fresh path every time, for the reason `lisp_mode.rs` gives: `Editor::load`
/// switches to a buffer already holding that path rather than reloading it.
fn load(shared: &Shared, text: &str, nth: usize) {
    let mut ed = shared.lock().unwrap();
    ed.load(
        text,
        Some(PathBuf::from(format!("/tmp/zemacs_test_modern_{nth}.org"))),
        Some("org".into()),
    );
    ed.apply(EditorCommand::SetMode(Mode::Normal));
    ed.apply(EditorCommand::MoveTo(0));
}

/// A buffer holding one of everything org-modern substitutes, plus three things
/// it must leave alone: a bullet inside a source block, arithmetic that looks
/// like emphasis, and a URL whose slashes look like italics.
const NOTES: &str = "\
* Alpha
Some *bold* text and a [[https://x/a/b][link]].
** Beta
- item one
- [X] done
#+begin_src sh
- not a bullet
#+end_src
2 * 3 is not bold.
";

#[test]
fn org_markup_is_drawn_as_glyphs_and_revealed_under_the_cursor() {
    let init = std::env::temp_dir().join("zemacs_test_org_modern_init.lisp");
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(load {:?} :verbose nil :print nil)
(message "org-modern test init loaded")
"#,
            runtime("modes/modes.lisp"),
            runtime("modes/org-modern.lisp"),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);

    wait_message(&shared, "the runtime files to load", |m| {
        m == "org-modern test init loaded"
    });
    let seen = shared.lock().unwrap().messages.clone();
    assert!(
        !seen.iter().any(|m| m.contains("error")),
        "org-modern.lisp must load cleanly; got {seen:#?}"
    );

    load(&shared, NOTES, 0);

    // --- turning it on -------------------------------------------------------
    //
    // Through the mode hook rather than by calling the toggle, because the hook
    // is what the editor calls and the auto-enable is the part worth proving:
    // `org-modern.lisp` re-declares `org-mode` with a body, so opening a `.org`
    // file is the whole installation.
    //
    // Point goes to the end first, onto the empty last line. Left at 0 it would
    // be *inside* the first heading's overlay, and org-appear would rightly
    // reveal it — which is a different assertion from this one.
    lisp.eval("(progn (goto-char (point-max)) (org-mode-hook))".into());
    says(&shared, &lisp, "(if (minor-mode-p 'org-modern) t nil)", "T");

    // One overlay per substitution, and not one more: the `- ` inside
    // `#+begin_src` is a shell flag, `2 * 3` is arithmetic, and the `//` in the
    // URL is a path separator. All three are inside the range that was scanned.
    let ovs = wait(&shared, "the substitutions", |ed| {
        let d = displays(ed);
        (d.len() == 7).then_some(d)
    });

    let want: Vec<&str> = vec![
        "◉",    // `*` — level 1: no padding
        "bold", // `*bold*` — markers gone, one overlay over the whole run
        "link", // `[[https://x/a/b][link]]` — the description alone
        " ○",   // `**` — level 2: one space, so the heading text does not move
        "•",    // `- item one`
        "•",    // `- [X] done`
        "☑",    // `[X]`
    ];
    let got: Vec<String> = ovs.iter().map(|o| o.2.clone().unwrap_or_default()).collect();
    assert_eq!(got, want, "one glyph per substitution: {ovs:#?}");

    // A heading's *text* is bold, and its stars are not the thing that carries
    // that. `bold` is a per-run attribute, so claiming it on the overlay that
    // replaces the stars would embolden exactly the bullet glyph; it needs the
    // range after them. Checked one character into the heading text, which is
    // inside that range and outside the bullet's.
    {
        let ed = shared.lock().unwrap();
        let heading_text = ovs[0].1 + 1; // just past `*` on the level-1 heading
        assert!(bold_at(&ed, heading_text), "a heading's text is bold");
        assert!(
            !bold_at(&ed, ed.buffer.len_chars() - 1),
            "and ordinary prose is not"
        );
    }

    // A level-2 heading substitutes exactly as many cells as it covers, which
    // is what keeps `Beta` in the column it was typed in.
    let (start, end, _) = ovs[3];
    assert_eq!(end - start, 2, "the level-2 overlay covers both stars");

    // --- org-appear ----------------------------------------------------------
    //
    // The cursor inside a run means its markup is real again: `display` is
    // cleared, so the renderer draws the characters underneath.
    lisp.eval(r#"(progn (goto-char (1+ (search-forward "*bold*" 0))) (org-modern-appear))"#.into());
    let emph = ovs[1].0;
    wait(&shared, "the emphasis to reveal its asterisks", |ed| {
        display_at(ed, emph).is_none().then_some(())
    });
    // ...and only that one. Everything else is still a glyph.
    assert_eq!(display_at(&shared.lock().unwrap(), ovs[0].0).as_deref(), Some("◉"));

    // Moving out puts it back, and the string is *recomputed* from the text
    // rather than restored from a copy — which is what makes editing inside a
    // revealed fragment safe.
    lisp.eval("(progn (goto-char (point-max)) (org-modern-appear))".into());
    wait(&shared, "the emphasis to hide again", |ed| {
        (display_at(ed, emph).as_deref() == Some("bold")).then_some(())
    });

    // Edit inside the revealed run, then leave it: the glyph that comes back is
    // the *new* text. A saved string would put `bold` back over `bo0ld`.
    lisp.eval(
        r#"(let ((at (search-forward "*bold*" 0)))
             (goto-char (1+ at))
             (org-modern-appear)
             (insert-at (+ at 3) "0")
             (goto-char (point-max))
             (org-modern-appear))"#
            .into(),
    );
    wait(&shared, "the glyph to follow the edit", |ed| {
        (display_at(ed, emph).as_deref() == Some("bo0ld")).then_some(())
    });

    // The one hook there is drives the same thing, which is what makes this
    // happen while you type rather than only when you ask.
    lisp.eval(r#"(progn (goto-char (1+ (search-forward "*bo0ld*" 0))) (after-change-hook))"#.into());
    wait(&shared, "after-change-hook to reveal", |ed| {
        display_at(ed, emph).is_none().then_some(())
    });

    // --- turning it off ------------------------------------------------------
    //
    // Exactly its own overlays. A foreign one — a LaTeX preview, an avy hint —
    // is invisible to every command in the file, which is what the `:org-modern`
    // mark is for and what `remove-overlays` would have got wrong.
    lisp.eval(r#"(overlay-put (make-overlay 3 6) 'display "XX")"#.into());
    // Six of org-modern's substitutions plus this one. Six and not seven: the
    // emphasis run was revealed a few lines above, so its overlay is still there
    // but no longer carries a display — and `displays` counts substitutions, not
    // overlays.
    wait(&shared, "the foreign overlay", |ed| {
        (displays(ed).len() == 7).then_some(())
    });
    lisp.eval("(org-modern)".into());
    let left = wait(&shared, "org-modern to clear up after itself", |ed| {
        let d = displays(ed);
        (d.len() == 1).then_some(d)
    });
    assert_eq!(left[0], (3, 6, Some("XX".into())), "the foreign overlay survives");
    says(&shared, &lisp, "(if (minor-mode-p 'org-modern) t nil)", "NIL");

    // --- the scanner, without the editor -------------------------------------
    //
    // Cheap to ask directly, and it pins the three exclusions to a reason
    // rather than to a count.
    // A *plist* of overlay properties, so that giving a kind of markup another
    // one — a bigger type size for a heading, say — is a line in
    // `%org-modern-render` and no change to the two functions that apply it.
    // Read with GETF here for the same reason.
    //
    // Two spaces then the third bullet: as wide as the stars it replaces, which
    // is the whole reason a heading's text does not move when it is drawn.
    says(&shared, &lisp, r#"(length (getf (%org-modern-render :heading "***") 'display))"#, "3");
    // ...and the property that made the plist worth having: a heading is
    // *larger type*, said from here as one more entry on the overlay this file
    // was already making. Nothing in the renderer knows the word "heading" —
    // `scale` is a multiple of the body size and this list is the whole policy.
    says(&shared, &lisp, r#"(getf (%org-modern-render :heading "*") 'scale)"#, "1.5");
    says(&shared, &lisp, r#"(getf (%org-modern-render :heading "**") 'scale)"#, "1.25");
    // Past the end of `*org-modern-heading-scale*` is body size, which is what
    // stops a deep outline from being a wall of large type.
    says(&shared, &lisp, r#"(getf (%org-modern-render :heading "****") 'scale)"#, "NIL");
    // Emphasis is really bold and really italic now, rather than the colour that
    // stood in for both while the renderer opened exactly one font face. The
    // face stays: it is what carries `code` and `comment`.
    says(&shared, &lisp, r#"(getf (%org-modern-render :emphasis "*x*") 'weight)"#, "BOLD");
    says(&shared, &lisp, r#"(getf (%org-modern-render :emphasis "/x/") 'slant)"#, "ITALIC");
    says(&shared, &lisp, r#"(getf (%org-modern-render :emphasis "=x=") 'weight)"#, "NIL");
    says(&shared, &lisp, r#"(getf (%org-modern-render :emphasis "/x/") 'face)"#, "italic");
    says(&shared, &lisp, r#"(getf (%org-modern-render :link "[[a][b]]") 'display)"#, "b");
    says(&shared, &lisp, r#"(getf (%org-modern-render :link "[[a]]") 'display)"#, "a");
    // Nothing claims a face where the highlighter already has the colour right.
    says(&shared, &lisp, r#"(getf (%org-modern-render :heading "**") 'face)"#, "NIL");
    // An overlay whose text stopped being markup cannot draw itself, and says
    // so — which is how `%org-modern-rehide` knows to retire it.
    says(&shared, &lisp, r#"(%org-modern-render :emphasis "*half")"#, "NIL");
    says(&shared, &lisp, r#"(%org-modern-render :checkbox "[?]")"#, "NIL");
    // Org's pre/body/post triple, which is the whole difficulty of `*`.
    says(&shared, &lisp, r#"(%org-emphasis-at "2 * 3" 2)"#, "NIL");
    says(&shared, &lisp, r#"(%org-emphasis-at "a *b* c" 2)"#, "4");
    says(&shared, &lisp, r#"(%org-emphasis-at "http://x/a/b/" 5)"#, "NIL");
}
