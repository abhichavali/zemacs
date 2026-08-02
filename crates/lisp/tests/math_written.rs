//! Headless proof of `runtime/modes/math-written.lisp` — a photograph of your
//! handwriting becoming the answer to a written problem.
//!
//! **Nothing here touches the network.** `*math-written-transport*` is the seam
//! the file publishes for exactly this: one function of one pathname answering
//! org text, so a stub proves the whole pipeline — which problem, what lands in
//! it, what happens to the photograph — without OpenRouter existing. The two
//! halves that *are* about the network are proved on their own, as pure
//! functions over a captured reply: the model is resolved against a literal
//! models list, and the transcription is pulled out of a literal completion.
//!
//! What is asserted, in the order it goes wrong:
//!
//! * the key is found in `$OPENROUTER_API_KEY`, and in the key file when that is
//!   unset, and its absence names *both* places rather than failing silently;
//! * a file still being uploaded is left alone — a photograph is only touched
//!   when its size has stopped changing;
//! * the transcription lands in the problem **point was in**, and writing it
//!   twice leaves the document identical;
//! * point outside a written problem declines and **does not consume the file**;
//! * a captured photograph is *moved*, never deleted.
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
            panic!("timed out waiting for {what}; status={:?} last={seen:#?}", ed.status);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_message(shared: &Shared, what: &str, pred: impl Fn(&str) -> bool) {
    wait(shared, what, |ed| ed.messages.iter().any(|m| pred(m)).then_some(()))
}

/// Evaluate `form` and wait for its value, printed with `~a`, to be `want`.
fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, want: &str) {
    let tag = NTH.fetch_add(1, Ordering::Relaxed);
    lisp.eval(format!("(message (format nil \"#{tag} ~a\" {form}))"));
    let want = format!("#{tag} {want}");
    wait_message(shared, form, |m| m == want);
}

/// Run `form` for its effect and wait until it has landed. Lisp runs on its own
/// thread, so an `eval` returns long before the edit it asked for arrives.
fn does(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str) {
    let tag = NTH.fetch_add(1, Ordering::Relaxed);
    lisp.eval(format!("(progn {form} (message \"#{tag} done\"))"));
    let want = format!("#{tag} done");
    wait_message(shared, form, |m| m == want);
}

fn text(shared: &Shared) -> String {
    let ed = shared.lock().unwrap();
    ed.buffer.slice_string(0, ed.buffer.len_chars())
}

/// `s` as a Lisp string literal.
///
/// Not `{:?}`: that is *Rust's* escape syntax, and the two do not agree —
/// `\n` inside a Lisp literal is the letter n, so a JSON fixture written with
/// `{:?}` arrives at the reader with its newlines turned into text. Lisp has
/// exactly two escapes; everything else, newlines included, goes in as itself.
fn lisp_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn runtime(file: &str) -> String {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime").join(file))
        .unwrap_or_else(|e| panic!("runtime/{file} must exist: {e}"))
        .display()
        .to_string()
}

/// Two written problems and one programming problem, so that "the right
/// problem" is a question with a wrong answer available, and so that a
/// `programming` problem can be shown to decline.
const CURRICULUM: &str = "\
#+TITLE: Linear Algebra I
#+ZEMACS_CURRICULUM: 1

* Contents
- [[id:unit-1][1. Vectors and Spaces]]

* Vectors and Spaces
:PROPERTIES:
:ID: unit-1
:END:

A basis is a linearly independent spanning set.

** Problem
:PROPERTIES:
:ZEMACS_PROBLEM: written
:ZEMACS_STATUS: todo
:END:

Prove that every basis of a finite-dimensional space has the same cardinality.

*** Response

** Problem
:PROPERTIES:
:ZEMACS_PROBLEM: written
:ZEMACS_STATUS: todo
:END:

Show that the intersection of two subspaces is a subspace.

*** Response

** Problem
:PROPERTIES:
:ZEMACS_PROBLEM: programming
:ZEMACS_STATUS: todo
:END:

Implement Gaussian elimination.

#+begin_src python :tangle gaussian.py
def solve(A, b):
    return None
#+end_src
";

/// One model list, with the three cases that matter: the model actually wanted
/// (punctuated differently from how a person says it), a same-family model that
/// must not be picked instead, and a model whose *name* matches but which cannot
/// see — choosing it would be a request certain to fail.
const MODELS: &str = r#"{"data":[
  {"id":"openai/gpt-4o","name":"OpenAI: GPT-4o",
   "architecture":{"input_modalities":["text","image"],"output_modalities":["text"]}},
  {"id":"openai/gpt-5.6-luna","name":"OpenAI: GPT-5.6 Luna",
   "architecture":{"input_modalities":["text","image"],"output_modalities":["text"]}},
  {"id":"openai/gpt-5.6-luna-text","name":"OpenAI: GPT-5.6 Luna Text",
   "architecture":{"input_modalities":["text"],"output_modalities":["text"]}},
  {"id":"anthropic/claude-3.5-sonnet","name":"Anthropic: Claude 3.5 Sonnet",
   "architecture":{"modality":"text+image->text"}}
]}"#;

/// A completion in the shape OpenRouter answers, with the escapes a LaTeX
/// transcription actually contains — backslashes, newlines, a `\u` escape.
const COMPLETION: &str = r#"{"id":"gen-1","choices":[{"index":0,"message":{"role":"assistant",
  "content":"Let $B$ and $C$ be bases.\n\n\\[\n  |B| = |C|\n\\]\n\nHence \u2200 bases agree."}}]}"#;

#[test]
fn a_photograph_becomes_the_answer_to_the_problem_point_is_in() {
    let root = std::env::temp_dir().join("zemacs_test_written");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let sync = root.join("MathSync");
    std::fs::create_dir_all(&sync).unwrap();
    let org = root.join("curriculum.org");
    let keyfile = root.join("openrouter-key");

    let lisp_str = |p: &Path| p.display().to_string().replace('\\', "\\\\");

    let init = root.join("init.lisp");
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load "{}" :verbose nil :print nil)
(load "{}" :verbose nil :print nil)
(load "{}" :verbose nil :print nil)
(load "{}" :verbose nil :print nil)
(setf *math-written-directory* (pathname "{}/"))
(setf *math-written-key-file* (pathname "{}"))
(setf *math-written-interval* 0.2)
;; The seam. Everything above the transport is exercised for real; the transport
;; itself answers a fixed transcription, and records that it was asked.
(defvar *stub-calls* 0)
(defvar *stub-text* "Let $B$ and $C$ be bases.")
(setf *math-written-transport*
      (lambda (path) (declare (ignore path)) (incf *stub-calls*) *stub-text*))
;; The typesetting seam. `org-latex-preview-new' lives in `init.lisp', which this
;; image deliberately does not load — so without this the `fboundp' guard in
;; `%mw-typeset' would hold and the call would be untestable rather than tested.
(defvar *typeset-calls* 0)
(defun org-latex-preview-new () (incf *typeset-calls*))
(message "math-written test init loaded")
"#,
            runtime("modes/modes.lisp"),
            runtime("modes/org-modern.lisp"),
            runtime("modes/math.lisp"),
            runtime("modes/math-written.lisp"),
            lisp_str(&sync),
            lisp_str(&keyfile),
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);

    wait_message(&shared, "the runtime files to load", |m| {
        m == "math-written test init loaded"
    });
    let seen = shared.lock().unwrap().messages.clone();
    assert!(
        !seen.iter().any(|m| m.contains("error")),
        "math-written.lisp must load cleanly; got {seen:#?}"
    );

    {
        let mut ed = shared.lock().unwrap();
        ed.load(CURRICULUM, Some(PathBuf::from(&org)), Some("org".into()));
        ed.apply(EditorCommand::SetMode(Mode::Normal));
        ed.apply(EditorCommand::MoveTo(0));
    }

    // --- the key --------------------------------------------------------------
    //
    // Nothing set and no file: NIL, and a message naming *both* places. A key
    // that is merely missing must not read as an editor that is broken.
    does(&shared, &lisp, r#"(ext:setenv "OPENROUTER_API_KEY" "")"#);
    says(&shared, &lisp, "(%mw-key)", "NIL");
    wait_message(&shared, "the missing-key message", |m| {
        m.contains("no OpenRouter key")
            && m.contains("OPENROUTER_API_KEY")
            && m.contains("openrouter-key")
    });

    // The file, at mode 600: read, and its trailing newline gone.
    //
    // Compared inside the image rather than echoed out of it — printing the key
    // to assert on it would put it in the very log the last assertion here
    // checks is clean.
    std::fs::write(&keyfile, "sk-from-the-file\n").unwrap();
    does(&shared, &lisp, r#"(ext:chmod *math-written-key-file* #o600)"#);
    says(&shared, &lisp, r#"(if (string= (%mw-key) "sk-from-the-file") t nil)"#, "T");
    // ...and a mode anybody can read is a warning, not a refusal: a key that
    // works must keep working, or the user is left with a puzzle.
    does(&shared, &lisp, r#"(ext:chmod *math-written-key-file* #o644)"#);
    says(&shared, &lisp, r#"(if (string= (%mw-key) "sk-from-the-file") t nil)"#, "T");
    wait_message(&shared, "the permissions warning", |m| {
        m.contains("chmod 600") && m.contains("-rw-r--r--")
    });

    // The environment wins over the file, because that is the order a machine
    // with a secret manager wants.
    does(&shared, &lisp, r#"(ext:setenv "OPENROUTER_API_KEY" "sk-from-the-env")"#);
    says(&shared, &lisp, r#"(if (string= (%mw-key) "sk-from-the-env") t nil)"#, "T");
    // ...and the key is never in a message. The whole log, checked once.
    {
        let ed = shared.lock().unwrap();
        assert!(
            !ed.messages.iter().any(|m| m.contains("sk-from-the")),
            "the key must never reach the status line: {:#?}",
            ed.messages
        );
    }

    // --- JSON, which everything below reads through ---------------------------
    says(&shared, &lisp, r#"(%mw-jget (%mw-json "{\"a\":{\"b\":[1,2,3]}}") "a" "b" 1)"#, "2");
    says(&shared, &lisp, r#"(%mw-json "true")"#, "T");
    says(&shared, &lisp, r#"(%mw-json "{}")"#, "NIL");
    says(&shared, &lisp, r#"(%mw-json "  [ ] ")"#, "NIL");
    // The escapes a LaTeX transcription is made of: a backslash survives, and so
    // does a `\u` escape.
    says(
        &shared,
        &lisp,
        r#"(%mw-json "\"a\\\\[ x \\\\]\"")"#,
        "a\\[ x \\]",
    );
    // ...and a round trip through the escaper this file writes requests with.
    says(
        &shared,
        &lisp,
        r#"(%mw-json (%mw-json-string (format nil "\\[ x \\]~%line")))"#,
        "\\[ x \\]\nline",
    );
    // base64, against the vector every implementation is checked with.
    says(
        &shared,
        &lisp,
        r#"(with-output-to-string (s)
             (%mw-base64-onto s (map 'vector #'char-code "Man")))"#,
        "TWFu",
    );
    says(
        &shared,
        &lisp,
        r#"(with-output-to-string (s)
             (%mw-base64-onto s (map 'vector #'char-code "Ma")))"#,
        "TWE=",
    );
    says(
        &shared,
        &lisp,
        r#"(with-output-to-string (s)
             (%mw-base64-onto s (map 'vector #'char-code "M")))"#,
        "TQ==",
    );

    // --- the model is resolved, never guessed ---------------------------------
    //
    // `GPT 5.6 luna` is what a person says. The slug is `openai/gpt-5.6-luna`,
    // and the distance between those two is the whole of the normalisation.
    does(
        &shared,
        &lisp,
        &format!("(defvar *models* (%mw-jget (%mw-json {}) \"data\"))", lisp_string(MODELS)),
    );
    says(&shared, &lisp, r#"(%mw-pick-model *models* "GPT 5.6 luna")"#, "openai/gpt-5.6-luna");
    says(&shared, &lisp, r#"(%mw-pick-model *models* "gpt-5.6-luna")"#, "openai/gpt-5.6-luna");
    says(
        &shared,
        &lisp,
        r#"(%mw-pick-model *models* "openai/GPT-5.6-Luna")"#,
        "openai/gpt-5.6-luna",
    );
    // A unique substring is an answer...
    says(&shared, &lisp, r#"(%mw-pick-model *models* "luna")"#, "openai/gpt-5.6-luna");
    // ...and a name nobody has is not, and says so with candidates rather than
    // posting a photograph at whatever was first in the list.
    says(&shared, &lisp, r#"(%mw-pick-model *models* "gpt 9 nova")"#, "NIL");
    says(
        &shared,
        &lisp,
        r#"(format nil "~{~a~^ ~}"
             (nth-value 1 (%mw-pick-model *models* "gpt 9 nova")))"#,
        "openai/gpt-4o openai/gpt-5.6-luna",
    );
    // The older `modality` spelling is understood as well as the new one.
    says(&shared, &lisp, r#"(%mw-pick-model *models* "claude 3.5 sonnet")"#,
         "anthropic/claude-3.5-sonnet");
    // A text-only model is not offered even when its name matches exactly:
    // sending it an image is a request certain to fail, and failing here is
    // easier to understand than failing later with a photograph attached.
    says(&shared, &lisp, r#"(%mw-vision-p (third *models*))"#, "NIL");
    says(&shared, &lisp, r#"(%mw-pick-model *models* "GPT 5.6 Luna Text")"#, "NIL");

    // --- a completion is read the way OpenRouter sends one --------------------
    does(
        &shared,
        &lisp,
        &format!("(defvar *said* (%mw-content (%mw-json {})))", lisp_string(COMPLETION)),
    );
    says(&shared, &lisp, "(subseq *said* 0 25)", "Let $B$ and $C$ be bases.");
    // The display maths survives the two decodings it has to: JSON's `\\` to one
    // backslash, and the newlines that make it a display rather than a line.
    says(
        &shared,
        &lisp,
        r#"(if (search (format nil "~%\\[~%  |B| = |C|~%\\]") *said*) t nil)"#,
        "T",
    );
    // ...and a `\uXXXX` escape is a character, not four.
    says(&shared, &lisp, "(if (find (code-char 8704) *said*) t nil)", "T");
    // A model that wrapped the answer in a fence anyway gets it taken off, which
    // the prompt asks for and models do regardless.
    says(
        &shared,
        &lisp,
        r#"(%mw-unfence (format nil "```org~%$x$~%```"))"#,
        "$x$",
    );

    // --- a file still uploading is left alone ---------------------------------
    //
    // The whole of the stability rule: same size as last time, or wait.
    let photo = sync.join("IMG_0001.jpg");
    std::fs::write(&photo, b"aaaa").unwrap();
    let photo_form = format!("(pathname \"{}\")", lisp_str(&photo));
    // First sighting: recorded, not settled — nothing has been compared yet.
    says(&shared, &lisp, &format!("(%mw-settled-p {photo_form})"), "NIL");
    // Still arriving: bigger than last time, so still not settled.
    std::fs::write(&photo, b"aaaaaaaa").unwrap();
    says(&shared, &lisp, &format!("(%mw-settled-p {photo_form})"), "NIL");
    // Finished: unchanged, and therefore ready.
    says(&shared, &lisp, &format!("(%mw-settled-p {photo_form})"), "T");
    // A file that exists but is empty is never settled, however long it sits
    // there: created-but-not-yet-written has a stable size too.
    let empty = sync.join("IMG_0002.jpg");
    std::fs::write(&empty, b"").unwrap();
    let empty_form = format!("(pathname \"{}\")", lisp_str(&empty));
    says(&shared, &lisp, &format!("(%mw-settled-p {empty_form})"), "NIL");
    says(&shared, &lisp, &format!("(%mw-settled-p {empty_form})"), "NIL");
    std::fs::remove_file(&empty).unwrap();

    // --- point outside a written problem declines, and consumes nothing -------
    //
    // In the preamble: not a problem at all.
    says(&shared, &lisp, "(progn (goto-char 0) (%mw-binding))", "NIL");
    // In a *programming* problem: a problem, and not this feature's.
    says(
        &shared,
        &lisp,
        r#"(progn (goto-char (search-forward "Implement Gaussian" 0)) (%mw-binding))"#,
        "NIL",
    );
    // ...so a capture from there declines, says so, and leaves the file exactly
    // where it was. Nothing is sent: the stub is not called.
    let before_decline = text(&shared);
    does(&shared, &lisp, &format!("(%mw-capture {photo_form})"));
    says(&shared, &lisp, "*stub-calls*", "0");
    wait_message(&shared, "the decline", |m| {
        m.contains("IMG_0001.jpg") && m.contains("put point in a written problem")
    });
    assert!(photo.exists(), "a declined photograph is not consumed");
    assert_eq!(before_decline, text(&shared), "and the document is untouched");

    // --- and lands in the problem point *is* in -------------------------------
    //
    // The second written problem, deliberately: the first is what a capture that
    // simply took "the first written problem" would hit.
    does(
        &shared,
        &lisp,
        r#"(goto-char (search-forward "intersection of two subspaces" 0))"#,
    );
    does(&shared, &lisp, &format!("(%mw-capture {photo_form})"));
    says(&shared, &lisp, "*stub-calls*", "1");
    let landed = text(&shared);
    assert!(
        landed.contains("Show that the intersection of two subspaces is a subspace.\n\n*** Response\nLet $B$ and $C$ be bases.\n"),
        "the transcription lands under the problem point was in: {landed}"
    );
    assert!(
        !landed.contains("cardinality.\n\n*** Response\nLet $B$"),
        "and not under the first written problem: {landed}"
    );
    // ...and it was asked to be typeset. The automatic previews hang off point
    // *movement*, and the watcher thread writes without moving anything — so
    // nothing else in the editor would ever ask, and `$B$` would sit there as
    // four characters until the cursor happened to leave the line.
    says(&shared, &lisp, "*typeset-calls*", "1");
    // Captured, not marked correct: `done` here means "there is an answer in the
    // file", and `SPC m t' sets it straight back. Nothing in this system judges
    // a proof.
    says(&shared, &lisp, "(getf (second (math-problems)) :status)", "done");
    says(&shared, &lisp, "(getf (first (math-problems)) :status)", "todo");

    // --- the photograph is moved, never deleted -------------------------------
    assert!(!photo.exists(), "the photograph leaves the watched directory");
    let archived = sync.join("done").join("IMG_0001.jpg");
    assert!(archived.exists(), "...into done/, still readable: {archived:?}");
    assert_eq!(std::fs::read(&archived).unwrap(), b"aaaaaaaa");

    // --- writing the same transcription twice changes nothing -----------------
    //
    // `math-response-insert` promises it and a capture pass depends on it: the
    // same photograph offered again — a restart, a file that could not be moved
    // — must not grow the document.
    std::fs::write(&photo, b"aaaaaaaa").unwrap();
    does(
        &shared,
        &lisp,
        &format!("(progn (%mw-settled-p {photo_form}) (%mw-settled-p {photo_form}))"),
    );
    does(&shared, &lisp, &format!("(%mw-capture {photo_form})"));
    assert_eq!(landed, text(&shared), "the same transcription twice is the same document");
    let twice = sync.join("done").join("IMG_0001-1.jpg");
    assert!(twice.exists(), "and the second photograph is kept beside the first, not over it");

    // --- a binding the document moved out from under is refused ---------------
    //
    // The ten seconds a real transcription takes are ten seconds the buffer can
    // change. The marker is what notices — and the answer is to keep the
    // photograph rather than to write into whatever is at that offset now.
    does(&shared, &lisp, r#"(setf *stub-text* "Second attempt.")"#);
    does(
        &shared,
        &lisp,
        r#"(progn (goto-char (search-forward "intersection of two subspaces" 0))
                 (defvar *held* (%mw-binding)))"#,
    );
    does(&shared, &lisp, r#"(let ((p (second (math-problems))))
                             (replace-region (getf p :begin) (getf p :end) ""))"#);
    says(&shared, &lisp, "(if (%mw-land *held* \"anything\") t nil)", "NIL");
    wait_message(&shared, "the refusal", |m| m.contains("photograph kept"));
    does(&shared, &lisp, "(undo)");

    // --- an unreadable page marks nothing done --------------------------------
    //
    // A model that cannot read the page must not leave the problem looking
    // answered. The photograph stays, and so does `todo`.
    std::fs::write(&photo, b"bbbbbbbb").unwrap();
    does(&shared, &lisp, r#"(setf *stub-text* "(illegible)")"#);
    does(
        &shared,
        &lisp,
        r#"(goto-char (search-forward "every basis of a finite" 0))"#,
    );
    let before_illegible = text(&shared);
    does(&shared, &lisp, &format!("(%mw-capture {photo_form})"));
    wait_message(&shared, "the illegible message", |m| m.contains("could not be read"));
    assert_eq!(before_illegible, text(&shared), "an unreadable page changes nothing");
    assert!(photo.exists(), "and the photograph is kept");
    says(&shared, &lisp, "(getf (first (math-problems)) :status)", "todo");

    // --- the watcher ----------------------------------------------------------
    //
    // The one thing a hook cannot do: fire with nobody at the keyboard. Point is
    // put in the first problem, the watcher is started, and a photograph is
    // dropped in from *outside* the editor — no key is pressed after this line.
    does(&shared, &lisp, r#"(setf *stub-text* "Any two bases are equinumerous.")"#);
    does(
        &shared,
        &lisp,
        r#"(goto-char (search-forward "every basis of a finite" 0))"#,
    );
    // A resolved model, so the watcher has somewhere to send to without a
    // network call to find out.
    does(&shared, &lisp, r#"(setf *math-written-model* "openai/gpt-5.6-luna")"#);
    does(&shared, &lisp, "(math-watch)");
    std::fs::write(sync.join("IMG_0009.jpg"), b"cccccccccccc").unwrap();

    wait(&shared, "the watcher to capture a photograph nobody told it about", |ed| {
        ed.buffer
            .slice_string(0, ed.buffer.len_chars())
            .contains("*** Response\nAny two bases are equinumerous.\n")
            .then_some(())
    });
    wait(&shared, "the watched photograph to be archived", |_| {
        sync.join("done").join("IMG_0009.jpg").exists().then_some(())
    });
    does(&shared, &lisp, "(math-watch)");
    says(&shared, &lisp, "(if *mw-watching* t nil)", "NIL");

    // The image is still there and still answering, which is the point of doing
    // every slow thing on a thread of its own.
    says(&shared, &lisp, "(length (math-problems))", "3");
}
