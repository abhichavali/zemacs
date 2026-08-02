//! A page built in Lisp, end to end — and the point of the file is *which end*.
//!
//! `crates/gui` has a parser test and a layout test, and both would have passed
//! while nothing could put a scene on screen. What is under test here is the
//! seam: five links, each in a different file, and every one of them silent when
//! it breaks.
//!
//! 1. `runtime/gui.lisp` prints a node — `block`, `text`, `run` and the rest are
//!    ordinary Lisp functions that evaluate their arguments;
//! 2. `scene-set` sends that string down `%do` as one verb;
//! 3. one arm of `command_for` reads it with `crates/lisp/src/scene.rs`;
//! 4. core installs it on the live buffer, filling in the sizes of any image
//!    that named none;
//! 5. and a click comes back the other way as an integer, which
//!    `%scene-click` turns into the closure the document wrote beside the widget.
//!
//! The two assertions that would not occur to anyone writing this from the
//! design document are the ones about *strings* and about *stale tags*. Lisp
//! holds buffer text as UTF-8 bytes and the encoder that carries a page to Rust
//! encodes characters, so a line with an em dash in it goes over twice-encoded
//! unless something decodes it first — a page that is wrong in a way no
//! assertion about the tree would notice. And a tag names a closure in a table,
//! so a click that arrives after the page it belonged to has been replaced must
//! find nothing rather than the wrong thing.
//!
//! Deliberately a single `#[test]`, like every other file here: `cl_boot`
//! initialises a process-wide image, so there is one `spawn` per test binary and
//! a new file is the only way to add one.

use std::path::Path;
use std::time::{Duration, Instant};

use zemacs_core::{Buffer, Editor, HlKind, Shared};
use zemacs_gui::{Align, Block, Dir, Length, Node, Run, Scene, Style};

const PATIENCE: Duration = Duration::from_secs(30);

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

fn mark(shared: &Shared) -> usize {
    shared.lock().unwrap().messages.len()
}

fn messages_since(shared: &Shared, from: usize) -> Vec<String> {
    shared.lock().unwrap().messages[from..].to_vec()
}

/// Evaluate `form`, wait for the page it installs, and hand it to `f`.
///
/// The page is taken down first and the wait is for one to be *there* again,
/// rather than for the tree to differ: everything here is asynchronous by
/// construction — Lisp runs on its own thread — and two consecutive pages are
/// perfectly entitled to have the same number of nodes in them.
fn page<T>(shared: &Shared, lisp: &zemacs_lisp::Lisp, form: &str, f: impl Fn(&Scene) -> T) -> T {
    lisp.eval("(scene-set)".into());
    wait(shared, "the previous page to come down", |ed| {
        ed.buffer.scene.is_none().then_some(())
    });
    lisp.eval(form.into());
    wait(shared, form, |ed| ed.buffer.scene.as_ref().map(&f))
}

/// The root of a page, which is the only node anything here starts from.
fn root(scene: &Scene) -> &Node {
    scene.node(scene.root().expect("a page that installed has a root")).unwrap()
}

fn block_of(node: &Node) -> &Block {
    match node {
        Node::Block(b) => b,
        other => panic!("expected a block, found {other:?}"),
    }
}

fn child<'a>(scene: &'a Scene, block: &Block, i: usize) -> &'a Node {
    scene.node(block.children[i]).expect("a child names a node")
}

fn runs_of(node: &Node) -> &[Run] {
    match node {
        Node::Text { runs, .. } => runs,
        other => panic!("expected a paragraph, found {other:?}"),
    }
}

/// The text of a run, or `<image>` — enough to compare a paragraph by.
fn words(runs: &[Run]) -> Vec<&str> {
    runs.iter()
        .map(|r| match r {
            Run::Text { text, .. } => text.as_str(),
            Run::Image { .. } => "<image>",
        })
        .collect()
}

#[test]
fn a_page_built_in_lisp_reaches_the_buffer_and_its_clicks_come_back() {
    let entry = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp"),
    )
    .expect("runtime/init.lisp must exist");

    let init = std::env::temp_dir().join("zemacs_test_scene_init.lisp");
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(message "scene test init loaded")
"#,
            entry.display().to_string()
        ),
    )
    .unwrap();

    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    wait_message(&shared, "gui.lisp to load", |m| m == "scene test init loaded");
    // Loading it must not have been reported as a failure — `init.lisp` wraps
    // the load in a HANDLER-CASE, so a broken file is a message and everything
    // below would then fail with "no such function" instead of saying why.
    let complaint = shared
        .lock()
        .unwrap()
        .messages
        .iter()
        .find(|m| m.contains("gui: not loaded"))
        .cloned();
    assert!(complaint.is_none(), "runtime/gui.lisp must load: {complaint:?}");

    // --- the document's own example ----------------------------------------
    //
    // `docs/gui.org` writes this form as what a config author types, and the
    // whole argument of "two languages with the same shape" is that it is
    // ordinary Lisp: the builders evaluate their arguments and answer the
    // printed form. Character for character from that file, except that the
    // image is a bare id — `(image (org-latex-image ...))` needs a TeX
    // installation, and what it *answers* is an integer, which is the part
    // under test.
    let installed = page(
        &shared,
        &lisp,
        r#"(scene-set
             (block :pad 48 :gap 12 :width (pct 100)
               (text (run "The Rank-Nullity Theorem" :size 200 :bold t))
               (text (run "For a linear map ") (run "T : V - W" :italic t)
                     (run ", the dimensions satisfy:"))
               (image 7314 :width 320 :height 24)))"#,
        |scene| {
            let b = block_of(root(scene)).clone();
            let heading = runs_of(child(scene, &b, 0)).to_vec();
            let sentence = runs_of(child(scene, &b, 1)).to_vec();
            let figure = child(scene, &b, 2).clone();
            (b, heading, sentence, figure)
        },
    );
    let (b, heading, sentence, figure) = installed;

    assert_eq!(b.pad, 48);
    assert_eq!(b.gap, 12);
    assert_eq!(b.width, Length::Pct(100));
    assert_eq!(b.height, Length::Auto, "an unstated length is Auto");
    assert_eq!(b.dir, Dir::Column, "a block stacks down unless told otherwise");
    assert_eq!(b.children.len(), 3);
    assert_eq!(
        heading,
        [Run::Text {
            text: "The Rank-Nullity Theorem".into(),
            style: Style { size: 200, bold: true, ..Style::default() },
            tag: None,
        }]
    );
    // Three runs in one paragraph, in source order, the middle one italic — the
    // whole reason a paragraph is runs rather than nodes.
    assert_eq!(
        words(&sentence),
        ["For a linear map ", "T : V - W", ", the dimensions satisfy:"]
    );
    assert_eq!(
        sentence
            .iter()
            .map(|r| matches!(r, Run::Text { style, .. } if style.italic))
            .collect::<Vec<_>>(),
        [false, true, false]
    );
    assert_eq!(
        figure,
        Node::Image { image: 7314, width: 320, height: 24, depth: 0 }
    );

    // --- the rest of the vocabulary ----------------------------------------
    //
    // Every value kind the printer has to get right, in one page: a length in
    // all four spellings, an alignment and a direction written as keywords, a
    // colour written as a *face name* — which is the vocabulary every overlay
    // already uses, resolved through `face-list` — and an image inside a
    // sentence, which is the construct the grid does better today and the one
    // the whole node model grew a fifth case for.
    let (outer, spacer, inline) = page(
        &shared,
        &lisp,
        r#"(scene-set
             (block :dir :row :align :center :background "comment" :border "markup"
                    :height :fill
               (rect :width (pct 60) :height 1 :fill "link")
               (text :align :end (run "where ") (image-run 12 :width 9 :height 20 :depth 6)
                     (run " is the first."))
               (block :width :auto)))"#,
        |scene| {
            let b = block_of(root(scene)).clone();
            let spacer = child(scene, &b, 0).clone();
            let inline = child(scene, &b, 1).clone();
            (b, spacer, inline)
        },
    );
    assert_eq!(outer.dir, Dir::Row);
    assert_eq!(outer.align, Align::Center);
    assert_eq!(outer.height, Length::Fill);
    // The one thing this file pins about colour: a name is turned into the
    // number `face-list` gives it, which is the number core maps back to an
    // `HlKind`. Written as the round trip rather than as `Some(5)` so that
    // renumbering a face is not a failure here.
    assert_eq!(outer.background, Some(HlKind::Comment.face_id()));
    assert_eq!(outer.border, Some(HlKind::Markup.face_id()));
    assert_eq!(
        spacer,
        Node::Rect {
            w: Length::Pct(60),
            h: Length::Px(1),
            fill: Some(HlKind::Link.face_id())
        }
    );
    match &inline {
        Node::Text { runs, align } => {
            assert_eq!(*align, Align::End);
            assert_eq!(words(runs), ["where ", "<image>", " is the first."]);
            // The depth is what keeps a subscript on the line rather than above
            // it, so a builder that dropped the keyword would be invisible
            // everywhere except in a screenshot.
            assert_eq!(
                runs[1],
                Run::Image { image: 12, width: 9, height: 20, depth: 6, tag: None }
            );
        }
        other => panic!("expected a paragraph, found {other:?}"),
    }

    // --- a malformed page is refused, loudly, and changes nothing ----------
    //
    // Two halves, because there are two places a page can be wrong and they
    // report in different files.
    //
    // Something no builder would print — a config that built the string itself,
    // or one written by hand — is caught by the reader in `scene.rs` and reaches
    // the status line the way `latex:` and `no such command:` do.
    lisp.eval(r#"(%scene-set "(paragraph (run \"x\"))")"#.into());
    wait_message(&shared, "the parse error", |m| {
        m.starts_with("scene-set:") && m.contains("paragraph")
    });
    // ...and the page that was up is still up. This is the assertion the whole
    // arm is shaped around: a parse failure must not install half a document and
    // must not clear the one already there.
    {
        let ed = shared.lock().unwrap();
        let scene = ed.buffer.scene.as_ref().expect("the page must have survived");
        assert_eq!(block_of(root(scene)).dir, Dir::Row);
    }

    // A keyword no node has is caught *here*, in Lisp, before anything is
    // printed — which is the difference between a message naming the keyword and
    // a byte offset into a form nobody wrote.
    let at2 = mark(&shared);
    lisp.eval(r#"(scene-set (block :margin 4))"#.into());
    wait_message(&shared, "the builder error", |m| m.contains(":MARGIN"));
    assert!(
        messages_since(&shared, at2).iter().all(|m| !m.starts_with("scene-set:")),
        "a builder error must not reach the reader at all"
    );
    // The mistake this API invites, since a builder answers a string and so does
    // every reader of buffer text.
    lisp.eval(r#"(scene-set (text "hello"))"#.into());
    wait_message(&shared, "a bare string where a run belongs", |m| {
        m.contains("did you mean (run")
    });

    // --- taking the page down ----------------------------------------------
    lisp.eval("(scene-set)".into());
    wait(&shared, "the page to come down", |ed| {
        ed.buffer.scene.is_none().then_some(())
    });

    // --- text out of a buffer, which is where this breaks ------------------
    //
    // The em dash is not decoration. Lisp holds buffer text as UTF-8 *bytes*,
    // one character per byte, and the encoder carrying a page back to Rust
    // encodes each character as UTF-8 — so buffer text handed straight to `run`
    // is encoded twice and the page disagrees with the file behind it.
    // `utf8-text` is the decode, and this is the pair of assertions that says
    // what it buys and what it costs to forget it.
    {
        let mut ed = shared.lock().unwrap();
        ed.buffer = Buffer::from_str("a \u{2014} dash\n");
    }
    let (decoded, raw) = page(
        &shared,
        &lisp,
        r#"(scene-set
             (block
               (text (run (utf8-text (line-string 1))))
               (text (run (line-string 1)))))"#,
        |scene| {
            let b = block_of(root(scene)).clone();
            let one = |i| match &runs_of(child(scene, &b, i))[0] {
                Run::Text { text, .. } => text.clone(),
                other => panic!("expected a text run, found {other:?}"),
            };
            (one(0), one(1))
        },
    );
    assert_eq!(decoded, "a \u{2014} dash", "utf8-text is what makes a page true");
    assert_ne!(raw, decoded);
    assert!(
        raw.starts_with("a \u{e2}"),
        "the undecoded path is the Latin-1 reading of the dash's own encoding, \
         which is the failure `utf8-text` exists to name; got {raw:?}"
    );
    // A literal in a file that was `load`ed is already characters and needs
    // nothing — which is the other half of the rule, and the reason `utf8-text`
    // cannot simply be folded into `run`: nothing can tell the two apart by
    // looking at them.
    let literal = page(
        &shared,
        &lisp,
        r#"(scene-set (text (run (format nil "one ~a two ~a" (code-char 8212)
                                                             (code-char 955)))))"#,
        |scene| match &runs_of(root(scene))[0] {
            Run::Text { text, .. } => text.clone(),
            other => panic!("expected a text run, found {other:?}"),
        },
    );
    assert_eq!(literal, "one \u{2014} two \u{3bb}");

    // --- a click is an integer, and the integer names a closure ------------
    let first = page(
        &shared,
        &lisp,
        r#"(scene-set
             (block :tag (lambda () (message "the pill was clicked"))
               (text (run "done"))))"#,
        |scene| block_of(root(scene)).tag.expect("the block carries its tag"),
    );
    lisp.eval(format!("(%scene-click {first})"));
    wait_message(&shared, "the click handler", |m| m == "the pill was clicked");

    // A tag naming no closure is *silence*: NIL is what a click on the page
    // itself sends, and a stale integer is what a click racing a rebuild sends.
    // Neither is a mistake anybody made, so neither is a message — and neither
    // may be an error, which would cost the editor the next form as well.
    let at = mark(&shared);
    lisp.eval("(%scene-click 987654)".into());
    lisp.eval("(%scene-click nil)".into());
    lisp.eval(r#"(message "and the image is still here")"#.into());
    wait_message(&shared, "the image after two dead clicks", |m| {
        m == "and the image is still here"
    });
    assert_eq!(
        messages_since(&shared, at),
        ["and the image is still here"],
        "an unknown tag must be silence rather than a report"
    );

    // ...and "stale" is real: the table is retired when the *next* page starts
    // being built, so the closure the last page parked cannot answer for a
    // widget that is no longer on screen.
    let second = page(
        &shared,
        &lisp,
        r#"(scene-set
             (block :tag (lambda () (message "the second page was clicked"))
               (text (run "next"))))"#,
        |scene| block_of(root(scene)).tag.expect("the block carries its tag"),
    );
    assert_ne!(second, first, "a tag is never reused");
    let at = mark(&shared);
    lisp.eval(format!("(%scene-click {first})"));
    lisp.eval(format!("(%scene-click {second})"));
    wait_message(&shared, "the second page's handler", |m| {
        m == "the second page was clicked"
    });
    assert_eq!(
        messages_since(&shared, at),
        ["the second page was clicked"],
        "the page that came down must not still answer for its tags"
    );

    // --- M-x hygiene --------------------------------------------------------
    //
    // The builders take `&rest` and so look zero-argument to the introspection
    // `refresh-commands` does; running one by hand builds a string nobody sees.
    // `scene-set` is deliberately still there — with no argument it takes the
    // page down, which is exactly what you want by hand when a mode has left one
    // up.
    {
        let names = shared.lock().unwrap().commands.clone();
        let heads: Vec<&str> = names
            .iter()
            .map(|n| n.split_whitespace().next().unwrap_or(n))
            .collect();
        for hidden in ["block", "text", "run", "rect"] {
            assert!(!heads.contains(&hidden), "M-x must not offer {hidden}: {names:?}");
        }
        assert!(heads.contains(&"scene-set"), "M-x must offer scene-set: {names:?}");
    }
}
