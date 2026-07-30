//! zemacs-lisp — the embedded Common Lisp image.
//!
//! zemacs is "a Common Lisp machine that edits text": ECL is the extension and
//! configuration language, the way elisp is for Emacs. The image is *not* a
//! scripting sidecar — `init.lisp` is the config file, and any function defined
//! there can be named by a keybinding or a dashboard item.
//!
//! ECL brings its own conservative GC and wants a thread it can register, so it
//! gets a dedicated one. That thread never touches editor state: the primitives
//! in `shim.c` translate Lisp calls into [`EditorCommand`]s and push them down a
//! channel to the main thread, which stays the single writer. After loading the
//! init file the thread blocks on a second channel of Lisp source strings, so
//! the editor can call back into the image (`CallLisp`, `M-x`-style eval)
//! without ever running Lisp on the UI thread.
//!
//! The Lisp-facing ABI lives in `shim.c` rather than in `extern "C"` blocks
//! here: ECL's API is largely macros (`ECL_NIL`, `cl_object` tagging), which
//! don't survive translation to Rust declarations.
//!
//! The channel sender lives in a thread-local because ECL's C-function ABI
//! carries no user-data pointer.

use std::cell::RefCell;
use std::ffi::{c_char, c_double, c_int, c_long, CStr, CString};
use std::path::PathBuf;
use std::thread::{self, JoinHandle};

use crossbeam_channel::Sender;
use zemacs_core::EditorCommand;

extern "C" {
    fn zemacs_boot();
    fn zemacs_load_init(path: *const c_char);
    fn zemacs_eval(src: *const c_char);
}

thread_local! {
    static SENDER: RefCell<Option<Sender<EditorCommand>>> = const { RefCell::new(None) };
}

fn emit(cmd: EditorCommand) {
    SENDER.with(|s| {
        if let Some(tx) = s.borrow().as_ref() {
            let _ = tx.send(cmd);
        }
    });
}

/// Owned copy of a shim string. `NULL` means the Lisp argument was `NIL`.
unsafe fn opt_str(p: *const c_char) -> Option<String> {
    (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
}

unsafe fn str_or_empty(p: *const c_char) -> String {
    opt_str(p).unwrap_or_default()
}

// --- Primitives called from shim.c ---------------------------------------
// One per EditorCommand the config language can produce. Argument checking and
// type coercion already happened on the C side.

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
/// A broken init file reports through `tx` as a `Message` and startup
/// continues — a typo in your config must not cost you your editor.
pub fn spawn(tx: Sender<EditorCommand>, init_path: PathBuf) -> Lisp {
    let (req_tx, req_rx) = crossbeam_channel::unbounded::<String>();
    let thread = thread::Builder::new()
        .name("zemacs-lisp".into())
        .spawn(move || unsafe {
            SENDER.with(|s| *s.borrow_mut() = Some(tx));
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
