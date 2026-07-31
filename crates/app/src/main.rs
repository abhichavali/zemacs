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
use std::time::{Duration, Instant};

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
mod project;
mod term;
use dired::Dired;
use magit::Magit;
use project::Project;
use term::Term;

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

// --- instrumentation -------------------------------------------------------

/// Where the frame went. Off unless `ZEMACS_PERF` is set, in which case it is
/// the reporting interval in seconds — `ZEMACS_PERF=1 zemacs` puts a line on
/// stderr once a second and nothing else changes.
///
/// This lives in the loop rather than in a benchmark because the number that
/// matters is not reproducible off-screen: `present` blocks until the display's
/// next vertical blank, so what a frame costs depends on the monitor, the
/// compositor and how many windows are open. It stays in the tree for the same
/// reason — the next time the editor feels slow, this is the difference between
/// a measurement and an argument.
///
/// The `Instant::now` calls are unconditional. Six of them a frame is on the
/// order of a hundred nanoseconds against a sixteen *millisecond* budget, and
/// paying it always is worth not having two versions of the loop.
struct Perf {
    every: Option<Duration>,
    since: Instant,
    frames: u32,
    /// Parked waiting for the user. On an idle editor this should be nearly the
    /// whole interval; if it is not, something is redrawing for no reason.
    idle: Duration,
    /// Events, keys, commands, hooks and the per-frame housekeeping — and the
    /// wait for the editor lock, which is where a slow Lisp primitive shows up.
    input: Duration,
    /// `render` for every window — the glyphs, on the CPU side.
    draw: Duration,
    /// `present` for every window that needed one. Vertical blanks, mostly.
    present: Duration,
    worst: Duration,
    presents: u32,
    draws: u64,
    /// The number the user actually feels: from SDL stamping a keystroke to the
    /// frame answering it reaching the screen. Everything above is where the
    /// time went; this is whether it mattered.
    latency: Duration,
    worst_latency: Duration,
    keys: u32,
}

impl Perf {
    fn new() -> Self {
        // Anything unparseable means "on, once a second", so `ZEMACS_PERF=1` and
        // `ZEMACS_PERF=yes` both do the obvious thing.
        let every = std::env::var("ZEMACS_PERF").ok().map(|v| {
            Duration::from_secs_f64(v.parse::<f64>().ok().filter(|s| *s > 0.0).unwrap_or(1.0))
        });
        Self {
            every,
            since: Instant::now(),
            frames: 0,
            idle: Duration::ZERO,
            input: Duration::ZERO,
            draw: Duration::ZERO,
            present: Duration::ZERO,
            worst: Duration::ZERO,
            presents: 0,
            draws: 0,
            latency: Duration::ZERO,
            worst_latency: Duration::ZERO,
            keys: 0,
        }
    }

    /// A keystroke that reached the screen this frame. `ms` is SDL's own clock
    /// from the event's timestamp to the present returning, so it counts the
    /// time the key spent queued while the loop was busy elsewhere — which is
    /// the half that a faster redraw cannot fix.
    fn key(&mut self, ms: u32) {
        if self.every.is_none() {
            return;
        }
        self.keys += 1;
        self.latency += Duration::from_millis(u64::from(ms));
        self.worst_latency = self.worst_latency.max(Duration::from_millis(u64::from(ms)));
    }

    /// One iteration is over. Reports and resets when the interval is up.
    fn frame(&mut self, started: Instant, presents: u32, draws: u32) {
        let Some(every) = self.every else { return };
        self.frames += 1;
        self.presents += presents;
        self.draws += u64::from(draws);
        self.worst = self.worst.max(started.elapsed());
        let elapsed = self.since.elapsed();
        if elapsed < every {
            return;
        }
        let n = f64::from(self.frames.max(1));
        let ms = |d: Duration| d.as_secs_f64() * 1000.0 / n;
        eprintln!(
            "perf: {:.0} fps ({} frames in {:.1}s) | per frame: wait {:.2} input {:.2} draw {:.2} present {:.2} ms, worst {:.1} ms | {:.1} presents, {:.0} draw calls",
            n / elapsed.as_secs_f64(),
            self.frames,
            elapsed.as_secs_f64(),
            ms(self.idle),
            ms(self.input),
            ms(self.draw),
            ms(self.present),
            self.worst.as_secs_f64() * 1000.0,
            f64::from(self.presents) / n,
            self.draws as f64 / n,
        );
        // Only when somebody typed — an idle interval has nothing to say here,
        // and a zero would read as "instant" rather than "not measured".
        if self.keys > 0 {
            eprintln!(
                "perf: {} keys, key to screen {:.1} ms average, {:.0} ms worst",
                self.keys,
                self.latency.as_secs_f64() * 1000.0 / f64::from(self.keys),
                self.worst_latency.as_secs_f64() * 1000.0,
            );
        }
        *self = Self {
            every: self.every,
            since: Instant::now(),
            ..Self::new()
        };
    }
}

/// Hints that have to be set *before* `sdl2::init`, because SDL reads them
/// while it is registering the application with Cocoa.
///
/// The green button is the one that needs saying out loud: without
/// `FULLSCREEN_SPACES`, SDL marks its windows `FullScreenAuxiliary`, macOS
/// disables zoom, and the button sits there doing nothing. With it, green is
/// native fullscreen, which is what it means everywhere else on the system.
///
/// `MAC_BACKGROUND_APP=0` asks for a *regular* application — one with a dock
/// icon that can be activated. A process launched straight from a terminal can
/// otherwise end up as an accessory, and an accessory's windows do not take
/// keyboard focus properly, which is the same root cause as a new frame not
/// receiving typing.
fn mac_window_hints() {
    sdl2::hint::set("SDL_VIDEO_MAC_FULLSCREEN_SPACES", "1");
    sdl2::hint::set("SDL_HINT_MAC_BACKGROUND_APP", "0");
    // Ctrl-click is a right click on this platform, and taking it would make
    // the trackpad's own secondary click unreachable.
    sdl2::hint::set("SDL_MAC_CTRL_CLICK_EMULATE_RIGHT_CLICK", "1");
}

/// Where focus lands once the frame at `closed` is removed from `before`
/// frames: still on the frame the user was using, shifted down by one if it sat
/// after the hole, and clamped when the focused frame is the one that went.
fn focus_after_close(focus: usize, closed: usize, before: usize) -> usize {
    let last = before.saturating_sub(2); // highest index that survives
    if focus > closed { focus - 1 } else { focus }.min(last)
}

fn main() -> anyhow::Result<()> {
    mac_window_hints();
    let sdl = sdl2::init().map_err(|e| anyhow::anyhow!("SDL init: {e}"))?;
    // One renderer per frame, in frame order. See the module docs.
    let mut renderers = vec![Renderer::new(&sdl, "zemacs", WINDOW_W, WINDOW_H)?];
    let mut pump = sdl
        .event_pump()
        .map_err(|e| anyhow::anyhow!("SDL event pump: {e}"))?;
    // Only the perf report reads this, but it has to be SDL's own clock: an
    // event's timestamp is in it, and the interesting part of a keystroke's life
    // is over before this loop ever sees the event.
    let timer = sdl.timer().map_err(|e| anyhow::anyhow!("SDL timer: {e}"))?;

    // Text input is toggled per mode — see `wants_text_input`. It is off to
    // begin with because the editor opens on the dashboard.
    let video = sdl.video().map_err(|e| anyhow::anyhow!("SDL video: {e}"))?;
    let text_input = video.text_input();
    text_input.stop();
    let mut text_input_on = false;

    let init_path = resolve_init_path();
    let (tx, rx): (Sender<EditorCommand>, Receiver<EditorCommand>) =
        crossbeam_channel::unbounded();

    // The editor is shared with the Lisp image, which reads and edits it
    // directly rather than only being able to shout commands at it. Seeded
    // before `spawn` so `init.lisp` cannot observe a half-built dashboard.
    let shared: zemacs_core::Shared = Default::default();
    {
        let mut editor = shared.lock().expect("fresh mutex");
        seed_dashboard(&mut editor, &init_path, &renderers[0].backend());
        // Any file named on the command line opens instead of the dashboard.
        if let Some(arg) = std::env::args().nth(1) {
            open_file(&mut editor, &PathBuf::from(arg), &init_path);
        }
    }
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init_path.clone());

    // Thread three: highlighting. The main thread owns input and drawing, the
    // Lisp thread owns the image, and neither ever waits on a parse.
    let highlighter = zemacs_syntax::spawn_worker();

    let mut last_revision = u64::MAX;
    let mut last_file_query: Option<String> = None;
    let mut last_grep: Option<String> = None;
    let mut keys: Vec<Key> = Vec::new();
    // There used to be a `swallow_text` flag here: an Alt keydown armed it and
    // the `TextInput` macOS composed from the combo (⌥- is –, ⌥= is ≠) was
    // dropped, because Option was a Meta fallback and `M--` had already been
    // dispatched. The config sets `mac-option-modifier 'none`, so that was
    // backwards — the composed character *is* the thing being asked for. Option
    // is no longer a modifier here (see `key_from_keydown`) and the text it
    // produces is now inserted like any other.
    let mut mouse = Mouse::default();
    let mut cursors = Cursors::new();
    let mut magit = Magit::default();
    let mut dired = Dired::default();
    let mut term = Term::default();
    let mut project = Project::default();
    let mut last_autosave = Instant::now();
    let mut last_revert = Instant::now();
    let mut revert_watch = Revert::default();
    let mut clipboard = Clipboard::new(&video);
    let mut perf = Perf::new();
    // Whether the last iteration put anything on screen, which is the same
    // question as "has this one already been paced": a present blocks until the
    // display's next vertical blank, and one that did not happen has to be paced
    // some other way. True to begin with so the first frame goes up at once.
    let mut presented = true;

    'main: loop {
        // The one place this loop is allowed to be idle, and most of what "the
        // cursor feels slow" turned out to be. `wait_event_timeout` returns the
        // *instant* a key, a click or a window event arrives, so a keystroke
        // that lands here starts its redraw immediately — instead of sitting in
        // SDL's queue until the loop came back round from a present it was
        // spending on an unchanged picture anyway.
        //
        // The timeout is not a frame clock; nothing the user does needs it. It
        // bounds one thing only: how long a change made by the *Lisp thread* —
        // which reaches the editor through the shared mutex and raises no SDL
        // event — can sit undrawn. One display frame is as long as that should
        // ever be.
        //
        // Before the lock, and that is load-bearing. Parking here holding the
        // editor would put every Lisp primitive behind the user's next
        // keystroke, which is precisely what `docs/threading.org` promises never
        // happens.
        let idle = Instant::now();
        let waited = (!presented)
            .then(|| pump.wait_event_timeout(renderers.first().map_or(16, Renderer::frame_ms)))
            .flatten();
        let frame_start = Instant::now();
        perf.idle += frame_start - idle;

        // Held for this whole iteration except the present at the bottom, which
        // is where the frame's idle time actually is. A Lisp primitive waits at
        // most for one iteration's worth of input handling and drawing, never
        // for the display — that is what keeps a slow config from being felt.
        let mut editor = shared.lock().unwrap_or_else(|e| e.into_inner());
        keys.clear();
        // At most one window closes per iteration: a close shifts every later
        // frame index down, and the rest of this batch was routed against the
        // old ones. Deferring it keeps the whole batch consistent.
        let mut closing: Option<usize> = None;
        // When the earliest keystroke of this batch was stamped, for the perf
        // report to subtract from the present at the bottom.
        let mut typed: Option<u32> = None;

        // The event the wait above returned is the first of the batch — dropping
        // it would lose exactly the keystroke that woke us.
        for event in waited.into_iter().chain(pump.poll_iter()) {
            match event {
                Event::Quit { .. } => break 'main,
                // Event-driven rather than polled: reading the clipboard is a
                // trip through the window server, and SDL is already watching
                // it for us. ponytail: if a platform turns out not to raise
                // this, the fallback is a once-a-second pull beside the
                // auto-save timer — same call, worse latency.
                Event::ClipboardUpdate { .. } => clipboard.pull(&mut editor),
                Event::Window {
                    window_id,
                    win_event,
                    ..
                } => {
                    let frame = frame_for_window(renderers.iter().map(Renderer::window_id), window_id);
                    // Anything the *window system* says happened is a reason to
                    // put the frame up again whether or not the editor moved:
                    // exposed, resized, un-minimised, dragged to another
                    // display. This is the one case where "the picture is
                    // identical" is not the same as "the screen is right", and
                    // it is what the skipped present below is safe *because* of.
                    // Window events are rare, so the extra present costs nothing
                    // anyone can measure.
                    if let Some(i) = frame {
                        renderers[i].invalidate();
                    }
                    match win_event {
                        // The window manager decides which frame is current and
                        // the editor follows it; there is no other source of
                        // truth.
                        //
                        // ponytail: a bare assignment, so the *live* buffer and
                        // cursor follow the pointer into the newly focused frame
                        // instead of that frame's own window being adopted. Core
                        // does the adopting (`Editor::adopt_window`) but only
                        // from inside `apply`, and it has no command for "focus
                        // frame N" — every window command works on
                        // `focus_frame`. Fixing it properly means an
                        // `EditorCommand::FocusFrame(usize)` in core, next to
                        // `NewFrame`; until then two frames showing different
                        // buffers swap contents when you click between them.
                        WindowEvent::FocusGained => {
                            if let Some(i) = frame {
                                // Through the command, not a bare assignment:
                                // the live buffer belongs to the focused window,
                                // so core has to park it and adopt the new
                                // frame's.
                                let cmd = EditorCommand::FocusFrame(i);
                                dispatch(&mut editor, &lisp, cmd, &init_path, &mut renderers, &mut magit, &mut dired, &mut term, &mut project);
                            }
                        }
                        WindowEvent::Close => {
                            if let Some(i) = frame {
                                closing.get_or_insert(i);
                            }
                        }
                        _ => {}
                    }
                }
                // Keys are not routed by window: the editor is global and the
                // focused frame is where they land.
                Event::KeyDown {
                    keycode: Some(kc),
                    keymod,
                    timestamp,
                    ..
                } => {
                    typed = Some(typed.map_or(timestamp, |t: u32| t.min(timestamp)));
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
                            &mut term,
                            &mut project,
                        );
                        if let Some(window) = mouse.press(&editor.frames[i], i, area, x, y) {
                            let cmd = EditorCommand::FocusWindow(window);
                            dispatch(&mut editor, &lisp, cmd, &init_path, &mut renderers, &mut magit, &mut dired, &mut term, &mut project);
                        }
                        // A program that asked for mouse events gets the click:
                        // that is what makes vim, htop and tmux usable in here.
                        // Focusing the pane happened first, so clicking into an
                        // unfocused terminal both focuses it and reaches the
                        // program in one gesture.
                        if editor.mode == zemacs_core::Mode::Terminal {
                            let (col, row) = cell_at(&editor, &renderers, x, y);
                            term.mouse(&editor, zemacs_term::Mouse {
                                button: zemacs_term::Button::Left,
                                kind: zemacs_term::MouseKind::Press,
                                col,
                                row,
                            });
                        }
                    }
                }
                Event::MouseButtonUp {
                    window_id,
                    mouse_btn: MouseButton::Left,
                    x,
                    y,
                    ..
                } => {
                    mouse.release();
                    // A press with no release is half a gesture. SGR reporting
                    // makes the two separate events, so a program that only
                    // ever hears the press has a button held down forever —
                    // which is why clicking in `vim` or `htop` in here did
                    // nothing until the *next* click.
                    if editor.mode == zemacs_core::Mode::Terminal {
                        if let Some(i) =
                            frame_for_window(renderers.iter().map(Renderer::window_id), window_id)
                        {
                            let (x, y) = renderers[i].to_pixels(x, y);
                            let (col, row) = cell_at(&editor, &renderers, x, y);
                            term.mouse(&editor, zemacs_term::Mouse {
                                button: zemacs_term::Button::Left,
                                kind: zemacs_term::MouseKind::Release,
                                col,
                                row,
                            });
                        }
                    }
                }
                Event::MouseMotion {
                    window_id,
                    x,
                    y,
                    mousestate,
                    ..
                } => match mouse.dragging() {
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
                            // Held-button motion is a drag, which is how a
                            // selection is made in `vim` or a pane resized in
                            // `tmux`. The terminal drops it unless the child
                            // asked for drag reporting, so this costs nothing
                            // for a program that did not.
                            if mousestate.left() && editor.mode == zemacs_core::Mode::Terminal {
                                let (col, row) = cell_at(&editor, &renderers, x, y);
                                term.mouse(&editor, zemacs_term::Mouse {
                                    button: zemacs_term::Button::Left,
                                    kind: zemacs_term::MouseKind::Drag,
                                    col,
                                    row,
                                });
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
                            dispatch(&mut editor, &lisp, cmd, &init_path, &mut renderers, &mut magit, &mut dired, &mut term, &mut project);
                        }
                        // A terminal has its own scrollback, and while the shell
                        // has the keyboard the buffer holds only the *visible*
                        // grid — scrolling that would move through a screenful
                        // that is already all there is. `wheel` also knows to
                        // hand the notch to a program that asked for mouse
                        // events, or to send arrow keys to `less`.
                        if editor.mode == zemacs_core::Mode::Terminal {
                            let (col, row) = cell_at(&editor, &renderers, px, py);
                            term.wheel(&editor, -y * SCROLL_LINES, col, row);
                        } else {
                            let cmd = EditorCommand::ScrollLines(-y * SCROLL_LINES);
                            dispatch(&mut editor, &lisp, cmd, &init_path, &mut renderers, &mut magit, &mut dired, &mut term, &mut project);
                        }
                    }
                }
                // Every character macOS hands us, including the ones it composed
                // from an Option combo and the second half of a dead-key pair.
                Event::TextInput {
                    text, timestamp, ..
                } => {
                    // Insert mode arrives here rather than as `KeyDown`, and it
                    // is the most latency-sensitive thing anyone does.
                    typed = Some(typed.map_or(timestamp, |t: u32| t.min(timestamp)));
                    keys.extend(text.chars().map(Key::Char));
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
                dispatch(&mut editor, &lisp, cmd, &init_path, &mut renderers, &mut magit, &mut dired, &mut term, &mut project);
            }
        }
        while let Ok(cmd) = rx.try_recv() {
            dispatch(&mut editor, &lisp, cmd, &init_path, &mut renderers, &mut magit, &mut dired, &mut term, &mut project);
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
            // The image's one signal that the document moved. Core reports mode
            // *entry* and nothing else, so without this a language server could
            // never learn that a buffer changed — and neither could anything
            // else a config wants to hang off an edit. Queued through
            // `pending_hooks` so it takes the same route, and the same
            // `fboundp` guard, as every other hook.
            //
            // ponytail: it carries no *delta*. Lisp is told the buffer changed
            // and has to read the whole thing back, which is why the LSP client
            // sends full-text `didChange`. The upgrade is core recording
            // `(start, old-end, new-end)` per edit — the same record overlays
            // will want, and worth doing once rather than twice.
            //
            // Not for a generated buffer: a terminal rewrites itself as fast as
            // the shell prints, and a Lisp form per frame of `ls -R` would
            // starve the image's queue for work no config can act on anyway —
            // every generated buffer is read-only.
            if !editor.buffer.kind.is_generated() {
                editor.pending_hooks.push("after-change-hook".into());
            }
            match &editor.buffer.language {
                Some(lang) => {
                    highlighter.request(editor.revision, lang, editor.buffer.text.to_string())
                }
                // A generated buffer colours itself — dired and magit hand their
                // own spans straight to the editor — so clearing here would
                // wipe them one frame after they were produced.
                None if !editor.buffer.kind.is_generated() => editor.buffer.highlights.clear(),
                None => {}
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

        // Anything a JSON-RPC child said since the last frame. This is the whole
        // answer to "how does an async reply reach Lisp": a reader thread parses
        // the child's output and pushes it onto a channel, *this* thread turns
        // each message into a Lisp form, and the image evaluates it on its own
        // thread — the same route a mode hook takes, and the only one that never
        // calls into ECL from a foreign thread.
        //
        // `%rpc-event` is guarded exactly as a hook is: a build whose config
        // never loaded `runtime/rpc.lisp` has nothing to call, and that must be
        // silence rather than an error per message.
        while let Some((conn, event)) = zemacs_rpc::poll() {
            let (kind, form) = match event {
                zemacs_rpc::Event::Message(v) => (":message", zemacs_rpc::lisp::to_lisp(&v)),
                zemacs_rpc::Event::Protocol(e) => (":error", zemacs_rpc::lisp::string(&e)),
                zemacs_rpc::Event::Exited(e) => (":exit", zemacs_rpc::lisp::string(&e)),
            };
            lisp.eval(format!(
                "(let ((h (find-symbol \"%RPC-EVENT\" :zemacs))) \
                   (when (and h (fboundp h)) (funcall h {conn} {kind} '{form})))"
            ));
        }

        refresh_file_completions(&mut editor, &mut last_file_query);
        refresh_grep(&mut editor, &project, &mut last_grep);

        // Adopt a result only if the buffer hasn't moved on; if it has, a newer
        // parse is already in flight and the current spans stay up meanwhile.
        if let Some((revision, spans)) = highlighter.poll() {
            if revision == editor.revision {
                editor.buffer.highlights = spans;
            }
        }

        // Where `M-x new-frame` becomes a window. Core pushes the frame, the
        // loop notices it has no renderer for it. Emacs spells extra frames
        // `<2>`, `<3>`; so do we, so they are tellable apart in the dock.
        while renderers.len() < editor.frames.len() {
            let title = format!("zemacs <{}>", renderers.len() + 1);
            let mut renderer = Renderer::new(&sdl, &title, WINDOW_W, WINDOW_H)?;
            // Keys are routed to `focus_frame`, not to whichever window the OS
            // considers frontmost, so a new frame has to claim it here. macOS
            // does not always send `FocusGained` for a window the application
            // opened itself, and without this every keystroke kept going to the
            // frame you opened the new one *from*.
            renderer.focus();
            editor.focus_frame = renderers.len();
            renderers.push(renderer);
        }

        // Size each session to the pane it is actually shown in, then let them
        // all catch up. `sync` is also what answers a child's queries, so it has
        // to run every frame whether or not the terminal is on screen — a
        // program asking how big its window is blocks until it is told, and a
        // background agent that nobody polls is a background agent that hangs.
        //
        // Measured out here because `Term` owns no renderer: it says which
        // buffers it has, this says how big each one's pane is.
        if term.is_live() {
            let sizes: Vec<(zemacs_core::BufferId, usize, usize)> = term
                .buffers()
                .into_iter()
                .map(|id| {
                    let (cols, rows) = terminal_size(&editor, &renderers, id);
                    (id, cols, rows)
                })
                .collect();
            term.sync(&mut editor, &sizes);
        }

        // Wall-clock rather than keystroke-counted: the loop already runs at
        // vsync, so an elapsed check is free, and a crash costs at most this
        // interval's worth of typing either way.
        if last_autosave.elapsed() >= AUTOSAVE_EVERY {
            autosave_all(&mut editor);
            last_autosave = Instant::now();
        }

        // Same shape, same argument, one order of magnitude more often: this one
        // is a `stat` rather than a write, and five seconds is how long a file
        // is allowed to be stale on screen.
        if last_revert.elapsed() >= REVERT_EVERY {
            revert_watch.poll(&mut editor);
            last_revert = Instant::now();
        }

        // After the whole batch, so a yank and the delete before it push once
        // rather than twice — the register only has to be right by the time
        // anyone outside can ask.
        clipboard.push(&editor);

        // Park the live cursor and scroll on the focused window, once, so every
        // pane in every frame can be drawn from its own `Window`.
        editor.sync_focused_window();
        let screen = term.screen(&editor);
        perf.input += frame_start.elapsed();

        // Every window, every iteration, and `render` syncs the font itself.
        // Drawing is the cheap half — measured at 2.4 ms for a full screen of
        // text, against 14 ms of vertical blank below — so nothing here is
        // conditional. What it produces besides pixels is a digest of every draw
        // call it made, which is what lets the present be skipped.
        //
        // ponytail: an idle editor still redraws at the refresh rate, throwing
        // the frame away when the digest says it was identical. That is a couple
        // of milliseconds of CPU per display frame doing nothing, and it buys
        // the one thing a cheaper test cannot: correctness without a list of
        // "fields that mean a redraw" to keep in step with the renderer. The
        // upgrade path is core stamping a generation on every mutation — the
        // *only* signal that also catches a Lisp primitive editing the buffer
        // through the shared mutex, which raises no event here — and then this
        // loop can skip the draw as well as the present, and sleep properly.
        let drawing = Instant::now();
        for (i, renderer) in renderers.iter_mut().enumerate() {
            renderer.render(&mut editor, i, screen.as_ref())?;
        }
        perf.draw += drawing.elapsed();
        let draws: u32 = renderers.iter().map(Renderer::draw_calls).sum();

        // Drawing is done; the editor is nobody's until the next iteration.
        // Presenting parks this thread until the next vertical blank, which is
        // most of the frame, and holding the lock across it would put every
        // Lisp primitive behind the display.
        drop(editor);
        let presenting = Instant::now();
        let mut presents = 0;
        for renderer in renderers.iter_mut() {
            // Only what changed. A present of an identical picture costs a whole
            // vertical blank and shows the user nothing, and it is the frame the
            // *next* keystroke would rather have been drawn in.
            //
            // Measured, since the comment that used to sit here guessed
            // otherwise: two visible windows both presenting take one vertical
            // blank between them on macOS/Metal, not two — the second swapchain
            // has a free drawable and returns at once. So this is not about the
            // count of presents; it is about not spending a blank at all on a
            // frame nobody asked for.
            if !renderer.changed() {
                continue;
            }
            renderer.present();
            presents += 1;
        }
        presented = presents > 0;
        perf.present += presenting.elapsed();
        // A keystroke that changed nothing on screen — `k` at the top of the
        // buffer — has no latency to report, only a frame that decided not to
        // happen.
        if let Some(stamped) = typed.filter(|_| presented) {
            perf.key(timer.ticks().saturating_sub(stamped));
        }
        perf.frame(frame_start, presents, draws);
    }
    // Language servers are children of this process and outlive it otherwise —
    // one stray `clangd` indexing a repository per session, which is the kind of
    // thing you only notice when the fan starts.
    zemacs_rpc::stop_all();
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
    term: &mut Term,
    project: &mut Project,
) {
    match cmd {
        EditorCommand::CallLisp(form) => lisp.eval(form),
        EditorCommand::Term(verb) => term.run(editor, &verb),
        EditorCommand::Project(verb) => {
            project.run_verb(editor, &verb);
            // `compile` and `test` ask for a shell command; running one belongs
            // to the terminal, which is the only thing here that owns a process.
            if let Some(command) = project.run.take() {
                term.run(editor, "open");
                let mut line = command.program.clone();
                for arg in &command.args {
                    line.push(' ');
                    line.push_str(arg);
                }
                line.push('\r');
                term.send(editor, line.into_bytes());
            }
        }
        // Dropped when no shell is running: a keystroke aimed at something that
        // is not there is nothing, not an error worth reporting on every key.
        EditorCommand::TermKey(key) => {
            term.key(editor, key);
        }
        // dired borrows the file prompt for rename/copy/mkdir, so an answer to
        // one of those is a filename for *it* rather than a file to open.
        EditorCommand::OpenFile(path) if dired.awaiting_input() => {
            dired.supply(editor, &path.to_string_lossy())
        }
        // Opening a directory lists it, as `find-file` does in Emacs.
        EditorCommand::OpenFile(path) if path.is_dir() => {
            // Remembered *before* it is listed, so that browsing to a repository
            // puts it in the project switcher's history. Without this the only
            // way into that list was opening a file inside a project, which is
            // the chicken-and-egg behind "there is no select project" — the
            // switcher could only ever offer somewhere you had already been.
            project_remember(&path);
            editor.buffer.path = Some(path);
            dired.run(editor, "open");
        }
        EditorCommand::OpenFile(path) => open_file(editor, &path, init_path),
        EditorCommand::OpenAt(hit) => {
            let root = project.search_root(editor);
            open_at(editor, &root, &hit, init_path);
        }
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
            editor.buffer.file_mode = file_mode(&path);
            editor.apply(EditorCommand::Message(match recovery_for(&path) {
                // Deliberately does not load it: the disk file is what was
                // asked for, and silently showing different text is worse than
                // one loud line. The copy is a plain file at a printable path,
                // so recovering is `:e` on the name in this message —
                // ponytail: a `recover-file` verb when that gets old.
                Some(copy) => format!("opened {shown} — newer auto-save at {}", copy.display()),
                None => format!("opened {shown}"),
            }));
            remember_recent(&path);
            project_remember(&path);
        }
        Err(e) => editor.apply(EditorCommand::Message(format!(
            "{}: {e}",
            display_path(&path)
        ))),
    }
}

/// Open the file a ripgrep hit names and put the cursor on its line.
///
/// The hit is `path:line:text`, so it is split from the *left* twice and no
/// further — a match whose text contains a colon is the common case, not an
/// edge one.
fn open_at(editor: &mut Editor, root: &Path, hit: &str, init_path: &Path) {
    let mut parts = hit.splitn(3, ':');
    let (Some(path), Some(line)) = (parts.next(), parts.next()) else {
        editor.apply(EditorCommand::Message(format!("not a match: {hit}")));
        return;
    };
    // Relative, because ripgrep ran with the project root as its directory.
    open_file(editor, &root.join(path), init_path);
    // ripgrep counts from 1. A hit for a file that changed under us lands on
    // the last line rather than refusing to open it at all.
    if let Ok(line) = line.parse::<usize>() {
        let target = line.saturating_sub(1).min(editor.buffer.len_lines());
        editor.buffer.move_to_line_col(target, 0);
        editor.buffer.cursor = editor.buffer.first_non_blank(target);
    }
}

/// Ask ripgrep for matches. Empty for a pattern too short to be worth running —
/// one character across a large tree is tens of thousands of hits and a visible
/// stall.
fn grep(root: &Path, pattern: &str) -> Vec<String> {
    const MIN: usize = 2;
    const LIMIT: usize = 2000;
    if pattern.trim().len() < MIN {
        return Vec::new();
    }
    // `--` so a pattern starting with a dash is a pattern, not a flag.
    let out = std::process::Command::new("rg")
        .current_dir(root)
        .args(["--line-number", "--no-heading", "--color=never", "--smart-case"])
        .arg("--max-count=50")
        .arg("--")
        .arg(pattern)
        .output();
    let Ok(out) = out else {
        return vec!["ripgrep (rg) is not installed".into()];
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .take(LIMIT)
        .map(str::to_string)
        .collect()
}

/// Re-run ripgrep when the pattern changes.
///
/// Mirrors [`refresh_file_completions`]: core does no IO, so the candidates are
/// pushed in from here. Keyed on the pattern so a keystroke that only moves the
/// selection does not re-run the search.
fn refresh_grep(editor: &mut Editor, project: &Project, last: &mut Option<String>) {
    let Some(prompt) = editor.prompt.as_mut() else {
        *last = None;
        return;
    };
    if prompt.kind != PromptKind::Grep {
        *last = None;
        return;
    }
    if last.as_deref() == Some(prompt.text.as_str()) {
        return;
    }
    *last = Some(prompt.text.clone());
    // Held across the search, since `search_root` needs the editor back.
    let pattern = prompt.text.clone();
    let root = project.search_root(editor);
    let hits = grep(&root, &pattern);
    if let Some(prompt) = editor.prompt.as_mut() {
        prompt.set_items(hits);
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
            editor.buffer.file_mode = file_mode(&target);
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
            autosave_forget(&editor.buffer);
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
    let (dir, name) = match typed.rsplit_once('/') {
        Some((d, name)) => (if d.is_empty() { "/" } else { d }.to_string(), name),
        None => (".".to_string(), typed.as_str()),
    };
    // Hidden entries appear exactly when you have started typing one. Without
    // it `~/.config` and `~/.emacs.d` are unreachable from this prompt, which is
    // half of "look at the entire filesystem"; with it unconditional, every
    // listing in a repository opens on `.git`. The dot is part of the cache key
    // because it changes what the listing *is*, not merely how it is filtered.
    let hidden = name.starts_with('.');
    let key = format!("{}{dir}", if hidden { "." } else { "" });
    if last_query.as_deref() == Some(key.as_str()) {
        return;
    }
    *last_query = Some(key);

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
        // The trim is load-bearing: a directory's entry carries a trailing `/`,
        // so the last component of `~/.config/` without it is the *empty
        // string* — which is how hidden directories slipped through this filter
        // for as long as it has existed while hidden files did not.
        .filter(|p| {
            hidden
                || !p
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .starts_with('.')
        })
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

// --- the system clipboard ------------------------------------------------
//
// `select-enable-clipboard t` is set in the config, so the unnamed register and
// the system clipboard are one thing: `yy` here pastes into a browser, and `⌘C`
// there is what `p` puts back. Core owns the register and cannot reach the
// window system, so the mirroring lives here.
//
// Push is polled once a frame off `register_revision` — an integer compare, not
// a string diff. Pull happens only on a paste, because reading the clipboard is
// a trip through the window server and doing it 60 times a second to answer a
// question nobody asked is exactly the kind of thing that shows up in a profile.

struct Clipboard {
    util: sdl2::clipboard::ClipboardUtil,
    /// The revision already pushed. Also updated after a *pull*, so adopting
    /// the clipboard's text does not read back as a register change and bounce
    /// straight out again.
    pushed: u64,
}

impl Clipboard {
    fn new(video: &sdl2::VideoSubsystem) -> Self {
        Clipboard {
            util: video.clipboard(),
            pushed: 0,
        }
    }

    fn push(&mut self, editor: &Editor) {
        let revision = editor.register_revision();
        if revision == self.pushed {
            return;
        }
        self.pushed = revision;
        let (text, _) = editor.register();
        if text.is_empty() {
            return;
        }
        // Silent on failure: no clipboard (the dummy video driver in tests, a
        // headless session) must not get between someone and their yank.
        let _ = self.util.set_clipboard_text(text);
    }

    /// Adopt the clipboard if it says something the register does not. Equal
    /// text is left alone so a plain `yy p` keeps its linewise-ness — the
    /// clipboard is a bare string and has no idea whether it holds whole lines.
    fn pull(&mut self, editor: &mut Editor) {
        let Ok(text) = self.util.clipboard_text() else {
            return;
        };
        if text.is_empty() || text == editor.register().0 {
            return;
        }
        // Text from outside is linewise only if it looks it — a trailing
        // newline is what `yy` would have produced, and pasting a whole line
        // above or below beats pasting it into the middle of the current one.
        let linewise = text.ends_with('\n');
        editor.adopt_register(text, linewise);
        self.pushed = editor.register_revision();
    }
}

// --- auto-save -----------------------------------------------------------
//
// Emacs writes `#foo.rs#` beside the file; this writes into
// `~/.config/zemacs/auto-save/` instead, so a project tree never grows litter
// that its `.gitignore` has to know about. The name is the absolute path with
// `/` turned into `!` — Emacs' own mangling, and the reason it is recoverable
// by eye when you need to go looking.

const AUTOSAVE_EVERY: Duration = Duration::from_secs(30);

fn autosave_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/zemacs/auto-save"))
}

fn autosave_file(path: &Path) -> Option<PathBuf> {
    let mangled = path.to_string_lossy().replace('/', "!");
    Some(autosave_dir()?.join(format!("#{mangled}#")))
}

/// `None` for a buffer with no file behind it: there is nothing to recover
/// *to*, and a name mangled from an empty path collides with every other one.
fn autosave_path(buffer: &zemacs_core::Buffer) -> Option<PathBuf> {
    autosave_file(buffer.path.as_ref()?)
}

/// Every modified buffer, not just the focused one — the whole point is the
/// buffer you were *not* looking at when the editor died.
fn autosave_all(editor: &mut Editor) {
    // `sync_focused_window` has not run yet this iteration, but auto-save only
    // reads text and path, and neither is window state.
    autosave_one(&editor.buffer);
    for buffer in &editor.others {
        autosave_one(buffer);
    }
}

fn autosave_one(buffer: &zemacs_core::Buffer) {
    // A dired listing or a magit status is a rendered view, not a document:
    // recovering one would restore a screenshot over a real file.
    if !buffer.modified || buffer.kind.is_generated() {
        return;
    }
    let Some(target) = autosave_path(buffer) else {
        return;
    };
    if let Some(dir) = target.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Silent on failure, like `remember_recent`: a full disk must not put a
    // message in front of someone who is mid-sentence.
    let _ = std::fs::write(target, buffer.text.to_string());
}

/// The auto-save copy of `path`, if there is one *and* it is newer than the
/// file. An older copy is one the last real save already superseded, and
/// offering it would be offering to go backwards.
fn recovery_for(path: &Path) -> Option<PathBuf> {
    let copy = autosave_file(path)?;
    let saved = std::fs::metadata(&copy).ok()?.modified().ok()?;
    let on_disk = std::fs::metadata(path).ok()?.modified().ok()?;
    (saved > on_disk).then_some(copy)
}

/// Drop the recovery copy once the real file holds the same text. Emacs does
/// this too — a stale `#file#` left behind is a recovery prompt for an edit
/// that was already saved.
fn autosave_forget(buffer: &zemacs_core::Buffer) {
    if let Some(path) = autosave_path(buffer) {
        let _ = std::fs::remove_file(path);
    }
}

// ponytail: auto-save only, no numbered backups — the config keeps 20 versions
// of every file, and that is a separate mechanism (copy the old contents aside
// *before* writing) rather than a knob on this one. Add it when losing the
// previous saved version actually bites.

// --- auto-revert ---------------------------------------------------------
//
// `global-auto-revert-mode` is on in the config: a file that moved underneath
// the editor — `git checkout`, a formatter, the other end of a rebase, and now
// a coding agent halfway through a refactor — should be what is on screen, not
// what used to be.
//
// # Polled, and why not watched
//
// This used to be one `stat` on the live buffer every five seconds, which was
// the right size for "a formatter ran". It is the wrong size for an agent: it
// writes six files in a burst, five of which you are not looking at, and
// finding out one buffer switch at a time is not "quickly". So the sweep is now
// *every* buffer with a file behind it, four times a second.
//
// `notify` was the obvious alternative and is not used, for three reasons that
// are about this problem rather than about dependencies in general:
//
//  1. On macOS `notify` is FSEvents, which reports *directories* and coalesces
//     on its own latency window. Learning that a directory changed still leaves
//     you stat-ing the file to find out whether *this* buffer's file did — so
//     the watcher does not remove the syscall, it removes the loop around it.
//  2. The loop it removes is small. N is the number of open buffers — tens, not
//     thousands — so this is a few dozen `stat`s per tick against a page cache
//     that already has every one of those inodes hot. The measurable cost of
//     the old five-second version was zero; this is twenty times as much of
//     zero.
//  3. What it would add is a background thread, a per-platform backend, and a
//     second source of truth about when a file changed — next to the two
//     safety properties below, which are the whole reason this code is
//     careful. Those properties are tested; re-deriving them on the far side of
//     a channel is how you lose one.
//
// The condition for changing the answer is a real one and worth writing down:
// if a project ever opens hundreds of buffers, or if any of them lives on a
// filesystem where `stat` is a network round trip (`crates/tramp`), the sweep
// stops being free and a watcher — or simply a longer interval for remote
// paths — earns itself.
//
// Two safety properties survive from the five-second version, and both are the
// point rather than details:
//
//  - a *modified* buffer is never silently clobbered; it gets one message and
//    keeps its text;
//  - the comparison is on *content*, not only on the mtime, so the buffer that
//    just saved a file does not revert itself.

/// Four times a second. Fast enough that an agent's edit is on screen before
/// you have finished reading the line it changed, and slow enough that it is
/// still a rounding error next to a frame.
const REVERT_EVERY: Duration = Duration::from_millis(250);

#[derive(Default)]
struct Revert {
    /// The modification time already accounted for, per file. A path is only
    /// ever compared against its own last-seen stamp, so the *first* sight of
    /// one records it and reverts nothing — opening a file is not a change to
    /// it. Keyed by path rather than by buffer id so switching away and back
    /// does not re-ask a question that was already answered.
    seen: std::collections::HashMap<PathBuf, std::time::SystemTime>,
}

impl Revert {
    /// One `stat` per open file, and a read only where the stamp moved.
    fn poll(&mut self, editor: &mut Editor) {
        // A dired listing's `path` is a directory and a magit status is a
        // rendered view. Neither has a file behind it that could be newer, and
        // both already refresh on their own verbs. Collected first because the
        // revert below needs `editor` mutably.
        let watched: Vec<(zemacs_core::BufferId, PathBuf)> = std::iter::once(&editor.buffer)
            .chain(editor.others.iter())
            .filter(|b| !b.kind.is_generated())
            .filter_map(|b| b.path.clone().map(|p| (b.id, p)))
            .collect();

        for (id, path) in watched {
            self.check(editor, id, &path);
        }

        // Forget files nothing has open any more, so a long session's map does
        // not grow by every file ever visited. Cheap: it is the same length as
        // the sweep that just ran.
        self.seen.retain(|path, _| {
            std::iter::once(&editor.buffer)
                .chain(editor.others.iter())
                .any(|b| b.path.as_deref() == Some(path.as_path()))
        });
    }

    fn check(&mut self, editor: &mut Editor, id: zemacs_core::BufferId, path: &Path) {
        let Ok(stamp) = std::fs::metadata(path).and_then(|m| m.modified()) else {
            // Gone, or unreadable. Emacs keeps the buffer and stays quiet until
            // you try to save it; so do we, because the text on screen has just
            // become the only copy and throwing it away would be the one
            // unrecoverable move available here.
            return;
        };
        match self.seen.insert(path.to_path_buf(), stamp) {
            Some(seen) if seen == stamp => return, // untouched since last look
            Some(_) => {}                          // moved — worth a read
            None => return,                        // first sight
        }

        let Some(buffer) = editor.buffer_by_id(id) else {
            return;
        };
        // The one thing a revert must never do. `insert` above already recorded
        // the new stamp, so this is said once per external change rather than
        // four times a second until you deal with it.
        //
        // ponytail: saving over the newer file afterwards is still allowed and
        // still silent — that is Emacs' "has changed since visited; save
        // anyway?", and it belongs with the confirmation flow the destructive
        // git verbs are waiting on rather than bolted on here.
        if buffer.modified {
            editor.apply(EditorCommand::Message(format!(
                "{} changed on disk — not reverted, this buffer has unsaved changes",
                display_path(path)
            )));
            return;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        // A save moves the mtime too, and this is what stops `:w` reverting the
        // buffer that just wrote the file: the text is identical, so there is
        // nothing to do. It costs one read of a file that genuinely changed, and
        // it makes a bare `touch` a no-op — which is the honest answer, since
        // nothing about the document moved.
        if text == buffer.text.to_string() {
            return;
        }
        revert(editor, id, path, &text);
    }
}

/// Replace buffer `id`'s text with what is on disk, keeping point where it was.
///
/// Through `Editor::revert_buffer` rather than by assigning `buffer.text`:
/// `splice` is the only thing that moves markers and overlays, and reaching
/// around it is exactly how a marker comes to name an offset in a document that
/// no longer exists. It also works on a buffer that is *not* live, which is what
/// lets an agent's whole burst of edits land at once instead of one buffer
/// switch at a time.
fn revert(editor: &mut Editor, id: zemacs_core::BufferId, path: &Path, text: &str) {
    if !editor.revert_buffer(id, text) {
        return;
    }
    // Permissions travel with the content — a `chmod +x` between saves shows up
    // in the modeline instead of going stale there.
    let mode = file_mode(path);
    if let Some(buffer) = std::iter::once(&mut editor.buffer)
        .chain(editor.others.iter_mut())
        .find(|b| b.id == id)
    {
        buffer.file_mode = mode;
    }
    // Loud, because the text changed without anyone typing — and louder still
    // for a buffer you are not looking at, which is the only notice you get.
    // ponytail: no checkpoint, so `u` restores the last thing *you* did rather
    // than the pre-revert text. Emacs discards the undo list here outright; this
    // keeps it, which is the more forgiving of the two and costs nothing.
    editor.apply(EditorCommand::Message(format!(
        "reverted {} — it changed on disk",
        display_path(path)
    )));
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

/// Unix mode bits of `path`, for the modeline. `None` for anything that cannot
/// be stat'd, which is one fewer thing on the strip rather than an error.
/// Record the project a freshly opened file belongs to, so the switcher's
/// history is a by-product of use rather than something to curate.
fn project_remember(path: &Path) {
    if let Some(found) = zemacs_project::find(path) {
        zemacs_project::remember(&found.root);
    }
}

fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).ok().map(|m| m.permissions().mode())
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

/// The cell a pixel lands on, relative to the pane showing the terminal.
///
/// The terminal thinks in cells and knows nothing about panes or HiDPI, so this
/// is where those stop being its problem.
fn cell_at(editor: &Editor, renderers: &[Renderer], x: i32, y: i32) -> (usize, usize) {
    let index = editor.focus_frame.min(renderers.len().saturating_sub(1));
    let Some(renderer) = renderers.get(index) else {
        return (0, 0);
    };
    let (cell_w, line_h) = renderer.cell_size();
    // The mouse only ever reaches the session whose buffer is live, so that is
    // the pane the pixels are relative to.
    let (rect, _) = terminal_rect(editor, renderers, editor.buffer.id);
    let col = ((x - rect.x).max(0) / cell_w.max(1)) as usize;
    let row = ((y - rect.y).max(0) / line_h.max(1)) as usize;
    (col, row)
}

/// The pane buffer `id` occupies and the renderer drawing it, or the focused
/// frame's whole content area when it is not on screen anywhere.
///
/// Every frame is searched, not just the focused one: with sessions in the
/// plural, an agent running in another OS window still has to be sized to the
/// pane it is actually in.
fn terminal_rect<'a>(
    editor: &Editor,
    renderers: &'a [Renderer],
    id: zemacs_core::BufferId,
) -> (Rect, Option<&'a Renderer>) {
    for (i, renderer) in renderers.iter().enumerate() {
        let Some(frame) = editor.frames.get(i) else {
            continue;
        };
        let area = renderer.content_area();
        let found = frame
            .panes(area)
            .into_iter()
            .find(|p| frame.window(p.window).map(|w| w.buffer) == Some(id))
            .map(|p| p.rect);
        if let Some(rect) = found {
            return (rect, Some(renderer));
        }
    }
    let index = editor.focus_frame.min(renderers.len().saturating_sub(1));
    match renderers.get(index) {
        Some(renderer) => (renderer.content_area(), Some(renderer)),
        None => (Rect { x: 0, y: 0, w: 0, h: 0 }, None),
    }
}

/// Columns and rows for the child, taken from the pane its buffer is shown in.
///
/// The pane rather than the window: with a split, sizing the child to the whole
/// frame means half its output is drawn outside the pane and `clear` leaves
/// debris. Falls back to the focused frame's content area when the session is
/// not on screen anywhere, so a background agent keeps a sane size rather than
/// being wrapped to nothing.
fn terminal_size(
    editor: &Editor,
    renderers: &[Renderer],
    id: zemacs_core::BufferId,
) -> (usize, usize) {
    let (rect, renderer) = terminal_rect(editor, renderers, id);
    let Some(renderer) = renderer else {
        return (80, 24);
    };
    let (cell_w, line_h) = renderer.cell_size();

    // One line goes to the modeline, and the padding is the renderer's.
    let cols = (rect.w / cell_w.max(1)).max(1) as usize;
    let rows = ((rect.h / line_h.max(1)) - 1).max(1) as usize;
    (cols, rows)
}

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
    // Command is Meta, and *only* Command — the macOS Emacs convention, and
    // unlike Option it does not compose text, so `⌘-` needs no special handling.
    //
    // Option is deliberately absent: the config sets `mac-option-modifier 'none`
    // because Option is how you type –, ≠ and ü. Claiming it as a Meta fallback
    // meant `M--` fired *and* the composed – had to be thrown away to stop it
    // being inserted twice over, so the key was worth one duplicate binding and
    // cost the whole compose layer. With it dropped, ⌥- reaches `TextInput` as –
    // wherever text is being typed, and is simply not a modifier anywhere else.
    //
    // ponytail: not configurable — one line, and nobody has asked for the
    // Linux-style `Alt is Meta`. The knob, if it is ever wanted, is a bool set
    // from Lisp and read here, next to the existing `Set*` commands.
    let meta = keymod.intersects(Mod::LGUIMOD | Mod::RGUIMOD);
    // Enter has a multi-character key name, so `combo_char` cannot spell it —
    // but `C-<ret>` and `C-M-<ret>` are the window splits.
    if matches!(kc, Keycode::Return | Keycode::KpEnter) {
        match (ctrl, meta) {
            (true, true) => return Some(Key::CtrlMetaEnter),
            (true, false) => return Some(Key::CtrlEnter),
            _ => {}
        }
    }
    // Same reason, for `⌘⌫` — kill the word before point.
    if kc == Keycode::Backspace && meta {
        return Some(Key::MetaBackspace);
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

    /// The whole reason for mangling instead of using the basename: two
    /// `mod.rs` open at once must not auto-save over each other.
    #[test]
    fn auto_save_names_are_per_path() {
        let Some(a) = autosave_file(Path::new("/a/src/mod.rs")) else {
            return; // no HOME, nothing to name it under
        };
        let b = autosave_file(Path::new("/b/src/mod.rs")).expect("HOME is set");
        assert_ne!(a, b);
        assert!(a.ends_with("#!a!src!mod.rs#"));
    }

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
    fn command_is_the_only_meta() {
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
        // The bindings people actually press: `M-x` and `M-o`, from ⌘.
        assert_eq!(
            key_from_keydown(Keycode::X, Mod::LGUIMOD, false),
            Some(Key::Meta('x'))
        );
        assert_eq!(
            key_from_keydown(Keycode::O, Mod::RGUIMOD, false),
            Some(Key::Meta('o'))
        );
        // Ctrl+Command is its own key, `C-M-`, not either one alone.
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
    }

    /// `mac-option-modifier 'none`: Option composes characters and must not be
    /// read as Meta, or ⌥- fires `M--` instead of typing –.
    #[test]
    fn option_is_not_a_modifier() {
        // Where text is being typed (Insert, a prompt) Option produces *nothing*
        // from the keydown, so the `TextInput` macOS composed — – for ⌥-, ≠ for
        // ⌥=, ü for ⌥u then u — is the only thing that reaches the buffer.
        for kc in [Keycode::Minus, Keycode::Equals, Keycode::U, Keycode::N] {
            assert_eq!(key_from_keydown(kc, Mod::LALTMOD, false), None, "{kc:?}");
            assert_eq!(key_from_keydown(kc, Mod::RALTMOD, false), None, "{kc:?}");
        }
        // Shift with it is the same story: ⌥⇧= is ±, not `M-+`.
        assert_eq!(
            key_from_keydown(Keycode::Equals, Mod::LALTMOD | Mod::LSHIFTMOD, false),
            None
        );
        // In a modal mode there is no text input to compose into, so Option is
        // simply ignored and the key is the key: `⌥j` moves down.
        assert_eq!(
            key_from_keydown(Keycode::J, Mod::LALTMOD, true),
            Some(Key::Char('j'))
        );
        // Ctrl still wins on its own, and Option adds nothing to it — this used
        // to be `C-M-j`.
        assert_eq!(
            key_from_keydown(Keycode::J, Mod::LALTMOD | Mod::RCTRLMOD, false),
            Some(Key::Ctrl('j'))
        );
        // ...and it takes ⌘ to make that `C-M-j` again.
        assert_eq!(
            key_from_keydown(Keycode::J, Mod::LALTMOD | Mod::RCTRLMOD | Mod::LGUIMOD, false),
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

    /// Reaching `~/.config` is half of "browse the whole filesystem", and not
    /// opening every listing on `.git` is the other half.
    #[test]
    fn dotfiles_appear_only_once_you_type_a_dot() {
        let dir = std::env::temp_dir().join("zemacs_hidden_test");
        let _ = std::fs::create_dir_all(dir.join(".config"));
        std::fs::write(dir.join("plain.txt"), "").unwrap();

        let mut ed = Editor::new();
        ed.open_prompt(PromptKind::File);
        let mut last = None;

        ed.prompt.as_mut().unwrap().text = format!("{}/", dir.display());
        refresh_file_completions(&mut ed, &mut last);
        let items = &ed.prompt.as_ref().unwrap().items;
        assert!(items.iter().any(|i| i.ends_with("plain.txt")));
        assert!(!items.iter().any(|i| i.ends_with(".config/")));

        // ...and the dot re-lists the same directory rather than being filtered
        // out of the answer it already had.
        ed.prompt.as_mut().unwrap().text = format!("{}/.", dir.display());
        refresh_file_completions(&mut ed, &mut last);
        let items = &ed.prompt.as_ref().unwrap().items;
        assert!(items.iter().any(|i| i.ends_with(".config/")));
        assert!(items.iter().any(|i| i.ends_with("plain.txt")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- auto-revert ------------------------------------------------------

    /// Push a file's mtime forward so a revert poll has something to notice.
    /// Written rather than slept for: two `fs::write`s in the same millisecond
    /// can share a stamp on a coarse filesystem, and a test that passes because
    /// nothing happened is worse than no test.
    fn touch_forward(path: &Path) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(10);
        file.set_times(std::fs::FileTimes::new().set_modified(later))
            .unwrap();
    }

    fn scratch(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("zemacs_revert_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    /// The one thing auto-revert must never do.
    #[test]
    fn a_modified_buffer_is_not_reverted_out_from_under_you() {
        let path = scratch("unsaved.txt", "one\ntwo\nthree\n");
        let mut ed = Editor::new();
        open_file(&mut ed, &path, &resolve_init_path());
        let mut watch = Revert::default();
        watch.poll(&mut ed); // first sight: records the stamp, reverts nothing

        // typed, not saved
        ed.apply(EditorCommand::InsertText("edited ".into()));
        let mine = ed.buffer.text.to_string();
        assert!(ed.buffer.modified);

        // ...and now the file moves underneath it
        std::fs::write(&path, "ONE\nTWO\nTHREE\n").unwrap();
        touch_forward(&path);
        watch.poll(&mut ed);

        assert_eq!(ed.buffer.text.to_string(), mine, "unsaved edits survive");
        assert!(ed.buffer.modified, "and the buffer is still dirty");
        // silence would be worse than the clobbering: you would find out at `:w`
        assert!(ed.status.contains("changed on disk"), "{}", ed.status);
        let _ = std::fs::remove_file(&path);
    }

    /// The other half: an untouched buffer follows the file, and point stays on
    /// the line it was on rather than at the offset it was at.
    #[test]
    fn an_unmodified_buffer_follows_the_file_and_keeps_point() {
        let path = scratch("external.txt", "alpha\nbeta\ngamma\n");
        let mut ed = Editor::new();
        open_file(&mut ed, &path, &resolve_init_path());
        let mut watch = Revert::default();
        watch.poll(&mut ed);
        ed.buffer.move_to_line_col(2, 1); // on `gamma`

        // Two lines inserted above: an offset would drift, a line number does not.
        std::fs::write(&path, "one\ntwo\nalpha\nbeta\ngamma\n").unwrap();
        touch_forward(&path);
        watch.poll(&mut ed);

        assert_eq!(ed.buffer.text.to_string(), "one\ntwo\nalpha\nbeta\ngamma\n");
        assert_eq!(ed.buffer.cursor_line_col(), (2, 1));
        assert!(!ed.buffer.modified, "reverted text is what is on disk");
        let _ = std::fs::remove_file(&path);
    }

    /// Writing a file must not revert the buffer that wrote it — the mtime moved
    /// but the text did not, and a revert would throw point at the top.
    #[test]
    fn saving_does_not_revert_the_buffer_that_saved() {
        let path = scratch("saved.txt", "alpha\nbeta\ngamma\n");
        let mut ed = Editor::new();
        open_file(&mut ed, &path, &resolve_init_path());
        let mut watch = Revert::default();
        watch.poll(&mut ed);

        ed.buffer.move_to_line_col(2, 0);
        ed.apply(EditorCommand::InsertText("delta ".into()));
        let at = ed.buffer.cursor;
        save_file(&mut ed, None);
        touch_forward(&path); // a save moves the stamp; make sure it is noticed
        watch.poll(&mut ed);

        assert_eq!(ed.buffer.text.to_string(), "alpha\nbeta\ndelta gamma\n");
        assert_eq!(ed.buffer.cursor, at, "point did not move");
        assert!(!ed.status.contains("reverted"), "{}", ed.status);
        let _ = std::fs::remove_file(&path);
    }

    /// The reason this got faster and wider: an agent rewrites files you are
    /// not looking at, and a buffer you have to visit before it catches up is a
    /// buffer that lied to you until you visited it.
    #[test]
    fn a_parked_buffer_follows_its_file_without_being_visited() {
        let one = scratch("parked_one.txt", "alpha\nbeta\n");
        let two = scratch("parked_two.txt", "gamma\ndelta\n");
        let mut ed = Editor::new();
        open_file(&mut ed, &one, &resolve_init_path());
        open_file(&mut ed, &two, &resolve_init_path()); // `one` is now parked
        assert_eq!(ed.buffer.path.as_deref(), Some(two.as_path()));

        let mut watch = Revert::default();
        watch.poll(&mut ed); // first sight of both

        // The agent writes the file nobody is looking at.
        std::fs::write(&one, "ALPHA\nBETA\nGAMMA\n").unwrap();
        touch_forward(&one);
        watch.poll(&mut ed);

        let parked = ed
            .others
            .iter()
            .find(|b| b.path.as_deref() == Some(one.as_path()))
            .expect("still open");
        assert_eq!(parked.text.to_string(), "ALPHA\nBETA\nGAMMA\n");
        assert!(!parked.modified);
        // ...and the live buffer was not touched in the process.
        assert_eq!(ed.buffer.text.to_string(), "gamma\ndelta\n");
        assert!(ed.status.contains("reverted"), "{}", ed.status);
        let _ = std::fs::remove_file(&one);
        let _ = std::fs::remove_file(&two);
    }

    /// The safety property, in the direction that used to be unreachable: an
    /// unsaved *parked* buffer is not clobbered either.
    #[test]
    fn a_modified_parked_buffer_is_left_alone_too() {
        let one = scratch("parked_dirty.txt", "alpha\n");
        let two = scratch("parked_other.txt", "beta\n");
        let mut ed = Editor::new();
        open_file(&mut ed, &one, &resolve_init_path());
        ed.apply(EditorCommand::InsertText("typed ".into()));
        let mine = ed.buffer.text.to_string();
        open_file(&mut ed, &two, &resolve_init_path());

        let mut watch = Revert::default();
        watch.poll(&mut ed);
        std::fs::write(&one, "ONE\n").unwrap();
        touch_forward(&one);
        watch.poll(&mut ed);

        let parked = ed
            .others
            .iter()
            .find(|b| b.path.as_deref() == Some(one.as_path()))
            .expect("still open");
        assert_eq!(parked.text.to_string(), mine, "unsaved edits survive");
        assert!(parked.modified);
        assert!(ed.status.contains("changed on disk"), "{}", ed.status);
        let _ = std::fs::remove_file(&one);
        let _ = std::fs::remove_file(&two);
    }

    /// A long session must not accumulate one map entry per file ever opened.
    #[test]
    fn closed_files_are_forgotten() {
        let path = scratch("forgotten.txt", "x\n");
        let mut ed = Editor::new();
        open_file(&mut ed, &path, &resolve_init_path());
        let mut watch = Revert::default();
        watch.poll(&mut ed);
        assert!(watch.seen.contains_key(&path));

        ed.kill_buffer(0);
        watch.poll(&mut ed);
        assert!(!watch.seen.contains_key(&path));
        let _ = std::fs::remove_file(&path);
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
