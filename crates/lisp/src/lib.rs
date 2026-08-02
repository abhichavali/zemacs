//! zemacs-lisp — the embedded Common Lisp image.
//!
//! zemacs is "a Common Lisp machine that edits text": ECL is the extension and
//! configuration language, the way elisp is for Emacs. The image is *not* a
//! scripting sidecar — `init.lisp` is the config file, and any function defined
//! there can be named by a keybinding or a dashboard item.
//!
//! ECL brings its own conservative GC and wants a thread it can register, so it
//! gets a dedicated one, and the editor can call back into the image
//! (`CallLisp`, `M-x`-style eval) without ever running Lisp on the UI thread.
//!
//! # Reading and writing editor state
//!
//! The image shares the [`Editor`] with the main thread through a mutex, and a
//! primitive takes that lock for the duration of *one primitive* — long enough
//! to read a slice or apply an edit, never longer.
//!
//! **Lisp is deliberately not synchronous with the command loop**, which is the
//! single biggest thing wrong with elisp: there, a slow function freezes
//! redisplay and input until it returns. Here nothing waits on a Lisp form.
//! Input keeps being handled and frames keep being drawn while it runs, because
//! the only thing it ever holds is a lock nobody keeps across a redraw. A
//! runaway `(loop)` in your config costs you that one command, not the editor.
//!
//! What that buys costs one guarantee: a Lisp function is **not atomic** with
//! respect to the user. Two primitives can have a keystroke land between them.
//! That is the right default — a command that reads point, thinks, and inserts
//! is doing what you meant — and where a sequence genuinely must not be
//! interleaved, the fix is to make it *one* primitive rather than to hold a
//! lock across arbitrary Lisp: `replace-region` is that, and is why it exists
//! next to `delete-region` and `insert`.
//!
//! The exception is the handful of commands that need the app layer rather than
//! the core: opening a file, git, dired, closing an OS window. Those cannot run
//! on this thread, so they go down a channel and land on the next turn of the
//! main loop — see [`EditorCommand::needs_app`].
//!
//! The Lisp-facing ABI lives in `shim.c` rather than in `extern "C"` blocks
//! here: ECL's API is largely macros (`ECL_NIL`, `cl_object` tagging), which
//! don't survive translation to Rust declarations.
//!
//! The editor handle and the channel sender live in a process-wide static
//! because ECL's C-function ABI carries no user-data pointer. **Process-wide,
//! and not a thread-local**, so that a primitive means the same thing on every
//! thread in the image: ECL has threads of its own
//! (`mp:process-run-function`), a config that starts one is entitled to call
//! `message` from it, and with the host installed on one thread only that call
//! would reach nothing at all — no error, no message, and no way to find out.
//! Silence is the worst failure a primitive can have.
//!
//! What it costs is nothing new: the lock is per primitive, exactly as it is
//! for the Lisp thread, so a form running on a worker thread is interleaved
//! with the editor in precisely the way [`Lisp`]'s own forms already are. The
//! rule stays what `docs/threading.org` says — when a sequence must be
//! all-or-nothing, make it one primitive.
//!
//! The Lisp-facing reference — every primitive, reader and helper, the recipes,
//! and the list of what is deliberately out of reach — lives in `docs/` at the
//! repository root.

use std::ffi::{c_char, c_double, c_int, c_long, CStr, CString};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};

use crossbeam_channel::Sender;
use zemacs_core::{Editor, EditorCommand, HlKind, Image, ImageId, OverlayEdit, Shared};

extern "C" {
    fn zemacs_boot();
    fn zemacs_load_init(path: *const c_char);
    fn zemacs_eval(src: *const c_char);
}

/// What a primitive has to work with. Both halves are `Sync`, which is what
/// lets one static serve every thread in the image.
struct Host {
    editor: Shared,
    /// For the few commands only the app layer can carry out.
    tx: Sender<EditorCommand>,
}

/// Installed once, by [`spawn`], and read from whichever thread a primitive
/// happens to be called on — the boot thread, or one ECL made itself.
static HOST: OnceLock<Host> = OnceLock::new();

/// Run `f` with the editor locked.
///
/// A poisoned lock is stepped over rather than propagated: the main thread
/// having panicked mid-edit is bad, but it is not a reason for every subsequent
/// keystroke to panic too. `None` before [`spawn`] has installed the host.
fn with_editor<T>(f: impl FnOnce(&mut Editor) -> T) -> Option<T> {
    let host = HOST.get()?;
    let mut guard = host.editor.lock().unwrap_or_else(|e| e.into_inner());
    Some(f(&mut guard))
}

/// Pure commands are applied on the spot, so a read that follows one sees it.
/// The rest need the app layer and land on the next turn of the main loop.
fn emit(cmd: EditorCommand) {
    if cmd.needs_app() {
        if let Some(host) = HOST.get() {
            let _ = host.tx.send(cmd);
        }
        return;
    }
    with_editor(|ed| ed.apply(cmd));
}

/// Answer a query as Lisp source for the shim to READ — see
/// [`zemacs_core::query`] for why it is source rather than a tagged value.
/// `"nil"` covers every failure, so a query can never panic a Lisp form.
fn ask(name: &str, a: i64, b: i64) -> String {
    if let Some(answer) = ask_here(name) {
        return answer;
    }
    with_editor(|ed| zemacs_core::query::query(ed, name, a, b)).unwrap_or_else(|| "nil".into())
}

/// The readers core cannot answer, because answering them needs a crate that
/// depends *on* core.
///
/// There is exactly one, and it is a real question about the live buffer rather
/// than a special case waiting to become a family: org's LaTeX fragments live in
/// `zemacs-syntax`, which is downstream of `zemacs-core`, so the arm cannot go
/// in `query.rs` without a dependency cycle. It answers in the same shape as
/// everything there and is spelled like every other reader from Lisp; only the
/// file it lives in differs.
///
/// The buffer text is copied under the lock and scanned outside it: the scan is
/// a pass over the whole buffer, and no keystroke should wait behind one.
fn ask_here(name: &str) -> Option<String> {
    if name != "latex-fragments" {
        return None;
    }
    let text = with_editor(|ed| ed.buffer.text.to_string())?;
    let rows: Vec<String> = zemacs_syntax::latex_fragments(&text)
        .iter()
        .map(|f| format!("({} {} {})", f.start, f.end, if f.display { "t" } else { "nil" }))
        .collect();
    Some(format!("({})", rows.join(" ")))
}

/// Owned copy of a shim string. `NULL` means the Lisp argument was `NIL`.
unsafe fn opt_str(p: *const c_char) -> Option<String> {
    (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
}

unsafe fn str_or_empty(p: *const c_char) -> String {
    opt_str(p).unwrap_or_default()
}

// --- Primitives called from shim.c ---------------------------------------
// One per EditorCommand whose arguments do not fit the `%do` envelope — a
// float, three floats, a handle coming back. The rest go through
// [`command_for`] further down, dispatched on a verb the way `%query` is.
// Argument checking and type coercion already happened on the C side.

#[no_mangle]
pub extern "C" fn rs_set_font_size(size: c_double) {
    emit(EditorCommand::SetFontSize(size as f32));
}

#[no_mangle]
pub extern "C" fn rs_set_background(r: c_double, g: c_double, b: c_double) {
    emit(EditorCommand::SetBackground([r as f32, g as f32, b as f32]));
}

#[no_mangle]
pub extern "C" fn rs_set_foreground(r: c_double, g: c_double, b: c_double) {
    emit(EditorCommand::SetForeground([r as f32, g as f32, b as f32]));
}

#[no_mangle]
pub extern "C" fn rs_set_syntax_color(face: *const c_char, r: c_double, g: c_double, b: c_double) {
    let face = unsafe { str_or_empty(face) };
    emit(EditorCommand::SetSyntaxColor(
        face,
        [r as f32, g as f32, b as f32],
    ));
}

#[no_mangle]
pub extern "C" fn rs_set_line_numbers(on: c_int) {
    emit(EditorCommand::SetLineNumbers(on != 0));
}

#[no_mangle]
pub extern "C" fn rs_set_tab_width(n: c_long) {
    emit(EditorCommand::SetTabWidth(n.max(0) as usize));
}

/// Columns to hold the text column to, centred in the pane. 0 is off, and a
/// negative is the same thing said clumsily rather than an error.
#[no_mangle]
pub extern "C" fn rs_set_text_width(n: c_long) {
    emit(EditorCommand::SetTextWidth(n.max(0) as usize));
}

/// Signed on purpose: the sign *is* the feature, exactly as in Emacs' `:box
/// :line-width` — positive raises the modeline, negative sinks it. Core clamps
/// the magnitude, so nothing is rejected here.
#[no_mangle]
pub extern "C" fn rs_set_modeline_relief(n: c_long) {
    emit(EditorCommand::SetModelineRelief(n as i32));
}

#[no_mangle]
pub extern "C" fn rs_set_modeline_pad(n: c_long) {
    emit(EditorCommand::SetModelinePad(n as i32));
}

#[no_mangle]
pub extern "C" fn rs_message(text: *const c_char) {
    emit(EditorCommand::Message(unsafe { str_or_empty(text) }));
}

#[no_mangle]
pub extern "C" fn rs_quit() {
    emit(EditorCommand::Quit);
}

#[no_mangle]
pub extern "C" fn rs_dashboard_banner(text: *const c_char) {
    emit(EditorCommand::SetDashboardBanner(unsafe {
        str_or_empty(text)
    }));
}

/// The picture above the banner, by the id `image-file` answered. `0` — which
/// is what that primitive returns when it could not read the file — takes the
/// logo away rather than leaving a dangling id, so `(dashboard-logo
/// (image-file "gone.png"))` degrades to the text banner instead of a blank.
#[no_mangle]
pub extern "C" fn rs_dashboard_logo(id: c_long) {
    emit(EditorCommand::SetDashboardLogo(
        (id > 0).then_some(id as ImageId),
    ));
}

#[no_mangle]
pub extern "C" fn rs_clear_dashboard_items() {
    emit(EditorCommand::ClearDashboardItems);
}

#[no_mangle]
pub extern "C" fn rs_dashboard_item(key: *const c_char, label: *const c_char, action: *const c_char) {
    // The shim PRINCs the key, so `#\f` and `"f"` arrive identically.
    let key = unsafe { str_or_empty(key) }.chars().next().unwrap_or('?');
    emit(EditorCommand::AddDashboardItem {
        key,
        label: unsafe { str_or_empty(label) },
        action: unsafe { str_or_empty(action) },
    });
}

#[no_mangle]
pub extern "C" fn rs_define_key(mode: *const c_char, keys: *const c_char, command: *const c_char) {
    emit(EditorCommand::BindKey {
        mode: unsafe { str_or_empty(mode) },
        keys: unsafe { str_or_empty(keys) },
        command: unsafe { str_or_empty(command) },
    });
}

#[no_mangle]
pub extern "C" fn rs_find_file(path: *const c_char) {
    emit(EditorCommand::OpenFile(PathBuf::from(unsafe {
        str_or_empty(path)
    })));
}

#[no_mangle]
pub extern "C" fn rs_save_file(path: *const c_char) {
    emit(EditorCommand::SaveFile(
        unsafe { opt_str(path) }.map(PathBuf::from),
    ));
}

#[no_mangle]
pub extern "C" fn rs_show_dashboard() {
    emit(EditorCommand::ShowDashboard);
}

#[no_mangle]
pub extern "C" fn rs_insert(text: *const c_char) {
    // Checkpoint first: text a Lisp command inserts is one user-level edit, so
    // a single `u` must take it back out. Without this, undo would jump past it
    // to whenever the user last typed.
    emit(EditorCommand::Checkpoint);
    emit(EditorCommand::InsertText(unsafe { str_or_empty(text) }));
}

#[no_mangle]
pub extern "C" fn rs_set_completion_style(style: *const c_char) {
    // Core owns the vocabulary (and its aliases); an unknown name is ignored
    // there rather than being an error here.
    emit(EditorCommand::SetCompletionStyle(unsafe {
        str_or_empty(style)
    }));
}

#[no_mangle]
pub extern "C" fn rs_clear_commands() {
    emit(EditorCommand::ClearCommands);
}

#[no_mangle]
pub extern "C" fn rs_set_line_overflow(mode: *const c_char) {
    emit(EditorCommand::SetLineOverflow(unsafe { str_or_empty(mode) }));
}

#[no_mangle]
pub extern "C" fn rs_set_relative_line_numbers(on: c_int) {
    emit(EditorCommand::SetRelativeLineNumbers(on != 0));
}

#[no_mangle]
pub extern "C" fn rs_set_major_mode(name: *const c_char) {
    emit(EditorCommand::SetMajorMode(unsafe { str_or_empty(name) }));
}

/// The modes whose buffers show no gutter, space-separated in one string. The
/// whole list every time, so setting it twice replaces rather than accumulates
/// — a config reload has to be idempotent.
#[no_mangle]
pub extern "C" fn rs_set_no_gutter_modes(modes: *const c_char) {
    emit(EditorCommand::SetNoGutterModes(unsafe {
        str_or_empty(modes)
    }));
}

#[no_mangle]
pub extern "C" fn rs_set_minor_mode(name: *const c_char, on: c_int) {
    emit(EditorCommand::SetMinorMode(
        unsafe { str_or_empty(name) },
        on != 0,
    ));
}

#[no_mangle]
pub extern "C" fn rs_register_command(name: *const c_char) {
    emit(EditorCommand::RegisterCommand(unsafe { str_or_empty(name) }));
}

// --- reading and writing the buffer ---------------------------------------

/// Answer `name`, as Lisp source. The caller owns the result and must hand it
/// back to [`rs_free_string`] — it is a Rust allocation, so `free(3)` is wrong.
#[no_mangle]
pub extern "C" fn rs_query(name: *const c_char, a: c_long, b: c_long) -> *mut c_char {
    let name = unsafe { str_or_empty(name) };
    let src = ask(&name, a as i64, b as i64);
    // Interior NULs cannot come out of `query`, but a fallback beats a panic
    // across an FFI boundary if that ever stops being true.
    CString::new(src)
        .unwrap_or_else(|_| CString::new("nil").expect("literal has no NUL"))
        .into_raw()
}

#[no_mangle]
pub extern "C" fn rs_free_string(p: *mut c_char) {
    if !p.is_null() {
        drop(unsafe { CString::from_raw(p) });
    }
}

/// Highlight a *string* as a language, as `((START END "face") ...)` in char
/// offsets. Rust-owned memory, freed by [`rs_free_string`] like [`rs_query`].
///
/// The reader `%query` could not carry, and the reason is the envelope rather
/// than the question: `%query` takes a name and two integers, and this takes two
/// strings. It is the same *shape* as `latex-fragments` — a fact `zemacs-syntax`
/// knows and `zemacs-core` cannot, answered in Lisp source — and it earns a C
/// entry point for the same reason `image-file` does, that it has an argument
/// the envelope has no room for.
///
/// **It is a pure function and takes no lock at all.** Nothing here reads the
/// editor: the caller hands over text it has already got out of the buffer, so a
/// keystroke never waits behind a parse. That is what makes it affordable to
/// highlight every source block in a document on entry to a mode.
///
/// The interesting use is the one org has: a `#+begin_src python` block inside a
/// buffer whose *own* language is org, which the buffer-wide highlighter cannot
/// colour because there is one language per buffer and no grammar for org at
/// all. Lisp finds the block, hands the body over as `python`, and turns the
/// answer into one `face` overlay per run — and a nested block is highlighted by
/// exactly the grammar that would colour the same text in a file of its own.
///
/// An unknown language answers `nil`, which is also what a parse failure and an
/// unhighlighted run answer, so the caller has one thing to check.
#[no_mangle]
pub extern "C" fn rs_highlight(lang: *const c_char, text: *const c_char) -> *mut c_char {
    let lang = unsafe { str_or_empty(lang) };
    let text = unsafe { str_or_empty(text) };
    let rows: Vec<String> = zemacs_syntax::highlight(&lang, &text)
        .iter()
        .map(|s| format!("({} {} \"{}\")", s.start, s.end, s.kind.name()))
        .collect();
    CString::new(format!("({})", rows.join(" ")))
        .unwrap_or_else(|_| CString::new("nil").expect("literal has no NUL"))
        .into_raw()
}

#[no_mangle]
pub extern "C" fn rs_goto_char(n: c_long) {
    emit(EditorCommand::MoveTo(n.max(0) as usize));
}

#[no_mangle]
pub extern "C" fn rs_delete_region(a: c_long, b: c_long) {
    let (start, end) = ordered(a, b);
    emit(EditorCommand::Checkpoint);
    emit(EditorCommand::DeleteRange(start, end));
}

/// Delete and insert as one undo step, with no window for a keystroke to land
/// in between — the atomic primitive the module docs point at. Without it,
/// "wrap the region in `*`" is two commands and a race.
#[no_mangle]
pub extern "C" fn rs_replace_region(a: c_long, b: c_long, text: *const c_char) {
    let text = unsafe { str_or_empty(text) };
    let (start, end) = ordered(a, b);
    with_editor(|ed| {
        ed.apply(EditorCommand::Checkpoint);
        ed.apply(EditorCommand::DeleteRange(start, end));
        // The cursor is set straight rather than through `MoveTo`, because
        // every `apply` ends in the Normal-mode clamp — point may not sit past
        // the last character of a line. A replacement whose range reached the
        // end of the buffer would therefore be pulled back one and insert a
        // character early: `(foo bar) baz` slurped came out `(foo ba baz)r`.
        // Inserting is not a motion, so the clamp has no business in it.
        ed.buffer.cursor = start.min(ed.buffer.len_chars());
        ed.apply(EditorCommand::InsertText(text));
    });
}

// --- markers ---------------------------------------------------------------
// A marker is not a document change, so these go straight at the editor rather
// than through a command — and `make-marker` has to answer with the handle it
// allocated, which a command could not do.

/// The handle, or 0 — never a live marker — if there is no editor to make one in.
#[no_mangle]
pub extern "C" fn rs_make_marker(pos: c_long, advance: c_int) -> c_long {
    let insertion = match advance {
        0 => zemacs_core::Insertion::Stay,
        _ => zemacs_core::Insertion::Advance,
    };
    with_editor(|ed| ed.make_marker(pos.max(0) as usize, insertion)).unwrap_or(0) as c_long
}

#[no_mangle]
pub extern "C" fn rs_set_marker(id: c_long, pos: c_long) {
    with_editor(|ed| ed.set_marker(id.max(0) as u64, pos.max(0) as usize));
}

#[no_mangle]
pub extern "C" fn rs_delete_marker(id: c_long) {
    with_editor(|ed| ed.delete_marker(id.max(0) as u64));
}

// --- overlays and inline images ------------------------------------------
// Only two primitives, for the only two things in this area that have to answer
// with something: a handle, and an image id. Every *change* to an overlay fits
// the `%do` envelope and is an arm of `command_for` below.

/// The handle, or 0 — never a live overlay — for an empty range or no editor.
#[no_mangle]
pub extern "C" fn rs_make_overlay(start: c_long, end: c_long) -> c_long {
    let (start, end) = ordered(start, end);
    with_editor(|ed| ed.make_overlay(start, end)).unwrap_or(0) as c_long
}

/// Render one LaTeX fragment and remember the pixels; the answer is the id an
/// `image` overlay property takes. 0 means it did not render, and the reason has
/// already gone to the status line.
///
/// Two short locks with the TeX run *between* them, never inside one. A cold
/// render is a few hundred milliseconds of shelling out, and holding the editor
/// across that would freeze the display — the one thing the threading contract
/// promises never happens. Lisp waits; nothing else does.
///
/// The id is a hash of exactly the inputs that decide the pixels, so previewing
/// the same fragment again is a lookup: no TeX, no upload, and the renderer's
/// texture for it is still there. ponytail: masked to 48 bits, because an ECL
/// fixnum is 62 and the handle has to survive the trip through Lisp as an
/// ordinary integer. A collision would show the wrong equation and needs about
/// 2^24 distinct fragments in one session to become likely.
#[no_mangle]
pub extern "C" fn rs_latex_preview(source: *const c_char) -> c_long {
    use std::hash::{Hash, Hasher};

    let source = unsafe { str_or_empty(source) };
    // The em in device pixels, which the renderer parks on the editor, and the
    // colour body text is drawn in — a preview that does not match both reads as
    // a pasted screenshot rather than as part of the line.
    let Some((px, color)) = with_editor(|ed| {
        let rgb = ed.theme.color(HlKind::Default, ed.settings.foreground);
        let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        (ed.font_px, (byte(rgb[0]), byte(rgb[1]), byte(rgb[2])))
    }) else {
        return 0;
    };
    let dpi = zemacs_latex::dpi_for_em(px);

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (&source, dpi, color).hash(&mut hasher);
    let id = (hasher.finish() & 0xFFFF_FFFF_FFFF).max(1);
    if with_editor(|ed| ed.has_image(id)) == Some(true) {
        return id as c_long;
    }

    match zemacs_latex::render(&source, dpi, color) {
        Ok(preview) => {
            with_editor(|ed| {
                ed.add_image(
                    id,
                    Image {
                        width: preview.width,
                        height: preview.height,
                        depth: preview.depth,
                        rgba: preview.rgba,
                    },
                )
            });
            id as c_long
        }
        // `{:#}` flattens anyhow's context chain onto one line, which is the
        // shape a status line wants.
        Err(e) => {
            emit(EditorCommand::Message(format!("latex: {e:#}")));
            0
        }
    }
}

/// Read an image *file* and remember its pixels; the answer is the id an
/// `image` overlay property takes, the same one [`rs_latex_preview`] answers
/// with. 0 means it did not load, and the reason has already gone to the status
/// line.
///
/// The second half of the display path org needed. LaTeX was its only producer,
/// so `[[file:fig-1.svg]]` had nowhere to get an id from; this is that, and
/// nothing more. Note what it deliberately does *not* add: no second image map,
/// no second overlay property, no second lifetime. A figure lands in
/// `Editor::add_image` beside every equation and is pruned by the same pass, so
/// the renderer's texture cache has one story about eviction rather than two.
///
/// `ems` is the widest the figure may be drawn, as **a hundredth of an em** —
/// the percentage the whole overlay bridge carries integers in, and the same
/// trick `overlay-scale` plays for the same reason. 0 means "as authored". It
/// is resolved here rather than in Lisp because the em in *device* pixels is
/// something only the renderer knows, and it parks it on the editor for exactly
/// this: a figure sized from `(font-size)` in the image would come out half
/// width on a Retina display.
///
/// The same shape of id as a LaTeX preview: a hash of exactly the inputs that
/// decide the pixels — the path, the file's length and mtime, and the width it
/// was rendered to. Editing `fig-1.svg` and previewing again therefore renders
/// it afresh, and re-previewing an unchanged buffer is a lookup with no file
/// read at all. ponytail: mtime and length rather than the bytes, so a figure
/// rewritten within the filesystem's timestamp granularity and to exactly the
/// same length keeps its old pixels. Hashing the content would mean reading the
/// file to decide whether to read the file.
///
/// One short lock to read the metrics, the decode *between* the locks, and a
/// short one to store — never the editor held across a rasterise, exactly as
/// the LaTeX primitive does it.
#[no_mangle]
pub extern "C" fn rs_image_file(path: *const c_char, ems: c_long) -> c_long {
    use std::hash::{Hash, Hasher};

    let path = PathBuf::from(unsafe { str_or_empty(path) });
    if path.as_os_str().is_empty() {
        return 0;
    }
    // The em in device pixels, and the ratio between it and the point size Lisp
    // asked for — which *is* the display's scale factor, and is the only place
    // it can be recovered. A vector figure has to be rasterised at it or it
    // comes out at half size next to text that was not.
    let Some((px, scale)) = with_editor(|ed| {
        let nominal = ed.settings.font_size.max(1.0);
        (ed.font_px, (ed.font_px / nominal).clamp(0.25, 8.0))
    }) else {
        return 0;
    };
    let max_width = ((px * ems.max(0) as f32) / 100.0).round().max(0.0) as u32;

    // Metadata failures are left to `zemacs_figure::load`, which reports the
    // path; hashing zeroes here just means an unreadable file gets a stable id
    // and fails again the same way.
    let stamp = std::fs::metadata(&path)
        .map(|m| {
            let secs = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            (m.len(), secs)
        })
        .unwrap_or((0, 0));

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (&path, stamp, max_width, scale.to_bits()).hash(&mut hasher);
    let id = (hasher.finish() & 0xFFFF_FFFF_FFFF).max(1);
    if with_editor(|ed| ed.has_image(id)) == Some(true) {
        return id as c_long;
    }

    match zemacs_figure::load(&path, max_width, scale) {
        Ok(fig) => {
            with_editor(|ed| {
                ed.add_image(
                    id,
                    Image {
                        width: fig.width,
                        height: fig.height,
                        // A figure sits *on* the baseline. Only a LaTeX fragment
                        // has a depth, because only a fragment is set inline with
                        // the words around it.
                        depth: 0,
                        rgba: fig.rgba,
                    },
                )
            });
            id as c_long
        }
        Err(e) => {
            emit(EditorCommand::Message(format!("image: {e:#}")));
            0
        }
    }
}

// --- the write half of `%query` -------------------------------------------

/// Turn a verb into a command, the way [`zemacs_core::query`] turns one into an
/// answer.
///
/// ponytail: one primitive for the whole tail of the command set rather than a
/// C function, an `extern`, a `defprim` and an export apiece. What each of these
/// has in common is that Lisp needs no value back and the arguments fit in one
/// optional string and two integers — the same envelope `%query` already lives
/// in. Anything outside it (a marker handle coming back, an f32 triple going
/// out) still gets a primitive of its own, which is why `%make-marker` and
/// `set-background` are not here.
///
/// `None` for a verb this build has never heard of; the caller reports it.
fn command_for(verb: &str, arg: String, a: i64, b: i64) -> Option<EditorCommand> {
    use zemacs_core::frame::Split;
    let n = a.max(0) as usize;
    Some(match verb {
        "undo" => EditorCommand::Undo,
        "redo" => EditorCommand::Redo,
        // Index into `buffer-list`; 0 is the live buffer and means nothing.
        "switch-buffer" => EditorCommand::SwitchBuffer(n),

        // Named after the result rather than the divider, as core does: vim and
        // Emacs disagree about what a "horizontal split" is, and nobody
        // disagrees about "right".
        "split-window-right" => EditorCommand::SplitWindow(Split::Columns),
        "split-window-below" => EditorCommand::SplitWindow(Split::Rows),
        "delete-window" => EditorCommand::CloseWindow,
        "other-window" => EditorCommand::FocusNextWindow,
        "select-window" => EditorCommand::FocusWindow(n),
        "new-frame" => EditorCommand::NewFrame,
        "delete-frame" => EditorCommand::CloseFrame,
        "select-frame" => EditorCommand::FocusFrame(n),
        "scroll" => EditorCommand::ScrollLines(a.clamp(i32::MIN as i64, i32::MAX as i64) as i32),

        // The register — vim's unnamed one, which `p` pastes from.
        "yank" | "yank-lines" => {
            let (start, end) = ordered(a as c_long, b as c_long);
            EditorCommand::Yank {
                start,
                end,
                linewise: verb == "yank-lines",
            }
        }
        "set-register" => EditorCommand::SetRegister {
            text: arg,
            linewise: a != 0,
        },
        "paste" => EditorCommand::Paste { after: a != 0 },

        // Backends core has none of. These are the verbs the enum documents;
        // an unknown one is the backend's to complain about, not ours.
        "git" => EditorCommand::Git(arg),
        "dired" => EditorCommand::Dired(arg),
        "term" => EditorCommand::Term(arg),
        // `path:line:text`, as ripgrep prints it. The only way to open a file
        // *and* land on a line: `find-file` reaches the app a frame later, so a
        // `goto-char` after it would move the cursor in the buffer you left.
        "open-at" => EditorCommand::OpenAt(arg),

        // --- overlays -------------------------------------------------------
        //
        // `a` is always the handle. A colour is a *face name* rather than three
        // floats, which is what keeps these inside the envelope and out of C —
        // and means an overlay follows a theme change for free. An unknown or
        // empty name clears the attribute rather than erroring, so
        // `(overlay-put ov 'face nil)` is the way to take one off.
        "overlay-face" => {
            EditorCommand::Overlay(OverlayEdit::Face(a.max(0) as u64, HlKind::from_name(&arg)))
        }
        "overlay-background" => EditorCommand::Overlay(OverlayEdit::Background(
            a.max(0) as u64,
            HlKind::from_name(&arg),
        )),
        // An empty string clears it. A `display` of "" would hide the text and
        // show nothing, which is what `(overlay-put ov 'display "")` should mean
        // in Emacs too, but a config that wants that can say so with a space.
        "overlay-display" => EditorCommand::Overlay(OverlayEdit::Display(
            a.max(0) as u64,
            (!arg.is_empty()).then_some(arg),
        )),
        "overlay-image" => EditorCommand::Overlay(OverlayEdit::Image(
            a.max(0) as u64,
            (b > 0).then_some(b as u64),
        )),

        // --- typesetting -----------------------------------------------------
        //
        // Size, weight and slant, which between them are what let a heading be
        // drawn as a heading rather than as a coloured line of text. Nothing
        // here has an opinion about *what* is a heading: `1.5` and `bold` arrive
        // from Lisp, the renderer snaps the size to one of the handful of steps
        // it will open a face at, and that is the whole of the mechanism.
        //
        // A percentage in `b` rather than a float, because the envelope carries
        // integers — and the renderer was going to quantise it anyway. 0 clears,
        // which is how `(overlay-put ov 'scale nil)` arrives.
        "overlay-scale" => EditorCommand::Overlay(OverlayEdit::Scale(
            a.max(0) as u64,
            (b > 0).then(|| b.min(u16::MAX as i64) as u16),
        )),
        // Emacs' `:weight` and `:slant`, spelled the same way and with the same
        // three answers. The middle one matters: `'normal` is a claim that
        // outranks an earlier overlay's bold, and NIL is the absence of one.
        "overlay-weight" => {
            EditorCommand::Overlay(OverlayEdit::Bold(a.max(0) as u64, emphasis(&arg, "bold")))
        }
        "overlay-slant" => EditorCommand::Overlay(OverlayEdit::Italic(
            a.max(0) as u64,
            emphasis(&arg, "italic"),
        )),
        // The two line-level attributes. A band the width of the pane and a
        // string in front of the text — between them, a source block drawn as a
        // block. Same face-name vocabulary as `overlay-background`.
        "overlay-line-background" => EditorCommand::Overlay(OverlayEdit::LineBackground(
            a.max(0) as u64,
            HlKind::from_name(&arg),
        )),
        "overlay-line-prefix" => EditorCommand::Overlay(OverlayEdit::LinePrefix(
            a.max(0) as u64,
            (!arg.is_empty()).then_some(arg),
        )),
        // Code folding, and the only overlay property that is about *lines*:
        // the lines after the overlay's first stop occupying rows entirely.
        // `b` rather than `arg` because the value is a flag, so one overlay can
        // be folded and unfolded without being remade — which is what org's
        // headline cycling does.
        "overlay-fold" => EditorCommand::Overlay(OverlayEdit::Fold(a.max(0) as u64, b != 0)),
        "delete-overlay" => EditorCommand::Overlay(OverlayEdit::Delete(a.max(0) as u64)),
        "remove-overlays" => {
            let (start, end) = ordered(a as c_long, b as c_long);
            EditorCommand::Overlay(OverlayEdit::RemoveIn(start, end))
        }

        // --- lisp-api: the verbs that closed `boundary.org`'s table ----------
        //
        // Every one of these fits the envelope, which is the point: the table
        // said "a new `EditorCommand` variant", and a new variant is *all* it
        // needed. Nothing below is a C primitive.

        // A buffer with no file behind it. Idempotent by name, so calling it
        // twice shows one buffer rather than stacking a second.
        "create-buffer" => EditorCommand::CreateBuffer(arg),
        // Index into `buffer-list`; 0 is the live buffer, which is the useful
        // default and the opposite of `switch-buffer`'s.
        "kill-buffer" => EditorCommand::KillBuffer(n),
        // An empty name means plain text — `(set-language nil)`.
        "set-language" => EditorCommand::SetLanguage((!arg.is_empty()).then_some(arg)),
        // A flag in `a` rather than a word in `arg`, the shape `overlay-fold`
        // already has and for the same reason: it is a claim that is turned on
        // and off, and a mode's exit hook has to be able to say the other one.
        "set-read-only" => EditorCommand::SetReadOnly(a != 0),

        // `read-string` and `completing-read`. `a` is the continuation id the
        // image parked its closure under, `b` says whether to draw a candidate
        // box, and the candidates themselves follow one at a time: the write
        // envelope carries a single string, and a list is many.
        "read-from-minibuffer" => EditorCommand::ReadFromMinibuffer {
            id: a.max(0) as u64,
            label: arg,
            completing: b != 0,
        },
        "prompt-item" => EditorCommand::PromptItem(arg),

        // One row of the which-key panel, empty string to clear it — the same
        // one-string-at-a-time shape `prompt-item` has, and for the same reason.
        // A row is `"KEY LABEL"`; the renderer splits at the first space, which
        // is unambiguous because a normalised key token never contains one.
        "which-key" => EditorCommand::WhichKey((!arg.is_empty()).then_some(arg)),

        // A keystroke for the shell. `Key::from_token` is the inverse of the
        // spelling `key-bindings` already reports, so the string a config reads
        // out of one is the string it can send — no second vocabulary.
        "term-key" => match zemacs_core::Key::from_token(&arg) {
            Some(key) => EditorCommand::TermKey(key),
            None => EditorCommand::Message(format!("no such key: {arg}")),
        },
        // --- end of the lisp-api block ---------------------------------------
        _ => return None,
    })
}

/// A `weight` or `slant` value, as the three answers Emacs' face attributes
/// have: `on` for the emphatic spelling, `false` for a plain one, `None` for
/// anything else.
///
/// `None` for the empty string is what makes `(overlay-put ov 'weight nil)`
/// clear the attribute, matching `face`; `None` for an unrecognised word is the
/// same forgiveness `HlKind::from_name` shows, and for the same reason — the
/// value came out of a config and must not be able to raise an error here.
fn emphasis(arg: &str, on: &str) -> Option<bool> {
    match arg.trim().to_ascii_lowercase().as_str() {
        s if s == on => Some(true),
        // Every spelling of "deliberately not". `regular` and `roman` are what
        // a font family calls the plain cut; `upright` is what CSS calls it.
        "normal" | "regular" | "roman" | "upright" => Some(false),
        _ => None,
    }
}

#[no_mangle]
pub extern "C" fn rs_do(verb: *const c_char, arg: *const c_char, a: c_long, b: c_long) {
    let verb = unsafe { str_or_empty(verb) };
    let arg = unsafe { str_or_empty(arg) };
    // lisp-api: one verb answers a *batch* rather than a command, so it is
    // resolved here rather than in `command_for` — the same place `paste` gets
    // its checkpoint, and for the same reason: what has to happen is more than
    // one `EditorCommand`.
    //
    // This is "call a built-in verb by name", the row `boundary.org` had for
    // `run_action` being private. `M-x` and every key binding already resolve a
    // name this way; Lisp could name one and not run it. The lock is taken for
    // the resolution and *released* before anything is emitted, because `emit`
    // takes it again — and each command then goes wherever it belongs, so a
    // built-in verb that needs the app (`magit-status`, `find-file`) reaches it
    // down the channel exactly as `(magit "status")` does.
    if verb == "call" {
        let cmds = with_editor(|ed| ed.run_action(&arg)).unwrap_or_default();
        for cmd in cmds {
            emit(cmd);
        }
        return;
    }
    match command_for(&verb, arg, a as i64, b as i64) {
        // Same reason `insert` checkpoints: a paste a Lisp command makes is one
        // user-level edit, and one `u` has to take all of it back out.
        Some(cmd @ EditorCommand::Paste { .. }) => {
            emit(EditorCommand::Checkpoint);
            emit(cmd);
        }
        Some(cmd) => emit(cmd),
        // Silence would hide a typo and an error would stop a config loading.
        // A message is neither, and matches what `%query` does by answering NIL.
        None => emit(EditorCommand::Message(format!("no such command: {verb}"))),
    }
}

/// Open one of the editor's prompts.
///
/// Straight at the editor rather than through a command, for the same reason
/// `make-marker` is: a prompt is ephemeral UI state rather than a document
/// change, and there is no [`EditorCommand`] for it.
///
/// What the answer *does* is fixed by the kind — a file is opened, a command is
/// run, a line is jumped to. So this is "put one of the *editor's* pickers up".
/// The prompt whose answer comes back to Lisp is a different thing and a
/// different route: `read-string` and `completing-read`, built on the
/// `"read-from-minibuffer"` verb, which carries the continuation id.
#[no_mangle]
pub extern "C" fn rs_open_prompt(kind: *const c_char) {
    use zemacs_core::PromptKind;
    let name = unsafe { str_or_empty(kind) };
    let kind = match name.to_ascii_lowercase().as_str() {
        "ex" | ":" => PromptKind::Ex,
        "search" | "/" => PromptKind::Search,
        "command" | "m-x" => PromptKind::Command,
        "file" => PromptKind::File,
        "buffer" => PromptKind::Buffer,
        "line" => PromptKind::Line,
        "project-file" | "project" => PromptKind::ProjectFile,
        "grep" => PromptKind::Grep,
        _ => {
            emit(EditorCommand::Message(format!("no such prompt: {name}")));
            return;
        }
    };
    with_editor(|ed| ed.open_prompt(kind));
}

#[no_mangle]
pub extern "C" fn rs_set_evil_state(name: *const c_char) {
    let name = unsafe { str_or_empty(name) };
    match zemacs_core::Mode::from_name(&name) {
        Some(mode) => emit(EditorCommand::SetMode(mode)),
        None => emit(EditorCommand::Message(format!("no such state: {name}"))),
    }
}

// --- JSON-RPC subprocesses -------------------------------------------------
//
// Everything below is the whole Rust half of the language-server client, and of
// anything else that wants to talk to a long-lived child: five primitives, none
// of which knows what LSP is. The protocol — the handshake, which server serves
// which mode, what to do with a reply — is Lisp, in `runtime/rpc.lisp` and
// `runtime/lsp.lisp`, because that is the part every config wants to bend.
//
// Each of these earns a C primitive under the rule in `docs/boundary.org`:
// three of them must hand a value *back* (a connection handle, a request id, an
// escaped string), which neither `%do` nor `%query` can carry, and the other
// two carry more strings than `%do`'s single one.
//
// **How an answer reaches Lisp**, which is the only interesting question here:
// it does not come back through these calls at all. The reader thread pushes
// onto a channel; the *main* thread drains it once a frame and queues
// `(%rpc-event CONN KIND FORM)` for the Lisp thread — the same route a mode
// hook takes, and the only route that never runs Lisp on a foreign thread. The
// continuation waiting for that reply is a closure in a Lisp hash table, which
// is why nothing here matches a response to its request.

/// Answer the handle, or 0 — never a live connection — when the child could not
/// be started. The reason lands in the status line, as `%do` does for a verb it
/// has never heard of: a config naming a server that is not installed should
/// say so, not fail to load.
#[no_mangle]
pub extern "C" fn rs_rpc_start(program: *const c_char, args: *const c_char, cwd: *const c_char) -> c_long {
    let program = unsafe { str_or_empty(program) };
    let args = unsafe { str_or_empty(args) };
    let cwd = unsafe { opt_str(cwd) }.map(PathBuf::from);
    let args = match zemacs_rpc::parse_args(&args) {
        Ok(args) => args,
        Err(e) => {
            emit(EditorCommand::Message(format!("rpc-start {program}: {e}")));
            return 0;
        }
    };
    match zemacs_rpc::start(&program, &args, cwd.as_deref()) {
        Ok(conn) => conn as c_long,
        Err(e) => {
            emit(EditorCommand::Message(format!("rpc-start {program}: {e}")));
            0
        }
    }
}

/// The id a request was given, or 0 for a notification and for a failure. The
/// caller parks its continuation under that id.
#[no_mangle]
pub extern "C" fn rs_rpc_send(
    conn: c_long,
    method: *const c_char,
    params: *const c_char,
    want_reply: c_int,
) -> c_long {
    let method = unsafe { str_or_empty(method) };
    let params = unsafe { opt_str(params) };
    match zemacs_rpc::send(conn.max(0) as u32, &method, params.as_deref(), want_reply != 0) {
        Ok(id) => id.unwrap_or(0) as c_long,
        Err(e) => {
            emit(EditorCommand::Message(format!("rpc-send {method}: {e}")));
            0
        }
    }
}

/// Answer a request the *child* made. `id` is raw JSON rather than an integer
/// because JSON-RPC allows a string there, and the only honest way to echo one
/// back is not to interpret it.
#[no_mangle]
pub extern "C" fn rs_rpc_respond(
    conn: c_long,
    id: *const c_char,
    result: *const c_char,
    error: *const c_char,
) {
    let id = unsafe { str_or_empty(id) };
    let result = unsafe { opt_str(result) };
    let error = unsafe { opt_str(error) };
    if let Err(e) = zemacs_rpc::respond(conn.max(0) as u32, &id, result.as_deref(), error.as_deref()) {
        emit(EditorCommand::Message(format!("rpc-respond: {e}")));
    }
}

#[no_mangle]
pub extern "C" fn rs_rpc_stop(conn: c_long) {
    zemacs_rpc::stop(conn.max(0) as u32);
}

/// `text` as a JSON string literal, quotes included. Rust-owned; free it with
/// [`rs_free_string`].
///
/// The one piece of *encoding* that is not in Lisp, because the string it is
/// asked to escape most often is a whole buffer on its way into a
/// `textDocument/didChange`, and doing that a character at a time in the image
/// on every keystroke is exactly the "hot enough to be felt" case the boundary
/// rule is about.
#[no_mangle]
pub extern "C" fn rs_json_quote(text: *const c_char) -> *mut c_char {
    let text = unsafe { str_or_empty(text) };
    CString::new(zemacs_rpc::json_string(&text))
        .unwrap_or_else(|_| CString::new("\"\"").expect("literal has no NUL"))
        .into_raw()
}

// --- end of the JSON-RPC block ---------------------------------------------

/// Negative offsets clamp to 0, and the pair is put in order, so a range built
/// out of arithmetic in Lisp cannot panic the editor.
fn ordered(a: c_long, b: c_long) -> (usize, usize) {
    let (a, b) = (a.max(0) as usize, b.max(0) as usize);
    (a.min(b), a.max(b))
}

// --- Public API -----------------------------------------------------------

/// A handle on the running Common Lisp image.
///
/// Dropping it closes the request channel, which ends the Lisp thread's loop.
pub struct Lisp {
    tx: Sender<String>,
    _thread: JoinHandle<()>,
}

impl Lisp {
    /// Queue Common Lisp source for evaluation on the Lisp thread.
    ///
    /// Non-blocking, and infallible from the caller's point of view: read and
    /// evaluation errors are reported as [`EditorCommand::Message`] on the
    /// command channel, never returned or panicked.
    pub fn eval(&self, form: String) {
        let _ = self.tx.send(form);
    }
}

/// Boot ECL on a dedicated thread, register the host primitives, and load
/// `init_path`.
///
/// `editor` is shared with the caller: primitives lock it per call, so the main
/// thread must not hold it across anything slow (a vsync wait, say) or config
/// will stutter behind it.
///
/// A broken init file reports through `tx` as a `Message` and startup
/// continues — a typo in your config must not cost you your editor.
pub fn spawn(tx: Sender<EditorCommand>, editor: Shared, init_path: PathBuf) -> Lisp {
    let (req_tx, req_rx) = crossbeam_channel::unbounded::<String>();
    let thread = thread::Builder::new()
        .name("zemacs-lisp".into())
        .spawn(move || unsafe {
            // A second `spawn` in one process would find the cell already
            // taken and is ignored: `cl_boot` initialises a *process-wide*
            // image, so there is one of these per process by construction.
            let _ = HOST.set(Host { editor, tx });
            zemacs_boot();

            if let Ok(path) = CString::new(init_path.into_os_string().into_encoded_bytes()) {
                zemacs_load_init(path.as_ptr());
            }

            // Interior NULs are the only way this can fail; such a form could
            // never have been read anyway, so dropping it is the whole recovery.
            for src in req_rx {
                if let Ok(src) = CString::new(src) {
                    zemacs_eval(src.as_ptr());
                }
            }
        })
        .expect("failed to spawn lisp thread");

    Lisp {
        tx: req_tx,
        _thread: thread,
    }
}
