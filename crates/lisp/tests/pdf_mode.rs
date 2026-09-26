//! A PDF opens as pages.
//!
//! `pdf-mode` is reached from the file name alone, puts the first page up as a
//! scene sized to the window, and `j`-style scrolling turns the page at either
//! end: onto the top of the next one going down, onto the *bottom* of the last
//! one going up. Search goes to the page that has the words.
//!
//! Needs `mutool` on `$PATH` — it draws the pages — and says so and passes
//! without it. One `#[test]`: `cl_boot` initialises a process-wide image.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, Shared};
use zemacs_gui::Node;

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

/// Same dispatch as `crates/app/src/main.rs`; see `image_mode.rs`.
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

fn runtime(file: &str) -> String {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime").join(file))
        .unwrap_or_else(|e| panic!("runtime/{file} must exist: {e}"))
        .display()
        .to_string()
}

/// Where the page is: the status line's `page N of M`, and the scene's scroll.
fn at(shared: &Shared, page: usize, scroll: i32) {
    let want = format!("page {page} of 2");
    wait(shared, &format!("{want} scrolled to {scroll}"), |ed| {
        let y = ed.buffer.scene.as_ref()?.scroll;
        (ed.status == want && y == scroll).then_some(())
    });
}

#[test]
fn a_pdf_opens_as_pages_that_scroll_into_one_another() {
    let dir = std::env::temp_dir().join(format!("zemacs_pdf_mode-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // Two 200x300 pt pages that say `alpha` and `beta`.
    let mut pages = Vec::new();
    for word in ["alpha", "beta"] {
        let p = dir.join(format!("{word}.txt"));
        std::fs::write(
            &p,
            format!("%%MediaBox 0 0 200 300\n%%Font F1 Times-Roman\nBT /F1 24 Tf 20 250 Td ({word}) Tj ET\n"),
        )
        .unwrap();
        pages.push(p);
    }
    let pdf = dir.join("two.pdf");
    match Command::new("mutool").arg("create").arg("-o").arg(&pdf).args(&pages).status() {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("skipped: no mutool on $PATH to draw a PDF with");
            return;
        }
    }

    let init = dir.join("init.lisp");
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n{}(message \"pdf mode test init loaded\")\n",
            ["modes/modes.lisp", "gui.lisp", "modes/pdf.lisp"]
                .map(|f| format!("(load {:?} :verbose nil :print nil)\n", runtime(f)))
                .concat()
        ),
    )
    .unwrap();
    let (_tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = Arc::new(zemacs_lisp::spawn(_tx, shared.clone(), init));
    pump(shared.clone(), lisp.clone());
    wait(&shared, "the init file", |ed| {
        ed.messages.iter().any(|m| m == "pdf mode test init loaded").then_some(())
    });

    // What `open_file` does for a file that is not text, in a 400x300 window.
    {
        let mut ed = shared.lock().unwrap();
        ed.viewport_px = (400, 300);
        ed.load("two.pdf", Some(pdf.clone()), None);
        ed.apply(EditorCommand::SetReadOnly(true));
    }
    at(&shared, 1, 0);
    // Fitted to the window's width less the margins: 368 wide, so 552 tall,
    // and 552 + 2 * 16 - 300 = 284 pixels of it below the window.
    let (w, h) = wait(&shared, "the page's bitmap", |ed| {
        let scene = ed.buffer.scene.as_ref()?;
        let Node::Block(page) = scene.node(scene.root()?)? else { return None };
        let Node::Image { image, .. } = scene.node(*page.children.first()?)? else { return None };
        ed.image(*image).map(|i| (i.width, i.height))
    });
    assert_eq!((w, h), (368, 552));

    lisp.eval("(pdf-scroll-down-half)".into());
    at(&shared, 1, 150);
    lisp.eval("(pdf-scroll-down-half)".into());
    at(&shared, 1, 284); // stops at the bottom rather than past it...
    lisp.eval("(pdf-scroll-down-half)".into());
    at(&shared, 2, 0); // ...and the next one turns the page.
    lisp.eval("(pdf-scroll-up)".into());
    at(&shared, 1, 284); // Going back lands where you left it: the bottom.

    lisp.eval("(progn (setf *pdf-last-search* \"BETA\") (pdf-search-next))".into());
    wait(&shared, "search to find page 2", |ed| {
        ed.status.starts_with("\"BETA\" on page 2 of 2").then_some(())
    });

    let _ = std::fs::remove_dir_all(&dir);
}
