//! Getting a picture *into* an org document: dropped, or pasted.
//!
//! Two gestures, one destination. Both end at `%org-insert-image`, so what is
//! worth pinning is that function's contract and the two thin arms in front of
//! it — because the arms are where the decisions are:
//!
//!   * **a link, not an attachment.** What lands in the buffer is text you could
//!     have typed, and the file stays where it is.
//!   * **on its own line**, which is org's rule for what a figure is and this
//!     runtime's rule for what it will draw. A line with anything on it gets a
//!     newline first; a blank one is used as it stands.
//!   * **relative when it can be.** A picture inside the document's own folder
//!     is written relative, so moving or sending the folder does not break it;
//!     one from `~/.zemacs.d/images/` is written `~/`-abbreviated.
//!   * **one edit**, so `u` takes the picture and its line away together.
//!
//! `%file-dropped` and `%clipboard-image` are called by the application, so they
//! are called here the same way — by name, with the argument the app builds.
//!
//! Deliberately a single `#[test]`: `cl_boot` initialises a process-wide image.

use std::path::Path;
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

fn says(shared: &Shared, lisp: &zemacs_lisp::Lisp, tag: &str, form: &str, want: &str) {
    lisp.eval(format!("(message (format nil \"{tag}=~a\" {form}))"));
    let want = format!("{tag}={want}");
    wait(shared, &format!("{form} => {want}"), |ed| {
        ed.messages.iter().any(|m| *m == want).then_some(())
    });
}

fn text_is(shared: &Shared, what: &str, want: &str) {
    wait(shared, what, |ed| (ed.buffer.text.to_string() == want).then_some(()));
}

fn runtime(file: &str) -> String {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime").join(file))
        .unwrap_or_else(|e| panic!("runtime/{file} must exist: {e}"))
        .display()
        .to_string()
}

#[test]
fn a_picture_dropped_or_pasted_becomes_a_figure() {
    // A real PNG, so `image-file` genuinely decodes and the overlay it hangs is
    // the one the renderer would draw. Hand-built: 1×1 red, deflate-stored, so
    // the test carries no fixture.
    let dir = std::env::temp_dir().join(format!("zemacs_org_figures-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("figures")).unwrap();
    std::fs::write(dir.join("figures").join("plot.png"), one_pixel_png()).unwrap();
    let outside = std::env::temp_dir().join(format!("zemacs_outside-{}.png", std::process::id()));
    std::fs::write(&outside, one_pixel_png()).unwrap();

    let init = std::env::temp_dir()
        .join(format!("zemacs_org_figures_init-{}.lisp", std::process::id()));
    std::fs::write(
        &init,
        format!(
            r#"(in-package :zemacs)
(load {:?} :verbose nil :print nil)
(load {:?} :verbose nil :print nil)
(message "org figures test init loaded")
"#,
            runtime("modes/modes.lisp"),
            runtime("modes/org-modern.lisp"),
        ),
    )
    .unwrap();

    // Held, not dropped: `find-file` is one of the commands only the
    // application can carry out, so it arrives here rather than on the editor.
    let (tx, rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init);
    wait(&shared, "the init file", |ed| {
        ed.messages.iter().any(|m| m == "org figures test init loaded").then_some(())
    });

    let note = dir.join("note.org");
    {
        let mut ed = shared.lock().unwrap();
        ed.load("", Some(note.clone()), None);
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::InsertText("intro\n".into()));
        ed.apply(EditorCommand::SetMode(Mode::Normal));
    }
    lisp.eval("(org-mode)".into());
    wait(&shared, "org-mode", |ed| (ed.buffer.major_mode == "org-mode").then_some(()));

    // --- how a path is written down -----------------------------------------
    //
    // Inside the document's own folder, the link is relative — which is what
    // survives the folder being moved, sent or checked in.
    says(&shared, &lisp, "rel",
         &format!("(%org-image-link-path {:?})", dir.join("figures/plot.png").display().to_string()),
         "figures/plot.png");
    // ...and a file that is nowhere near the document keeps an absolute path,
    // with `~/` put back when it is under home. `%org-expand-file` reads both.
    says(&shared, &lisp, "home", r#"(%org-image-link-path (format nil "~a/pic.png" (string-right-trim "/" (namestring (user-homedir-pathname)))))"#,
         "~/pic.png");

    // --- dropping one -------------------------------------------------------
    //
    // Point is on line 2, which is blank, so the link uses that line rather
    // than opening a gap above itself.
    lisp.eval("(goto-char (point-max))".into());
    lisp.eval(format!("(%file-dropped {:?})", dir.join("figures/plot.png").display().to_string()));
    text_is(&shared, "the dropped link", "intro\n[[file:figures/plot.png]]");
    // ...and it is drawn, not just typed: one figure overlay covering the link.
    says(&shared, &lisp, "drawn", "(length (%org-images))", "1");

    // --- and a second one, where the line is not blank ----------------------
    lisp.eval("(goto-char 6)".into()); // end of `intro`
    lisp.eval(format!("(%file-dropped {:?})", outside.display().to_string()));
    wait(&shared, "a newline before the second link", |ed| {
        ed.buffer.text.to_string().starts_with("intro\n[[file:").then_some(())
    });
    says(&shared, &lisp, "drawn2", "(length (%org-images))", "2");

    // --- a dropped file that is not a picture is a file you meant to open ---
    //
    // The editor only asks Lisp about a drop when the live buffer is org, so
    // this arm is the fallback for "org buffer, but that was a `.rs`".
    // Headless there is no application to *do* the opening, so what is asserted
    // is the request: `find-file` emits `EditorCommand::OpenFile`, which is one
    // of the few core cannot carry out itself. Catching it on the channel is
    // better evidence than a status string — it is the very value the app acts
    // on.
    let src = dir.join("code.rs");
    std::fs::write(&src, "fn main() {}\n").unwrap();
    let before = shared.lock().unwrap().buffer.text.to_string();
    lisp.eval(format!("(%file-dropped {:?})", src.display().to_string()));
    let opened = wait(&shared, "the non-picture to be handed to find-file", |_| {
        while let Ok(cmd) = rx.try_recv() {
            if let EditorCommand::OpenFile(p) = cmd {
                return Some(p);
            }
        }
        None
    });
    assert_eq!(opened, src);
    says(&shared, &lisp, "no-third-figure", "(length (%org-images))", "2");
    assert_eq!(
        shared.lock().unwrap().buffer.text.to_string(),
        before,
        "a dropped `.rs` must not put a link in the document"
    );

    // --- pasting one --------------------------------------------------------
    //
    // `%clipboard-image` is the reply half. NIL is the case worth pinning: it
    // is a key you pressed *because* you had copied a picture, so silence would
    // be the wrong answer.
    lisp.eval("(%clipboard-image nil)".into());
    wait(&shared, "an empty clipboard to say so", |ed| {
        ed.messages.iter().any(|m| m == "no image in the clipboard").then_some(())
    });
    // ...and a path arriving while the buffer is *not* org says where the file
    // went rather than writing a link into it. The buffer can change between the
    // request and the reply — it is a round trip through another thread — so
    // this is not a hypothetical.
    lisp.eval("(text-mode)".into());
    wait(&shared, "a buffer that is not org", |ed| {
        (ed.buffer.major_mode == "text-mode").then_some(())
    });
    let before = shared.lock().unwrap().buffer.text.to_string();
    lisp.eval(format!("(%clipboard-image {:?})", outside.display().to_string()));
    wait(&shared, "a paste in the wrong buffer to report the path", |ed| {
        ed.messages.iter().any(|m| m.starts_with("image saved to ")).then_some(())
    });
    assert_eq!(
        shared.lock().unwrap().buffer.text.to_string(),
        before,
        "a paste must not write a link into a buffer that is not org"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&outside);
}

/// A 1×1 red PNG, built rather than stored so the suite carries no binary.
fn one_pixel_png() -> Vec<u8> {
    fn chunk(tag: &[u8], data: &[u8]) -> Vec<u8> {
        let mut body = tag.to_vec();
        body.extend_from_slice(data);
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
        out
    }
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }
    // One stored (uncompressed) deflate block, which needs no compressor.
    let raw = [0u8, 0xFF, 0x00, 0x00];
    let mut z = vec![0x78, 0x01, 0x01, 0x04, 0x00, 0xFB, 0xFF];
    z.extend_from_slice(&raw);
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = 1u32.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&1u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
    png.extend_from_slice(&chunk(b"IHDR", &ihdr));
    png.extend_from_slice(&chunk(b"IDAT", &z));
    png.extend_from_slice(&chunk(b"IEND", &[]));
    png
}
