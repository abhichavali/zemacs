//! A picture opens as a picture.
//!
//! The application loads a file that is not text as a read-only buffer holding
//! its name, and `image-mode` — chosen by extension from `fundamental-mode` —
//! hangs one image overlay over that name. What is pinned here is the Lisp
//! half: the mode is reached from the file name alone, it draws exactly one
//! overlay, and the buffer is not something `:w` could write back over the
//! picture.
//!
//! One `#[test]`: `cl_boot` initialises a process-wide image.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zemacs_core::{Editor, EditorCommand, ReadOnly, Shared};

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

/// Mode hooks are queued by core and run by the application; headless, this
/// thread stands in for it. Same dispatch as `crates/app/src/main.rs`.
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

#[test]
fn a_png_opens_in_image_mode_with_one_picture_over_its_name() {
    let dir = std::env::temp_dir().join(format!("zemacs_image_mode-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("plot.PNG"); // upper-case, since extensions are not case-sensitive to anyone
    std::fs::write(&png, one_pixel_png()).unwrap();

    let init = dir.join("init.lisp");
    std::fs::write(
        &init,
        format!(
            "(in-package :zemacs)\n(load {:?} :verbose nil :print nil)\n(message \"image mode test init loaded\")\n",
            runtime("modes/modes.lisp"),
        ),
    )
    .unwrap();
    let (_tx, _rx) = crossbeam_channel::unbounded();
    let shared: Shared = Default::default();
    let lisp = Arc::new(zemacs_lisp::spawn(_tx, shared.clone(), init));
    pump(shared.clone(), lisp.clone());
    wait(&shared, "the init file", |ed| {
        ed.messages.iter().any(|m| m == "image mode test init loaded").then_some(())
    });

    // What `open_file` does for a file that is not text.
    {
        let mut ed = shared.lock().unwrap();
        ed.load("plot.PNG", Some(png.clone()), None);
        ed.apply(EditorCommand::SetReadOnly(true));
    }
    wait(&shared, "image-mode from the extension", |ed| {
        (ed.buffer.major_mode == "image-mode").then_some(())
    });
    wait(&shared, "one image overlay over the name", |ed| {
        let images: Vec<_> = ed.buffer.overlays().iter().filter(|o| o.image.is_some()).collect();
        (images.len() == 1 && images[0].start == 0 && images[0].end == ed.buffer.len_chars())
            .then_some(())
    });
    let ed = shared.lock().unwrap();
    assert_eq!(ed.buffer.read_only(), ReadOnly::Claimed, "a picture's name must not be typed into");
    assert_eq!(ed.buffer.text.to_string(), "plot.PNG");
    drop(ed);

    let _ = std::fs::remove_dir_all(&dir);
}

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
    png.extend(chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0]));
    png.extend(chunk(b"IDAT", &z));
    png.extend(chunk(b"IEND", &[]));
    png
}
