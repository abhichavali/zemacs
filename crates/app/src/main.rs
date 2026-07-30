//! zemacs — the application.
//!
//! Owns the SDL2 event loop, the single mutable `Editor`, and one OS window per
//! frame; spawns the Common Lisp image and drains its commands each frame.
//!
//! Command flow, in one place: the keyboard, the mouse and Lisp all produce
//! [`EditorCommand`]s, and every one of them goes through [`dispatch`]. Most
//! land in `Editor::apply` (the single document writer); three are *effects*
//! the pure core cannot perform — evaluating Lisp, reading a file, writing a
//! file — and this layer performs them.
//!
//! The invariant worth stating out loud: **`renderers[i]` draws
//! `editor.frames[i]`**. Frames appear from the core (`M-x new-frame`) and the
//! loop opens a window for each one it has not got a window for yet; frames
//! disappear only through [`close_frame`], which drops the matching renderer in
//! the same breath. Everything else — event routing, focus — is an index into
//! both vectors at once, so nothing may remove from one alone.

use std::path::{Path, PathBuf};

use crossbeam_channel::{Receiver, Sender};
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::{Keycode, Mod};
use sdl2::mouse::{Cursor, MouseButton, MouseWheelDirection, SystemCursor};

use zemacs_core::frame::{Divider, Split};
use zemacs_core::{Editor, EditorCommand, Frame, Key, PromptKind, Rect, WindowId};
use zemacs_lisp::Lisp;
use zemacs_render::Renderer;

mod dired;
mod magit;
use dired::Dired;
use magit::Magit;

const RECENT_LIMIT: usize = 10;
const RECENT_ON_DASHBOARD: usize = 5;
/// Lines per wheel notch.
const SCROLL_LINES: i32 = 3;
/// Size of every window, the first and the ones `new-frame` opens.
const WINDOW_W: u32 = 1100;
const WINDOW_H: u32 = 760;

// --- mouse ---------------------------------------------------------------

/// A divider being dragged, and the frame it belongs to.
struct Drag {
    frame: usize,
    divider: Divider,
}

/// The mouse state machine. A struct rather than a couple of locals in the
/// event loop so the transitions can be tested without opening a window.
#[derive(Default)]
struct Mouse {
    drag: Option<Drag>,
}

impl Mouse {
    /// The frame whose divider is being dragged, if any.
    fn dragging(&self) -> Option<usize> {
        self.drag.as_ref().map(|d| d.frame)
    }

    /// Left button down in frame `index`. Grabs a divider if the press landed
    /// on one, otherwise reports the pane that was clicked so the caller can
    /// focus it. Dividers are tested first because their grab area deliberately
    /// overlaps both neighbouring panes.
    fn press(&mut self, frame: &Frame, index: usize, area: Rect, x: i32, y: i32) -> Option<WindowId> {
        match frame.divider_at(area, x, y) {
            Some(divider) => {
                self.drag = Some(Drag {
                    frame: index,
                    divider,
                });
                None
            }
            None => frame.window_at(area, x, y),
        }
    }

    /// Motion only means something while a divider is held; with no button down
    /// there is nothing to move.
    ///
    /// The coordinates are deliberately *not* range-checked. A drag has to
    /// survive the pointer leaving the pane and even the window — SDL captures
    /// the mouse for the duration of a press, so the numbers keep arriving in
    /// the right window's space — and clamping is `drag_divider`'s job.
    fn motion(&self, frames: &mut [Frame], x: i32, y: i32) {
        let Some(drag) = &self.drag else { return };
        if let Some(frame) = frames.get_mut(drag.frame) {
            frame.drag_divider(&drag.divider, x, y);
        }
    }

    fn release(&mut self) {
        self.drag = None;
    }
}

/// Pointer shapes. Swapping in a resize arrow over a divider is the only hint
/// that it can be dragged at all.
struct Cursors {
    arrow: Cursor,
    we: Cursor,
    ns: Cursor,
    shown: Option<Split>,
}

impl Cursors {
    /// `None` when the platform has no system cursors — the dummy video driver
    /// used for headless runs is one. Pointer feedback is a nicety, and losing
    /// it must not be able to stop the editor from starting.
    fn new() -> Option<Self> {
        Some(Self {
            arrow: Cursor::from_system(SystemCursor::Arrow).ok()?,
            we: Cursor::from_system(SystemCursor::SizeWE).ok()?,
            ns: Cursor::from_system(SystemCursor::SizeNS).ok()?,
            shown: None,
        })
    }

    /// `over` is the divider under the pointer, if any. Guarded on the current
    /// shape because this runs on every motion event.
    fn hover(&mut self, over: Option<Split>) {
        if self.shown == over {
            return;
        }
        self.shown = over;
        match over {
            Some(Split::Columns) => self.we.set(),
            Some(Split::Rows) => self.ns.set(),
            None => self.arrow.set(),
        }
    }
}

// --- frames and their windows --------------------------------------------

/// Which frame an SDL event belongs to, given every renderer's window id in
/// frame order. `None` for a window that has already been closed but still has
/// events queued behind it.
fn frame_for_window(mut windows: impl Iterator<Item = u32>, window_id: u32) -> Option<usize> {
    windows.position(|id| id == window_id)
}

/// Close frame `index` and the window showing it.
///
/// Core only ever removes the *focused* frame, so focus is pointed at the
/// doomed one for the call and put back afterwards — closing someone else's
/// window must not steal the keyboard. Removing from `editor.frames` shifts
/// every later frame down, which is what [`focus_after_close`] is for, and the
/// renderer at the same index has to go in the same step or every frame after
/// the hole would be drawn into the wrong window.
///
/// Generic over the renderer purely so the bookkeeping is testable without SDL.
fn close_frame<R>(editor: &mut Editor, renderers: &mut Vec<R>, index: usize) {
    if index >= editor.frames.len() {
        return;
    }
    let focus = editor.focus_frame;
    let before = editor.frames.len();
    editor.focus_frame = index;
    editor.apply(EditorCommand::CloseFrame);
    if editor.frames.len() == before {
        // The last frame: core turned this into a quit and kept the frame, so
        // its window stays up until the loop notices `should_quit`.
        editor.focus_frame = focus;
        return;
    }
    renderers.remove(index);
    editor.focus_frame = focus_after_close(focus, index, before);
}

/// Where focus lands once the frame at `closed` is removed from `before`
/// frames: still on the frame the user was using, shifted down by one if it sat
/// after the hole, and clamped when the focused frame is the one that went.
fn focus_after_close(focus: usize, closed: usize, before: usize) -> usize {
    let last = before.saturating_sub(2); // highest index that survives
    if focus > closed { focus - 1 } else { focus }.min(last)
}

fn main() -> anyhow::Result<()> {
    let sdl = sdl2::init().map_err(|e| anyhow::anyhow!("SDL init: {e}"))?;
    // One renderer per frame, in frame order. See the module docs.
    let mut renderers = vec![Renderer::new(&sdl, "zemacs", WINDOW_W, WINDOW_H)?];
    let mut pump = sdl
        .event_pump()
        .map_err(|e| anyhow::anyhow!("SDL event pump: {e}"))?;

    // Text input is toggled per mode — see `wants_text_input`. It is off to
    // begin with because the editor opens on the dashboard.
    let video = sdl.video().map_err(|e| anyhow::anyhow!("SDL video: {e}"))?;
    let text_input = video.text_input();
    text_input.stop();
    let mut text_input_on = false;

    let init_path = resolve_init_path();
    let (tx, rx): (Sender<EditorCommand>, Receiver<EditorCommand>) =
        crossbeam_channel::unbounded();
    let lisp = zemacs_lisp::spawn(tx, init_path.clone());

    // Thread three: highlighting. The main thread owns input and drawing, the
    // Lisp thread owns the image, and neither ever waits on a parse.
    let highlighter = zemacs_syntax::spawn_worker();

    let mut editor = Editor::new();
    seed_dashboard(&mut editor, &init_path, &renderers[0].backend());

    // Any file named on the command line opens instead of the dashboard.
    if let Some(arg) = std::env::args().nth(1) {
        open_file(&mut editor, &PathBuf::from(arg), &init_path);
    }

    let mut last_revision = u64::MAX;
    let mut last_file_query: Option<String> = None;
    let mut keys: Vec<Key> = Vec::new();
    // macOS composes text for Option combos (⌥- is –, ⌥= is ≠) and SDL2 has no
    // hint to turn that off — SDL_MAC_OPTION_AS_ALT is SDL3-only. So an Alt
    // keydown arms this, and the TextInput that follows is dropped rather than
    // inserted. Cleared by the next non-Alt keydown, so it can never eat a
    // character the user actually typed.
    let mut swallow_text = false;
    let mut mouse = Mouse::default();
    let mut cursors = Cursors::new();
    let mut magit = Magit::default();
    let mut dired = Dired::default();

    'main: loop {
        keys.clear();
        // At most one window closes per iteration: a close shifts every later
        // frame index down, and the rest of this batch was routed against the
        // old ones. Deferring it keeps the whole batch consistent.
        let mut closing: Option<usize> = None;

        for event in pump.poll_iter() {
            match event {
                Event::Quit { .. } => break 'main,
                Event::Window {
                    window_id,
                    win_event,
                    ..
                } => match win_event {
                    // The window manager decides which frame is current and the
                    // editor follows it; there is no other source of truth.
                    //
                    // ponytail: a bare assignment, so the *live* buffer and
                    // cursor follow the pointer into the newly focused frame
                    // instead of that frame's own window being adopted. Core
                    // does the adopting (`Editor::adopt_window`) but only from
                    // inside `apply`, and it has no command for "focus frame
                    // N" — every window command works on `focus_frame`. Fixing
                    // it properly means an `EditorCommand::FocusFrame(usize)`
                    // in core, next to `NewFrame`; until then two frames
                    // showing different buffers swap contents when you click
                    // between them.
                    WindowEvent::FocusGained => {
                        if let Some(i) =
                            frame_for_window(renderers.iter().map(Renderer::window_id), window_id)
                        {
                            // Through the command, not a bare assignment: the
                            // live buffer belongs to the focused window, so
                            // core has to park it and adopt the new frame's.
                            let cmd = EditorCommand::FocusFrame(i);
                            dispatch(&mut editor, &lisp, cmd, &init_path, &mut renderers, &mut magit, &mut dired);
                        }
                    }
                    WindowEvent::Close => {
                        if let Some(i) =
                            frame_for_window(renderers.iter().map(Renderer::window_id), window_id)
                        {
                            closing.get_or_insert(i);
                        }
                    }
                    _ => {}
                },
                // Keys are not routed by window: the editor is global and the
                // focused frame is where they land.
                Event::KeyDown {
                    keycode: Some(kc),
                    keymod,
                    ..
                } => {
                    swallow_text = keymod.intersects(Mod::LALTMOD | Mod::RALTMOD);
                    keys.extend(key_from_keydown(kc, keymod, !text_input_on));
                }
                Event::MouseButtonDown {
                    window_id,
                    mouse_btn: MouseButton::Left,
                    x,
                    y,
                    ..
                } => {
                    if let Some(i) =
                        frame_for_window(renderers.iter().map(Renderer::window_id), window_id)
                    {
                        let (x, y) = renderers[i].to_pixels(x, y);
                        let area = renderers[i].content_area();
                        // A click anywhere in a window focuses its frame. The
                        // `FocusGained` above would do it a moment later anyway;
                        // doing it here means the `FocusWindow` below addresses
                        // the frame that was actually clicked.
                        dispatch(
                            &mut editor,
                            &lisp,
                            EditorCommand::FocusFrame(i),
                            &init_path,
                            &mut renderers,
                            &mut magit,
                            &mut dired,
                        );
                        if let Some(window) = mouse.press(&editor.frames[i], i, area, x, y) {
                            let cmd = EditorCommand::FocusWindow(window);
                            dispatch(&mut editor, &lisp, cmd, &init_path, &mut renderers, &mut magit, &mut dired);
                        }
                    }
                }
                Event::MouseButtonUp {
                    mouse_btn: MouseButton::Left,
                    ..
                } => mouse.release(),
                Event::MouseMotion { window_id, x, y, .. } => match mouse.dragging() {
                    // A held divider keeps its own frame rather than the event's:
                    // SDL captures the mouse for the press, so the coordinates
                    // stay in that window's space even once the pointer has left
                    // it, and letting go of the drag at the edge is exactly the
                    // bug this avoids.
                    Some(i) => {
                        if let Some(renderer) = renderers.get(i) {
                            let (x, y) = renderer.to_pixels(x, y);
                            mouse.motion(&mut editor.frames, x, y);
                        }
                    }
                    None => {
                        if let Some(i) =
                            frame_for_window(renderers.iter().map(Renderer::window_id), window_id)
                        {
                            let (x, y) = renderers[i].to_pixels(x, y);
                            let area = renderers[i].content_area();
                            if let Some(cursors) = &mut cursors {
                                cursors
                                    .hover(editor.frames[i].divider_at(area, x, y).map(|d| d.dir));
                            }
                        }
                    }
                },
                // SDL reports natural-scroll wheels with `direction: Flipped`
                // and the raw sign, so undo that first; then negate, because a
                // wheel push (positive y) moves the view *up* the file.
                Event::MouseWheel {
                    window_id,
                    y,
                    direction,
                    mouse_x,
                    mouse_y,
                    ..
                } => {
                    let y = match direction {
                        MouseWheelDirection::Flipped => -y,
                        _ => y,
                    };
                    let frame =
                        frame_for_window(renderers.iter().map(Renderer::window_id), window_id);
                    if let (true, Some(i)) = (y != 0, frame) {
                        // Scroll the pane under the pointer — by focusing it
                        // first. `ScrollLines` moves the *live* window, and
                        // focusing is the only way to make a pane live without
                        // duplicating core's scroll clamping out here. It also
                        // matches the click: pointing at a pane and acting on it
                        // is the same gesture either way.
                        let (px, py) = renderers[i].to_pixels(mouse_x, mouse_y);
                        let area = renderers[i].content_area();
                        editor.focus_frame = i;
                        if let Some(window) = editor.frames[i].window_at(area, px, py) {
                            let cmd = EditorCommand::FocusWindow(window);
                            dispatch(&mut editor, &lisp, cmd, &init_path, &mut renderers, &mut magit, &mut dired);
                        }
                        let cmd = EditorCommand::ScrollLines(-y * SCROLL_LINES);
                        dispatch(&mut editor, &lisp, cmd, &init_path, &mut renderers, &mut magit, &mut dired);
                    }
                }
                Event::TextInput { text, .. } => {
                    if swallow_text {
                        swallow_text = false;
                    } else {
                        keys.extend(text.chars().map(Key::Char));
                    }
                }
                _ => {}
            }
        }

        if let Some(i) = closing {
            mouse.release(); // whatever was being dragged may be going away
            close_frame(&mut editor, &mut renderers, i);
        }

        for key in keys.drain(..) {
            for cmd in editor.handle_key(key) {
                dispatch(&mut editor, &lisp, cmd, &init_path, &mut renderers, &mut magit, &mut dired);
            }
        }
        while let Ok(cmd) = rx.try_recv() {
            dispatch(&mut editor, &lisp, cmd, &init_path, &mut renderers, &mut magit, &mut dired);
        }

        // Toggle SDL text input to match the mode. This is what stops macOS
        // press-and-hold: the accent panel is a text-input-client feature, so
        // with text input off, holding `j` repeats the keystroke natively
        // instead of offering ĵ. Insert mode and prompts keep it on, where
        // layout-correct characters and dead keys are what you actually want.
        let want_text = wants_text_input(&editor);
        if want_text != text_input_on {
            if want_text {
                text_input.start();
            } else {
                text_input.stop();
                swallow_text = false;
            }
            text_input_on = want_text;
        }

        // The last window going away is a quit even if core has not said so.
        if editor.should_quit || renderers.is_empty() {
            break 'main;
        }

        // Hand each new revision to the syntax thread and carry on drawing.
        // ponytail: still a full reparse per revision, just off the UI thread,
        // and the rope is flattened to a String to hand over. Both go away
        // together by keeping the `Tree` per buffer and feeding tree-sitter the
        // changed ranges — worth doing when files get big, not before.
        if editor.revision != last_revision {
            last_revision = editor.revision;
            match &editor.buffer.language {
                Some(lang) => {
                    highlighter.request(editor.revision, lang, editor.buffer.text.to_string())
                }
                None => editor.highlights.clear(),
            }
        }
        // Mode hooks: core records that one is due, the image runs it. Guarded
        // with `fboundp` so a mode with no hook defined is silence rather than
        // an "undefined function" every time you open a file.
        for hook in std::mem::take(&mut editor.pending_hooks) {
            lisp.eval(format!(
                "(let ((h (find-symbol {:?} :zemacs))) (when (and h (fboundp h)) (funcall h)))",
                hook.to_uppercase()
            ));
        }

        refresh_file_completions(&mut editor, &mut last_file_query);

        // Adopt a result only if the buffer hasn't moved on; if it has, a newer
        // parse is already in flight and the current spans stay up meanwhile.
        if let Some((revision, spans)) = highlighter.poll() {
            if revision == editor.revision {
                editor.highlights = spans;
            }
        }

        // Where `M-x new-frame` becomes a window. Core pushes the frame, the
        // loop notices it has no renderer for it. Emacs spells extra frames
        // `<2>`, `<3>`; so do we, so they are tellable apart in the dock.
        while renderers.len() < editor.frames.len() {
            let title = format!("zemacs <{}>", renderers.len() + 1);
            renderers.push(Renderer::new(&sdl, &title, WINDOW_W, WINDOW_H)?);
        }

        // Park the live cursor and scroll on the focused window, once, so every
        // pane in every frame can be drawn from its own `Window`.
        editor.sync_focused_window();

        // `render` syncs the font itself, and each canvas presents on vsync —
        // that is what paces this loop, so there is no sleep here. With N
        // windows it is also N waits per iteration, so the loop runs at 1/N of
        // the refresh rate. Left alone deliberately: fixing it means presenting
        // off-thread or giving up vsync, and neither is worth it for two windows.
        for (i, renderer) in renderers.iter_mut().enumerate() {
            renderer.render(&mut editor, i)?;
        }
    }
    Ok(())
}

/// The one place a command becomes an action. Everything the pure core can do
/// goes to `apply`; the three effects it can't perform are handled here.
///
/// `CloseFrame` is a fourth: core removes the frame, but only this layer can
/// take the window down with it, and `M-x delete-frame` reaches core through
/// here just like the window's close button does.
fn dispatch(
    editor: &mut Editor,
    lisp: &Lisp,
    cmd: EditorCommand,
    init_path: &Path,
    renderers: &mut Vec<Renderer>,
    magit: &mut Magit,
    dired: &mut Dired,
) {
    match cmd {
        EditorCommand::CallLisp(form) => lisp.eval(form),
        // dired borrows the file prompt for rename/copy/mkdir, so an answer to
        // one of those is a filename for *it* rather than a file to open.
        EditorCommand::OpenFile(path) if dired.awaiting_input() => {
            dired.supply(editor, &path.to_string_lossy())
        }
        // Opening a directory lists it, as `find-file` does in Emacs.
        EditorCommand::OpenFile(path) if path.is_dir() => {
            editor.buffer.path = Some(path);
            dired.run(editor, "open");
        }
        EditorCommand::OpenFile(path) => open_file(editor, &path, init_path),
        EditorCommand::SaveFile(path) => save_file(editor, path),
        EditorCommand::Git(verb) => magit.run(editor, &verb),
        EditorCommand::Dired(verb) => {
            dired.run(editor, &verb);
            // `RET` on a file leaves dired; opening a buffer is this layer's
            // job, so dired asks rather than doing it.
            if let Some(path) = dired.open_file.take() {
                open_file(editor, &path, init_path);
            }
        }
        EditorCommand::CloseFrame => {
            let focused = editor.focus_frame;
            close_frame(editor, renderers, focused);
        }
        other => editor.apply(other),
    }
}

// --- file effects --------------------------------------------------------

/// `@init` is the sentinel the dashboard's "Edit configuration" item uses —
/// the core has no idea where the config lives, this layer does.
fn open_file(editor: &mut Editor, path: &Path, init_path: &Path) {
    let path = if path == Path::new("@init") {
        init_path.to_path_buf()
    } else {
        path.to_path_buf()
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let lang = zemacs_syntax::language_for_path(&path);
            let shown = display_path(&path);
            editor.load(&text, Some(path.clone()), lang);
            editor.apply(EditorCommand::Message(format!("opened {shown}")));
            remember_recent(&path);
        }
        Err(e) => editor.apply(EditorCommand::Message(format!(
            "{}: {e}",
            display_path(&path)
        ))),
    }
}

fn save_file(editor: &mut Editor, path: Option<PathBuf>) {
    // A dired buffer's `path` is the *directory* it lists, and a magit buffer's
    // text is a rendered status. Writing either would overwrite something real
    // with a screenshot of a view.
    if editor.buffer.kind.is_generated() {
        let name = editor.buffer.name();
        editor.apply(EditorCommand::Message(format!("{name} is not a file")));
        return;
    }
    let Some(target) = path.or_else(|| editor.buffer.path.clone()) else {
        editor.apply(EditorCommand::Message("no file name — use :w <path>".into()));
        return;
    };
    let text = editor.buffer.text.to_string();
    match std::fs::write(&target, &text) {
        Ok(()) => {
            editor.buffer.modified = false;
            if editor.buffer.path.is_none() {
                editor.buffer.language = zemacs_syntax::language_for_path(&target);
                editor.buffer.path = Some(target.clone());
                // The text did not change, but the *language* did, and the
                // highlight request is gated on the revision — without this,
                // `:w foo.rs` on a scratch buffer stays uncolored until the
                // next edit.
                editor.revision += 1;
            }
            remember_recent(&target);
            editor.apply(EditorCommand::Message(format!(
                "wrote {} ({} bytes)",
                display_path(&target),
                text.len()
            )));
        }
        Err(e) => editor.apply(EditorCommand::Message(format!(
            "{}: {e}",
            display_path(&target)
        ))),
    }
}

// --- file completion -----------------------------------------------------

/// Feed directory listings to an open find-file prompt.
///
/// This lives here rather than in the prompt itself because core does no IO.
/// The listing is refreshed only when the *directory* part of the typed path
/// changes — the filename part is what the fuzzy matcher filters on, so
/// re-reading the directory per keystroke would be pure waste.
fn refresh_file_completions(editor: &mut Editor, last_query: &mut Option<String>) {
    let Some(prompt) = editor.prompt.as_mut() else {
        *last_query = None;
        return;
    };
    if prompt.kind != PromptKind::File {
        *last_query = None;
        return;
    }
    let typed = expand_tilde(&prompt.text);
    let (dir, _) = match typed.rsplit_once('/') {
        Some((d, name)) => (if d.is_empty() { "/" } else { d }.to_string(), name),
        None => (".".to_string(), typed.as_str()),
    };
    if last_query.as_deref() == Some(dir.as_str()) {
        return;
    }
    *last_query = Some(dir.clone());

    let mut entries: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let name = e.file_name().to_string_lossy().into_owned();
            // Keep the prefix the user typed, so the completion is a path they
            // can hit Enter on, and mark directories so they read as such.
            let path = if dir == "/" {
                format!("/{name}")
            } else if dir == "." && !typed.starts_with("./") {
                name
            } else {
                format!("{dir}/{name}")
            };
            if is_dir {
                format!("{path}/")
            } else {
                path
            }
        })
        .filter(|p| !p.rsplit('/').next().unwrap_or_default().starts_with('.'))
        .collect();
    entries.sort();
    prompt.set_items(entries);
}

/// `~/x` -> `/Users/you/x`. Mirrors the core-side expansion so what the user
/// types, what gets completed, and what gets opened all agree.
fn expand_tilde(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => format!("{}/{rest}", home.to_string_lossy()),
            None => p.to_string(),
        },
        None => p.to_string(),
    }
}

// --- dashboard -----------------------------------------------------------

/// Put recently-opened files on the dashboard, then a footer. Runs before
/// `init.lisp` loads; recents live in their own list, so a config that calls
/// `clear-dashboard-items` still keeps them.
fn seed_dashboard(editor: &mut Editor, init_path: &Path, backend: &str) {
    editor.dashboard.recents = read_recent()
        .into_iter()
        .take(RECENT_ON_DASHBOARD)
        .enumerate()
        .map(|(i, p)| zemacs_core::dashboard::Item {
            key: char::from_digit(i as u32 + 1, 10).unwrap_or('?'),
            label: display_path(&p),
            action: format!("open:{}", p.display()),
        })
        .collect();
    editor.dashboard.footer = format!(
        "config: {}\nrenderer: {backend} · j/k or number to pick · RET to open · q to quit",
        display_path(init_path)
    );
}

// --- recent files --------------------------------------------------------

fn recent_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/share/zemacs/recent"))
}

fn read_recent() -> Vec<PathBuf> {
    let Some(file) = recent_path() else {
        return Vec::new();
    };
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect()
}

/// Most-recent-first, deduplicated, capped. Failures are silent: a missing
/// recents file must never get between the user and their editor.
fn remember_recent(path: &Path) {
    let Some(file) = recent_path() else { return };
    let Ok(canonical) = path.canonicalize() else {
        return;
    };
    let mut list = vec![canonical.clone()];
    list.extend(read_recent().into_iter().filter(|p| *p != canonical));
    list.truncate(RECENT_LIMIT);

    let body: String = list
        .iter()
        .map(|p| format!("{}\n", p.display()))
        .collect();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, body);
}

/// `~`-shortened, for anything the user reads.
fn display_path(path: &Path) -> String {
    let full = path.display().to_string();
    match std::env::var_os("HOME") {
        Some(home) => {
            let home = home.to_string_lossy();
            match full.strip_prefix(home.as_ref()) {
                Some(rest) => format!("~{rest}"),
                None => full,
            }
        }
        None => full,
    }
}

// --- input translation ---------------------------------------------------

/// Where a character comes from depends on the mode.
///
/// With text input on (Insert, prompts) the keyboard layout and any dead keys
/// or IME get their say, and printable characters arrive as `TextInput` — so
/// this only handles the keys that produce no text.
///
/// With it off (Normal, Visual, Dashboard) there are no `TextInput` events at
/// all, so `raw` makes this synthesise the character itself from the keycode.
/// That is the modal half of the editor, where keys are commands rather than
/// text, and where holding one has to repeat.
fn key_from_keydown(kc: Keycode, keymod: Mod, raw: bool) -> Option<Key> {
    let shift = keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
    let ctrl = keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD);
    // Command is Meta — the macOS Emacs convention, and unlike Option it does
    // not compose text, so `⌘-` needs no special handling. Option stays a Meta
    // fallback for anyone used to it (and for other OSes).
    let meta = keymod.intersects(Mod::LGUIMOD | Mod::RGUIMOD | Mod::LALTMOD | Mod::RALTMOD);
    // Enter has a multi-character key name, so `combo_char` cannot spell it —
    // but `C-<ret>` and `C-M-<ret>` are the window splits.
    if matches!(kc, Keycode::Return | Keycode::KpEnter) {
        match (ctrl, meta) {
            (true, true) => return Some(Key::CtrlMetaEnter),
            (true, false) => return Some(Key::CtrlEnter),
            _ => {}
        }
    }
    match (ctrl, meta) {
        // Shift is not consulted for Ctrl combos: `C-a` and `C-A` are one key,
        // as in Emacs. Meta keeps it so `M-+` can be spelled the way it's typed.
        (true, true) => return combo_char(kc, false).map(Key::CtrlMeta),
        (true, false) => return combo_char(kc, false).map(Key::Ctrl),
        (false, true) => return combo_char(kc, shift).map(Key::Meta),
        (false, false) => {}
    }
    match kc {
        Keycode::Escape => Some(Key::Esc),
        Keycode::Return | Keycode::KpEnter => Some(Key::Enter),
        Keycode::Backspace => Some(Key::Backspace),
        Keycode::Tab => Some(Key::Tab),
        Keycode::Left => Some(Key::Left),
        Keycode::Right => Some(Key::Right),
        Keycode::Up => Some(Key::Up),
        Keycode::Down => Some(Key::Down),
        // Space has a multi-character key name, so `combo_char` cannot produce
        // it — but it is the leader key, so it has to work.
        Keycode::Space if raw => Some(Key::Char(' ')),
        _ if raw => combo_char(kc, shift).map(Key::Char),
        _ => None,
    }
}

/// Text input is for typing text. Everywhere else keys are commands, and
/// turning it off is what gives us native key repeat and no accent panel.
fn wants_text_input(editor: &Editor) -> bool {
    editor.mode == zemacs_core::Mode::Insert || editor.prompt.is_some()
}

/// The character a `C-`/`M-` combo names. `None` when the key's name is a word
/// (`Left`, `F1`, `Space`) — those have no `C-x` style spelling here.
///
/// SDL keycodes are layout-aware but always *unshifted*, so `⌥⇧=` arrives as
/// `=` plus a shift bit; [`shifted`] is what lets a binding be written `M-+`,
/// the way it is typed.
fn combo_char(kc: Keycode, shift: bool) -> Option<char> {
    let name = kc.name();
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(if shift {
            shifted(c)
        } else {
            c.to_ascii_lowercase()
        }),
        _ => None,
    }
}

/// US-layout shifted symbols. Only used to *spell* a binding: the unshifted
/// forms (`M-=`, `M--`) work on every layout, so a different layout costs you
/// the punctuation spellings, not the feature.
fn shifted(c: char) -> char {
    match c {
        '=' => '+',
        '-' => '_',
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        '`' => '~',
        other => other.to_ascii_uppercase(),
    }
}

/// `$ZEMACS_INIT`, else `~/.config/zemacs/init.lisp` if it exists, else the copy
/// shipped in the repo — so `cargo run` works out of the box.
fn resolve_init_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("ZEMACS_INIT") {
        return PathBuf::from(explicit);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let user = PathBuf::from(home).join(".config/zemacs/init.lisp");
        if user.exists() {
            return user;
        }
    }
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../runtime/init.lisp"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_keys_translate() {
        assert_eq!(
            key_from_keydown(Keycode::R, Mod::LCTRLMOD, false),
            Some(Key::Ctrl('r'))
        );
        assert_eq!(key_from_keydown(Keycode::Escape, Mod::NOMOD, false), Some(Key::Esc));
        // printable keys are left to TextInput
        assert_eq!(key_from_keydown(Keycode::A, Mod::NOMOD, false), None);
    }

    /// The press-and-hold fix: in modal modes there are no `TextInput` events,
    /// so the keycode itself has to yield the character — that is what lets a
    /// held `j` repeat instead of opening the macOS accent panel.
    #[test]
    fn modal_modes_read_characters_from_the_keycode() {
        for (kc, want) in [
            (Keycode::J, 'j'),
            (Keycode::K, 'k'),
            (Keycode::D, 'd'),
            (Keycode::Num4, '4'),
        ] {
            assert_eq!(
                key_from_keydown(kc, Mod::NOMOD, true),
                Some(Key::Char(want)),
                "{kc:?} should produce {want} with text input off"
            );
        }
        // shifted punctuation and letters, so `$`, `:` and `ZZ` still work
        assert_eq!(
            key_from_keydown(Keycode::Num4, Mod::LSHIFTMOD, true),
            Some(Key::Char('$'))
        );
        assert_eq!(
            key_from_keydown(Keycode::Semicolon, Mod::LSHIFTMOD, true),
            Some(Key::Char(':'))
        );
        assert_eq!(
            key_from_keydown(Keycode::Z, Mod::LSHIFTMOD, true),
            Some(Key::Char('Z'))
        );
        // the leader key, whose key name is a word rather than a character
        assert_eq!(
            key_from_keydown(Keycode::Space, Mod::NOMOD, true),
            Some(Key::Char(' '))
        );
        // named keys keep working either way
        assert_eq!(key_from_keydown(Keycode::Escape, Mod::NOMOD, true), Some(Key::Esc));
    }

    #[test]
    fn text_input_is_on_only_where_text_is_typed() {
        let mut ed = Editor::new();
        assert!(!wants_text_input(&ed)); // dashboard
        ed.mode = zemacs_core::Mode::Normal;
        assert!(!wants_text_input(&ed));
        ed.mode = zemacs_core::Mode::Visual;
        assert!(!wants_text_input(&ed));
        ed.mode = zemacs_core::Mode::Insert;
        assert!(wants_text_input(&ed));
        // and prompts type text whatever the mode
        ed.mode = zemacs_core::Mode::Normal;
        ed.open_prompt(PromptKind::Command);
        assert!(wants_text_input(&ed));
    }

    #[test]
    fn command_is_meta_and_beats_option() {
        // ⌘= and ⌘⇧= (which is how you type ⌘+)
        assert_eq!(
            key_from_keydown(Keycode::Equals, Mod::LGUIMOD, false),
            Some(Key::Meta('='))
        );
        assert_eq!(
            key_from_keydown(Keycode::Equals, Mod::LGUIMOD | Mod::LSHIFTMOD, false),
            Some(Key::Meta('+'))
        );
        assert_eq!(
            key_from_keydown(Keycode::Minus, Mod::LGUIMOD, false),
            Some(Key::Meta('-'))
        );
        // Option still works as a fallback...
        assert_eq!(
            key_from_keydown(Keycode::Minus, Mod::LALTMOD, false),
            Some(Key::Meta('-'))
        );
        // ...and Ctrl+Command is its own key, `C-M-`, not either one alone.
        assert_eq!(
            key_from_keydown(Keycode::J, Mod::LGUIMOD | Mod::LCTRLMOD, false),
            Some(Key::CtrlMeta('j'))
        );
        assert_eq!(Key::CtrlMeta('j').token(), "C-M-j");
        // the window splits
        assert_eq!(
            key_from_keydown(Keycode::Return, Mod::LCTRLMOD, false),
            Some(Key::CtrlEnter)
        );
        assert_eq!(
            key_from_keydown(Keycode::Return, Mod::LCTRLMOD | Mod::LGUIMOD, false),
            Some(Key::CtrlMetaEnter)
        );
        // plain Enter is unaffected
        assert_eq!(
            key_from_keydown(Keycode::Return, Mod::NOMOD, false),
            Some(Key::Enter)
        );
        // Ctrl+Option spells the same key, so either Meta source works.
        assert_eq!(
            key_from_keydown(Keycode::J, Mod::LALTMOD | Mod::RCTRLMOD, false),
            Some(Key::CtrlMeta('j'))
        );
    }

    #[test]
    fn init_sentinel_resolves_to_the_config() {
        let mut ed = Editor::new();
        let init = resolve_init_path();
        open_file(&mut ed, Path::new("@init"), &init);
        // Either it loaded the config or it reported why; it must never be the
        // literal path "@init".
        assert_ne!(ed.buffer.path.as_deref(), Some(Path::new("@init")));
    }

    #[test]
    fn file_prompt_completes_from_the_filesystem() {
        let dir = std::env::temp_dir().join("zemacs_completion_test");
        let _ = std::fs::create_dir_all(dir.join("sub"));
        std::fs::write(dir.join("alpha.rs"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();

        let mut ed = Editor::new();
        ed.open_prompt(PromptKind::File);
        ed.prompt.as_mut().unwrap().text = format!("{}/", dir.display());
        let mut last = None;
        refresh_file_completions(&mut ed, &mut last);

        let items = &ed.prompt.as_ref().unwrap().items;
        assert!(items.iter().any(|i| i.ends_with("alpha.rs")));
        // directories are marked, dotfiles are not offered
        assert!(items.iter().any(|i| i.ends_with("sub/")));
        assert!(!items.iter().any(|i| i.contains(".hidden")));

        // typing a filename filters without re-reading the directory
        ed.prompt.as_mut().unwrap().text = format!("{}/alph", dir.display());
        ed.prompt.as_mut().unwrap().refilter();
        assert_eq!(
            ed.prompt.as_ref().unwrap().current().map(|s| s.ends_with("alpha.rs")),
            Some(true)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn completions_are_dropped_when_the_prompt_closes() {
        let mut ed = Editor::new();
        let mut last = Some("/somewhere".to_string());
        refresh_file_completions(&mut ed, &mut last);
        assert_eq!(last, None);
    }

    #[test]
    fn dashboard_seeds_a_footer() {
        let mut ed = Editor::new();
        seed_dashboard(&mut ed, Path::new("/tmp/init.lisp"), "metal");
        assert!(ed.dashboard.footer.contains("init.lisp"));
        assert!(ed.dashboard.footer.contains("metal"));
        assert_eq!(ed.mode, zemacs_core::Mode::Dashboard);
    }

    // --- frames and their windows ----------------------------------------

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        w: 1000,
        h: 600,
    };

    /// Stand-in for the renderer vector: the tests care about the indices, and
    /// a `Renderer` needs a window.
    fn windows(n: usize) -> Vec<u32> {
        (0..n as u32).collect()
    }

    #[test]
    fn events_route_to_the_frame_that_owns_the_window() {
        // SDL window ids are opaque and not in any order, hence the lookup.
        let ids = [7u32, 3, 11];
        assert_eq!(frame_for_window(ids.into_iter(), 7), Some(0));
        assert_eq!(frame_for_window(ids.into_iter(), 3), Some(1));
        assert_eq!(frame_for_window(ids.into_iter(), 11), Some(2));
        // a window that has already gone: its queued events are dropped
        assert_eq!(frame_for_window(ids.into_iter(), 99), None);
        // the common case
        assert_eq!(frame_for_window([7u32].into_iter(), 7), Some(0));
    }

    #[test]
    fn focus_shifts_down_only_when_it_sat_after_the_hole() {
        assert_eq!(focus_after_close(2, 0, 3), 1); // the user's frame moved down
        assert_eq!(focus_after_close(0, 2, 3), 0); // ...and here it did not
        assert_eq!(focus_after_close(1, 1, 3), 1); // closed the focused one
        assert_eq!(focus_after_close(2, 2, 3), 1); // ...at the end: step back
        assert_eq!(focus_after_close(1, 0, 2), 0);
        assert_eq!(focus_after_close(0, 1, 2), 0);
    }

    #[test]
    fn closing_a_background_frame_keeps_the_vectors_in_step() {
        let mut ed = Editor::new();
        ed.apply(EditorCommand::NewFrame);
        ed.apply(EditorCommand::NewFrame);
        assert_eq!(ed.frames.len(), 3);

        let mut renderers = windows(3);
        ed.focus_frame = 2;
        close_frame(&mut ed, &mut renderers, 0);

        assert_eq!(ed.frames.len(), renderers.len(), "one window per frame");
        // the *right* windows survived, not just the right number of them
        assert_eq!(renderers, vec![1, 2]);
        // and the frame the user was on — index 2 — is now index 1
        assert_eq!(ed.focus_frame, 1);
        assert!(!ed.should_quit);
    }

    #[test]
    fn closing_the_focused_frame_lands_on_a_neighbour() {
        let mut ed = Editor::new();
        ed.apply(EditorCommand::NewFrame);
        let mut renderers = windows(2);
        ed.focus_frame = 1;
        close_frame(&mut ed, &mut renderers, 1);
        assert_eq!(ed.frames.len(), 1);
        assert_eq!(renderers, vec![0]);
        assert_eq!(ed.focus_frame, 0);
    }

    #[test]
    fn closing_the_last_frame_quits_instead() {
        let mut ed = Editor::new();
        let mut renderers = windows(1);
        close_frame(&mut ed, &mut renderers, 0);
        assert!(ed.should_quit);
        // core keeps the frame, so the window stays up until the loop notices
        assert_eq!(ed.frames.len(), 1);
        assert_eq!(renderers.len(), 1);
    }

    // --- mouse ------------------------------------------------------------

    #[test]
    fn a_press_grabs_a_divider_or_focuses_a_pane() {
        let mut f = Frame::new(0);
        let left = f.current;
        let right = f.split(Split::Columns);
        let divider = f.dividers(AREA)[0].clone();
        let mut mouse = Mouse::default();

        // inside a pane: focus it, and start nothing
        assert_eq!(mouse.press(&f, 0, AREA, 10, 10), Some(left));
        assert_eq!(mouse.dragging(), None);
        assert_eq!(mouse.press(&f, 0, AREA, AREA.w - 10, 10), Some(right));
        assert_eq!(mouse.dragging(), None);

        // on the divider: the opposite — a drag, and no focus change
        assert_eq!(mouse.press(&f, 0, AREA, divider.rect.x, 300), None);
        assert_eq!(mouse.dragging(), Some(0));

        // and the drag remembers which frame it belongs to
        let mut mouse = Mouse::default();
        assert_eq!(mouse.press(&f, 2, AREA, divider.rect.x, 300), None);
        assert_eq!(mouse.dragging(), Some(2));
    }

    #[test]
    fn a_divider_moves_only_while_it_is_held() {
        let mut frames = vec![Frame::new(0)];
        frames[0].split(Split::Columns);
        let width = |frames: &[Frame]| frames[0].panes(AREA)[0].rect.w;
        let before = width(&frames);

        // no button down: motion is inert
        let mut mouse = Mouse::default();
        mouse.motion(&mut frames, 250, 300);
        assert_eq!(width(&frames), before);

        let divider = frames[0].dividers(AREA)[0].clone();
        assert_eq!(mouse.press(&frames[0], 0, AREA, divider.rect.x, 300), None);
        mouse.motion(&mut frames, 250, 300);
        assert!((width(&frames) - 250).abs() <= zemacs_core::frame::DIVIDER);

        // overshooting the pane, and the window, keeps the drag alive — core
        // clamps, so both panes are still on screen
        mouse.motion(&mut frames, -400, 900);
        assert!(frames[0].panes(AREA)[0].rect.w > 0);
        assert!(frames[0].panes(AREA)[1].rect.w > 0);

        // ...and the button coming up ends it
        mouse.release();
        assert_eq!(mouse.dragging(), None);
        let held = width(&frames);
        mouse.motion(&mut frames, 700, 300);
        assert_eq!(width(&frames), held);
    }

    /// The common case: one frame, one pane, no dividers to get in the way.
    #[test]
    fn a_single_pane_frame_has_nothing_to_drag() {
        let f = Frame::new(0);
        let mut mouse = Mouse::default();
        assert!(f.dividers(AREA).is_empty());
        assert_eq!(mouse.press(&f, 0, AREA, 500, 300), Some(f.current));
        assert_eq!(mouse.dragging(), None);
    }
}
