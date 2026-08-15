//! zemacs — the application.
//!
//! Owns the SDL2 event loop, the single mutable `Editor`, and one OS window per
//! frame; spawns the Common Lisp image and drains its commands each frame.
//!
//! Command flow, in one place: the keyboard, the mouse and Lisp all produce
//! [`EditorCommand`]s, and every one of them goes through [`App::dispatch`].
//! Most land in `Editor::apply` (the single document writer); three are
//! *effects* the pure core cannot perform — evaluating Lisp, reading a file,
//! writing a file — and this layer performs them.
//!
//! The invariant worth stating out loud: **`renderers[i]` draws
//! `editor.frames[i]`**. Frames appear from the core (`M-x new-frame`) and the
//! loop opens a window for each one it has not got a window for yet; frames
//! disappear only through [`close_frame`], which drops the matching renderer in
//! the same breath. Everything else — event routing, focus — is an index into
//! both vectors at once, so nothing may remove from one alone.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use sdl3::event::{Event, WindowEvent};
use sdl3::keyboard::{Keycode, Mod};
use sdl3::mouse::{Cursor, MouseButton, MouseWheelDirection, SystemCursor};

use zemacs_core::frame::{Divider, Split};
use zemacs_core::{
    BufferId, BufferKind, Change, Editor, EditorCommand, Frame, Key, PromptKind, Rect, WindowId,
};
use zemacs_lisp::Lisp;
use zemacs_render::Renderer;
use zemacs_tramp as tramp;

mod control;
mod dired;
#[cfg(target_os = "macos")]
mod dock;
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
    /// The press landed in a terminal and the child did not take it, so this
    /// gesture is dragging out a text selection rather than being reported to
    /// whatever is running. Latched at the press: a drag has to keep meaning
    /// what it meant when it started, even if the child turns mouse reporting
    /// on halfway through it.
    selecting: bool,
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

    /// Every gesture ends here, including the ones that ended somewhere the
    /// press did not start — a drag out of a terminal and into another pane
    /// would otherwise leave `selecting` latched for the next press.
    fn release(&mut self) -> bool {
        self.drag = None;
        std::mem::take(&mut self.selecting)
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
/// SDL's monotonic clock in nanoseconds — the unit `Event::timestamp` is in.
///
/// `sdl3::timer::ticks()` answers milliseconds, so it is deliberately not used
/// anywhere in this file: one clock, one unit, no arithmetic between the two.
fn now_ns() -> u64 {
    unsafe { sdl3::sys::timer::SDL_GetTicksNS() }
}

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

/// Hints that have to be set *before* `sdl3::init`, because SDL reads them
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
    sdl3::hint::set("SDL_VIDEO_MAC_FULLSCREEN_SPACES", "1");
    // `SDL_MAC_BACKGROUND_APP`, not `SDL_HINT_MAC_BACKGROUND_APP`: the latter is
    // the *name of the C macro*, and SDL reads the string it expands to. Setting
    // the macro's name sets a hint nothing has ever asked for.
    sdl3::hint::set("SDL_MAC_BACKGROUND_APP", "0");
    // Ctrl-click is a right click on this platform, and taking it would make
    // the trackpad's own secondary click unreachable.
    sdl3::hint::set("SDL_MAC_CTRL_CLICK_EMULATE_RIGHT_CLICK", "1");
}

/// Where focus lands once the frame at `closed` is removed from `before`
/// frames: still on the frame the user was using, shifted down by one if it sat
/// after the hole, and clamped when the focused frame is the one that went.
fn focus_after_close(focus: usize, closed: usize, before: usize) -> usize {
    let last = before.saturating_sub(2); // highest index that survives
    if focus > closed { focus - 1 } else { focus }.min(last)
}

/// The `after-edit-hook` call for whatever has happened to the live buffer
/// since the image was last told, or `None` when the answer is "nothing".
///
/// `(after-edit-hook START OLD-END NEW-END TEXT)` — character offsets, and
/// `TEXT` is what now occupies `START..NEW-END`. Replaying it against any copy
/// of the document is "replace `START..OLD-END` with `TEXT`", which is
/// `textDocument/didChange` with a range, and `parinfer`'s smart mode, and
/// anything else that keeps a shadow of the buffer.
///
/// **The text is sliced here, and that is the whole answer to the threading
/// gap.** Lisp is not synchronous with the command loop: this form goes onto
/// the image's queue and is evaluated a turn or more later, by which time the
/// buffer will happily have moved on. A hook that was handed three offsets and
/// told to read the text back would read a *different* document — offsets that
/// were exact when the record was made and are nonsense two keystrokes later —
/// and a language server fed that would desynchronise permanently and silently.
/// So the record is made self-contained under the same lock that produced it,
/// and nothing downstream ever has to look at the live buffer to interpret it.
/// See `docs/threading.org`; this is the same rule `replace-region` follows,
/// applied in the other direction.
///
/// `OLD-END` is `nil` when the app cannot say what the reader had — a buffer
/// switch, a document replaced wholesale, or a reader further behind than
/// core's log reaches. `TEXT` is then the whole buffer, and the meaning is
/// "replace everything you have", which is exactly the full-text `didChange`
/// the LSP client sends on every keystroke today. So the incremental case is
/// the new one and the resynchronising case is the status quo.
///
/// One thing a consumer still owes itself, because no delta shape can supply
/// it: `OLD-END` is an offset into the document *before* the edit, and LSP
/// wants it as a line and a character. The buffer no longer holds that text, so
/// a ranged `didChange` needs the shadow copy of each open file that the client
/// is already building every keystroke — kept, and updated by applying this
/// record to it, rather than rebuilt. Emacs' eglot solves the same problem the
/// other way, with a *before*-change hook, which would be the alternative here
/// and costs a second signal per keystroke to save one string per open file.
fn after_edit_form(editor: &Editor, told: &mut Option<(BufferId, u64)>) -> Option<String> {
    let buffer = editor.buffer.id;
    let log = told
        .filter(|(b, _)| *b == buffer)
        .and_then(|(_, seen)| editor.buffer.changes_since(seen));
    *told = Some((buffer, editor.buffer.change_count()));
    let (change, old_end) = match log {
        // The revision moved and the text did not: `set-language`, a mode
        // change, a scene. There is no edit, so there is nothing to say — and
        // saying it anyway would cost a `didChange` per minor-mode toggle.
        Some([]) => return None,
        Some(log) => {
            let change = Change::coalesce(log)?;
            (change, change.old_end.to_string())
        }
        None => (
            Change { start: 0, old_end: 0, new_end: editor.buffer.len_chars() },
            "nil".to_string(),
        ),
    };
    let text = editor.buffer.slice_string(change.start, change.new_end);
    Some(lisp_call(
        "AFTER-EDIT-HOOK",
        &format!(
            "{} {old_end} {} {}",
            change.start,
            change.new_end,
            zemacs_rpc::lisp::string(&text),
        ),
    ))
}

/// Queue `point-moved-hook` if point is somewhere it has not been reported
/// from, and remember where that is.
///
/// The other signal the image gets about a buffer, and the twin of
/// [`after_edit_form`]: the *cursor* moved. Without it a config only ever hears
/// about a buffer when the document changes, which is the wrong half for
/// anything whose job is to react to where you are looking — org's markup would
/// reveal itself as you typed and stay hidden as you navigated, because `j` and
/// `w` change no text at all.
///
/// Queued rather than called, through the same `pending_hooks` and the same
/// `fboundp` guard, so it costs nothing until something defines
/// `point-moved-hook` — `runtime/modes/modes.lisp` does. It is appended to
/// whatever core already put there, so a switch reaches a config as
/// `buffer-switch-hook` then `point-moved-hook`: the mode's global settings are
/// re-resolved before anything re-renders against them, which is the order a
/// config that has both wants and the one it has always seen.
///
/// **A buffer and an offset, because an offset alone is not a position.** The
/// question being asked is "is the text under point the text that was under
/// point", and `buffer.cursor` on its own only answers it for a switch that
/// happens to land somewhere else. Offset 0 is where two freshly opened files
/// both sit, and where a jump out to a generated buffer and back leaves you, and
/// every one of those switches used to be silent — the equation preview,
/// org-appear and show-paren went on rendering the buffer you had left.
///
/// A generated buffer is excluded rather than merely skipped, and `last` is
/// deliberately left standing when it is: a terminal rewrites itself as fast as
/// the shell prints and no config can act on it anyway, and holding the position
/// keeps the return trip honest — coming back to the same offset in the same
/// file after a detour through one really is no move at all.
///
/// ponytail: the previous position is remembered here but never *carried* to the
/// image, so a config that wants to act on the range you left — re-rendering the
/// equation you just stepped out of, say — still has to remember it in the
/// image. The upgrade is what [`after_edit_form`] already does for the document:
/// build the call here, where the lock is held, rather than queueing a name.
fn queue_point_moved(editor: &mut Editor, last: &mut Option<(BufferId, usize)>) {
    let now = (editor.buffer.id, editor.buffer.cursor);
    if *last != Some(now) && !editor.buffer.kind.is_generated() {
        *last = Some(now);
        editor.pending_hooks.push("point-moved-hook".into());
    }
}

/// The one shape every signal this layer sends the image takes: look the symbol
/// up in `ZEMACS`, and call it only if a config actually defined it.
///
/// Five call sites wrote this out by hand, which is five chances to drop the
/// `fboundp` — and dropping it turns a config that never loaded `runtime/rpc.lisp`
/// into an "undefined function" per message rather than into silence, which is
/// the behaviour every one of the five wanted and none of them stated. `args` is
/// already spelled as Lisp, because the only thing that varies between the five
/// is how their arguments are spelled and two of them have none.
///
/// A `String` rather than a send, because [`after_edit_form`] has to build its
/// call under the lock and hand it over — the delta is only meaningful sliced
/// from the document that produced it. [`App::call_lisp`] is the other four.
fn lisp_call(name: &str, args: &str) -> String {
    format!(
        "(let ((h (find-symbol {} :zemacs))) (when (and h (fboundp h)) (funcall h {args})))",
        zemacs_rpc::lisp::string(name)
    )
}

/// What the command line asked for.
///
/// Parsed by hand. Two flags and an optional file is not a case for a CLI
/// crate: `clap` is a build-time dependency, a derive macro and a help format
/// to keep in step, and what it would buy here is a `match` on three strings.
#[derive(Default)]
struct Args {
    /// Protocol on stdin/stdout, and no window unless `show` says otherwise.
    control: bool,
    /// With `control`, open a real window as well — for watching an automated
    /// session happen, which is the one thing the plain-text `screen` op and a
    /// PNG cannot give you.
    show: bool,
    file: Option<PathBuf>,
}

fn args() -> Args {
    let mut args = Args::default();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--control" => args.control = true,
            "--show" => args.show = true,
            // The first non-flag is the file to open, exactly as it was when
            // the whole of this was `args().nth(1)`.
            _ if args.file.is_none() => args.file = Some(PathBuf::from(arg)),
            _ => {}
        }
    }
    args
}

fn main() -> anyhow::Result<()> {
    let args = args();
    // **The first statement, and it has to be.** SDL3 does not read `environ`
    // when it wants a hint; it reads a *copy* it takes the first time anything
    // asks for one — and `mac_window_hints` below is exactly such a call. A
    // `set_var` after it is set in this process and invisible to SDL, which
    // shows up as `--control` cheerfully opening a window on your screen.
    // Belt and braces: the hint is set too, because that path is immune to the
    // ordering entirely and the two names are different vintages of SDL.
    //
    // Everything downstream — the window, the renderer, the pane arithmetic,
    // `term.sync`, the present — runs exactly as it does with a display
    // attached; the dummy driver just has nowhere to put the pixels. That is
    // what makes this a headless *editor* rather than a stub of one.
    if args.control && !args.show {
        std::env::set_var("SDL_VIDEODRIVER", "dummy");
        sdl3::hint::set("SDL_VIDEO_DRIVER", "dummy");
    }
    start_in_home();
    inherit_login_path();
    mac_window_hints();
    let sdl = sdl3::init().map_err(|e| anyhow::anyhow!("SDL init: {e}"))?;
    // One renderer per frame, in frame order. See the module docs.
    let mut renderers = vec![Renderer::new(&sdl, "zemacs", WINDOW_W, WINDOW_H)?];
    // After the first renderer, because that is what brings the video subsystem
    // up, and the delegate the dock menu attaches to does not exist before it.
    #[cfg(target_os = "macos")]
    dock::install();
    // Ask for the keyboard, exactly as a new frame does. macOS hands focus to a
    // window the application opened itself only when the application is already
    // the active one — so `zemacs` typed at a shell put a window on screen and
    // left the keystrokes going to whatever was in front, which reads as the
    // editor ignoring every key you press.
    renderers[0].focus();
    let mut pump = sdl
        .event_pump()
        .map_err(|e| anyhow::anyhow!("SDL event pump: {e}"))?;
    // Only the perf report reads the clock, but it has to be SDL's own: an
    // event's timestamp is in it, and the interesting part of a keystroke's life
    // is over before this loop ever sees the event.
    //
    // SDL3 retired the timer *subsystem* and moved the clock to free functions —
    // and moved event timestamps to **nanoseconds** while `ticks()` stayed in
    // milliseconds. Subtracting one from the other is the kind of mistake that
    // compiles and then reports nonsense forever, so `now_ns` is the only clock
    // this file reads and it is the one events are stamped in.
    let video = sdl.video().map_err(|e| anyhow::anyhow!("SDL video: {e}"))?;

    // Both before the image starts, and in this order. The image reads
    // `$ZEMACS_RUNTIME` while it boots — that is how `init.lisp` finds
    // `library.lisp` — and `seed_user_init` reads the same path to find the
    // default it copies, so the variable has to be set first.
    std::env::set_var("ZEMACS_RUNTIME", runtime_dir());
    seed_user_init();

    let init_path = resolve_init_path();
    let (tx, rx): (Sender<EditorCommand>, Receiver<EditorCommand>) =
        crossbeam_channel::unbounded();

    // The editor is shared with the Lisp image, which reads and edits it
    // directly rather than only being able to shout commands at it. Seeded
    // before `spawn` so `init.lisp` cannot observe a half-built dashboard.
    let shared: zemacs_core::Shared = Default::default();
    // Built out here for one reason: `zemacs /ssh:host:/etc/nginx.conf` is a
    // `find-file` like any other, and the request it queues has to go to the
    // worker the loop will poll rather than to one dropped on the next line.
    let mut remote = Remote::default();
    {
        let mut editor = shared.lock().expect("fresh mutex");
        seed_dashboard(&mut editor, &init_path, &renderers[0].backend());
        // Any file named on the command line opens instead of the dashboard.
        if let Some(path) = &args.file {
            open_file(&mut editor, path, &init_path, &mut remote);
        }
    }
    let lisp = zemacs_lisp::spawn(tx, shared.clone(), init_path.clone());

    // Immediately after `spawn` and before the loop: the probe it queues has to
    // be the first thing in the image's request channel, or a client's own
    // `eval` could be answered before the config it depends on has loaded.
    let mut control = args
        .control
        .then(|| control::Control::start(&sdl, &lisp, init_path.clone()))
        .transpose()?;

    // Thread three: highlighting. The main thread owns input and drawing, the
    // Lisp thread owns the image, and neither ever waits on a parse.
    let highlighter = zemacs_syntax::spawn_worker();

    // Everything else the loop needs, in one place — see [`App`]. The `sdl`
    // handle goes in with it, because `pump`, `timer` and `video` above are all
    // it was wanted for out here and the one remaining caller — opening a window
    // for a frame core pushed — belongs to the app rather than to this function.
    let mut app = App::new(sdl, &video, renderers, lisp, init_path, remote);
    let mut batch = Batch::default();
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
        //
        // A queued control request is the third reason not to park, beside a
        // frame that has already been paced. The request woke us with a pushed
        // SDL event, so the *first* of a burst always arrives at once; this is
        // what keeps the second through tenth from each costing a wakeup.
        let idle = Instant::now();
        let queued = control.as_ref().is_some_and(control::Control::pending);
        let waited = (!presented && !queued)
            .then(|| {
                pump.wait_event_timeout_ms(app.renderers.first().map_or(16, Renderer::frame_ms))
            })
            .flatten();
        let frame_start = Instant::now();
        perf.idle += frame_start - idle;

        // Held for this whole iteration except the present at the bottom, which
        // is where the frame's idle time actually is. A Lisp primitive waits at
        // most for one iteration's worth of input handling and drawing, never
        // for the display — that is what keeps a slow config from being felt.
        //
        // Every `app.` call below takes `&mut editor`, and that is the whole
        // statement of the discipline: each one is a phase of *this* iteration,
        // run under *this* lock, in this order. The only one that does not is
        // `present`, below the `drop`.
        let mut editor = shared.lock().unwrap_or_else(|e| e.into_inner());
        batch.start();

        // The event the wait above returned is the first of the batch — dropping
        // it would lose exactly the keystroke that woke us.
        for event in waited.into_iter().chain(pump.poll_iter()) {
            if app.route_event(&mut editor, event, &mut batch).is_break() {
                break 'main;
            }
        }

        if let Some(i) = batch.closing {
            app.mouse.release(); // whatever was being dragged may be going away
            close_frame(&mut editor, &mut app.renderers, i);
        }

        // The dock menu's **New Frame**. It cannot reach the editor from the
        // AppKit callback — see `dock` — so it raises a flag and this is where
        // the flag is spent, on the same command `M-x new-frame` sends.
        #[cfg(target_os = "macos")]
        if dock::wanted() {
            app.dispatch(&mut editor, EditorCommand::NewFrame);
        }

        // Above the key drain, and that is the whole of why `keys` is honest: a
        // request that feeds keystrokes puts them in `batch.keys` and they go
        // through `handle_key` and `dispatch` on the next four lines, exactly
        // as the ones SDL produced do.
        if let Some(c) = &mut control {
            if !c.poll(&mut app, &mut editor, &mut batch) {
                break 'main;
            }
        }

        for key in batch.keys.drain(..) {
            for cmd in editor.handle_key(key) {
                app.dispatch(&mut editor, cmd);
            }
        }
        while let Ok(cmd) = rx.try_recv() {
            app.dispatch(&mut editor, cmd);
        }

        app.sync_text_input(&editor);

        // The last window going away is a quit even if core has not said so.
        if editor.should_quit || app.renderers.is_empty() {
            break 'main;
        }

        app.tell_readers(&mut editor, &highlighter);
        app.housekeep(&mut editor, &highlighter)?;

        // Park the live cursor and scroll on the focused window, once, so every
        // pane in every frame can be drawn from its own `Window`.
        editor.sync_focused_window();
        // One grid per terminal buffer on screen, not one for "the" terminal:
        // see `Term::screens`. Gathered after `sync_focused_window`, so a
        // session that just became visible is measured against the window it is
        // actually in.
        let screens = app.term.screens(&editor);
        perf.input += frame_start.elapsed();

        // Whether to draw at all. `Editor::generation` moves on every keystroke,
        // every command, every Lisp primitive and every writer in this layer
        // that reaches around them; if it has not moved, nothing on screen can
        // have changed and the frame is two milliseconds spent proving it.
        //
        // A screenshot request draws regardless: `shoot` reads back the frame
        // buffer below, and reading back one we declined to fill would hand out
        // whatever was in it.
        //
        // ...and so does a frame that has not been drawn for `DRAW_AT_LEAST`,
        // which is the safety net rather than a clock. `generation` is a list of
        // writers rather than a property of the editor, so a writer added later
        // that forgets to touch costs a fraction of a second of staleness — a
        // lag somebody notices and reports — instead of a pane that silently
        // stops updating, which is the failure nobody can describe.
        let due = app.last_draw.elapsed() >= DRAW_AT_LEAST;
        let shooting = control.as_ref().is_some_and(control::Control::shooting);
        let drew = editor.generation != app.drawn_generation || due || shooting;

        let drawing = Instant::now();
        let draws = match drew {
            false => 0,
            true => {
                let n = app.draw(&mut editor, &screens)?;
                // *After* the draw, not before: the renderer parks each pane's
                // width and height back on the editor and `scroll_scene` may
                // fix up a scene's offset, so a generation read before the draw
                // would differ from the one after it and every frame would
                // redraw its own bookkeeping for ever.
                app.drawn_generation = editor.generation;
                app.last_draw = Instant::now();
                n
            }
        };
        perf.draw += drawing.elapsed();

        // Between the draw and the present, because that is the one moment the
        // frame being asked for exists in a buffer that can be read back — see
        // `Renderer::save_png`, which presents on the caller's behalf for
        // exactly this reason.
        if let Some(c) = &mut control {
            c.shoot(&mut app, &editor);
        }

        // Drawing is done; the editor is nobody's until the next iteration.
        // Presenting parks this thread until the next vertical blank, which is
        // most of the frame, and holding the lock across it would put every
        // Lisp primitive behind the display.
        drop(editor);
        let presenting = Instant::now();
        // Nothing was drawn, so there is nothing new to show. Skipped rather
        // than left to `present`'s own digest test, which would answer the same
        // and flush an empty command list per renderer to do it.
        let presents = if drew { app.present() } else { 0 };
        presented = presents > 0;
        perf.present += presenting.elapsed();
        // A keystroke that changed nothing on screen — `k` at the top of the
        // buffer — has no latency to report, only a frame that decided not to
        // happen.
        if let Some(stamped) = batch.typed.filter(|_| presented) {
            perf.key((now_ns().saturating_sub(stamped) / 1_000_000) as u32);
        }
        perf.frame(frame_start, presents, draws);
    }
    // Language servers are children of this process and outlive it otherwise —
    // one stray `clangd` indexing a repository per session, which is the kind of
    // thing you only notice when the fan starts.
    zemacs_rpc::stop_all();
    // And so does a `:!`, for a different reason: it is not a PTY child, so the
    // `SIGHUP` the note below relies on never reaches it. By name rather than by
    // `Drop`, because `_exit` runs no destructors.
    app.term.hangup();
    // Every way out of the loop, not just the `quit` op: a client waiting on
    // this also hears about the window being closed and about the editor
    // quitting itself.
    if let Some(c) = &mut control {
        c.exit(0);
    }
    // Everything the protocol promised is on the wire before the process is
    // taken down without running a single `atexit` handler.
    {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
    }
    // `_exit` and not `exit`, and not a plain `return`.
    //
    // This process embeds two runtimes that outlive `main`: ECL, with a GC
    // marker pool and an asynchronous signal servicer, and AppKit, with its
    // event thread. Returning normally left the process alive with its own main
    // thread already gone — every response written, the exit event flushed, and
    // nothing to reap it but `SIGKILL`, which is also the only signal it would
    // take. `std::process::exit` did not help either: it runs `atexit`, and that
    // is where the wait actually is. Only skipping the handlers ends it.
    //
    // Reproducible, and worth keeping reproducible: take a screenshot, then
    // quit. Without a readback the process happens to come down on its own,
    // which is why this survived a suite that never asked for a picture. Under
    // `--control` the cost was a leaked editor per session.
    //
    // Nothing is lost by skipping the handlers, but only because the two things
    // that genuinely outlive this process are taken by name above:
    // `zemacs_rpc::stop_all` for the language servers, `Term::hangup` for an
    // outstanding `:!`. A PTY's child needs neither — it gets its `SIGHUP` from
    // the kernel. Anything else that comes to own a process must be added there
    // too; a `Drop` will not run.
    unsafe { libc::_exit(0) }
}

// --- the app ---------------------------------------------------------------

/// Everything the loop carries from one turn to the next, and everything a
/// command needs in order to be carried out.
///
/// These were nineteen locals in `main`, seven of which `dispatch` re-listed at
/// every one of its fifteen call sites — the same bindings in the same order
/// every time, wrapped across three lines because they did not fit on one. They
/// share a lifetime (built before the loop, dropped when it ends) and they are
/// the *app's* half of the editor, the half the pure core cannot reach: a Lisp
/// image, a window per frame, a git working tree, a directory, a PTY, a project,
/// the clipboard. So they are one struct — and with a name to hang them on,
/// each phase of the loop becomes a method here rather than six hundred lines
/// inline.
///
/// **Every method below is called with the editor lock held**, which is what the
/// `&mut Editor` in each signature says out loud: `main` takes the lock once at
/// the top of an iteration and drops it before presenting, so nothing in here
/// has to think about where the lock is. [`App::present`] is the exception and
/// takes no editor at all, because by the time it runs there is none to take.
/// `docs/threading.org` is the contract; the comments in `main` are where the
/// lock is taken and where it goes.
struct App {
    /// Kept for [`App::housekeep`] alone: a frame core pushed needs an OS
    /// window, and opening one needs the video subsystem back.
    sdl: sdl3::Sdl,
    lisp: Lisp,
    init_path: PathBuf,
    /// One renderer per frame, in frame order. See the module docs.
    renderers: Vec<Renderer>,
    magit: Magit,
    dired: Dired,
    term: Term,
    project: Project,
    mouse: Mouse,
    /// The gutter row [`App::hover`] last resolved — `(frame, window, row)` —
    /// and whether it left a box up. Purely a cache: see `hover`, which is where
    /// both halves are argued for. Beside `mouse` because it is the same kind of
    /// thing, a fact about the pointer that outlives one event.
    hovered: (Option<(usize, zemacs_core::frame::WindowId, usize)>, bool),
    cursors: Option<Cursors>,
    clipboard: Clipboard,
    /// Text input is toggled per mode — see [`wants_text_input`]. It is off to
    /// begin with because the editor opens on the dashboard.
    text_input: sdl3::keyboard::TextInputUtil,
    text_input_on: bool,
    last_revision: u64,
    /// How much of the live buffer's change log each reader has already acted
    /// on, as `(buffer, count)`. Two watermarks and not one, because the two
    /// readers are fed on different conditions — the parser only for a buffer
    /// with a language — and a shared one would hand whichever of them had been
    /// skipped a list of edits starting after text it never saw. `None`, and a
    /// buffer that does not match, both mean "start again".
    told_lisp: Option<(BufferId, u64)>,
    told_syntax: Option<(BufferId, u64)>,
    /// Where point was when the image was last told, as `(buffer, offset)`.
    /// The pair and not the offset alone, for the same reason the two above are
    /// pairs: an offset means nothing without the document it indexes into, and
    /// two buffers sit at the same one far too often to treat "unchanged" as
    /// "unmoved". `None` is "never told", so the first pass through the loop
    /// always reports — which is what makes a config see the cursor it started
    /// next to. See [`queue_point_moved`].
    last_point: Option<(BufferId, usize)>,
    last_file_query: Option<String>,
    last_grep: Option<String>,
    last_autosave: Instant,
    last_revert: Instant,
    /// [`Editor::generation`] as it stood when the last frame was drawn. A frame
    /// whose generation matches this one is a frame nothing could have changed
    /// in, and is skipped whole. `MAX` so the first one always draws.
    drawn_generation: u64,
    /// When that was, for the safety net beside it — see [`DRAW_AT_LEAST`].
    last_draw: Instant,
    revert_watch: Revert,
    /// Reads, writes and listings on other machines. Inert — no thread, no
    /// socket — until the first `/ssh:` name of the session.
    remote: Remote,
}

/// What one turn of the event pump produced.
///
/// Gathered rather than acted on event by event, because all three answers are
/// about the batch as a whole: the keys reach core in order once routing is
/// done, the close is deferred so the rest of the batch keeps addressing the
/// frames it was routed against, and the timestamp is the earliest of them.
#[derive(Default)]
struct Batch {
    /// There used to be a `swallow_text` flag beside this: an Alt keydown armed
    /// it and the `TextInput` macOS composed from the combo (⌥- is –, ⌥= is ≠)
    /// was dropped, because Option was a Meta fallback and `M--` had already
    /// been dispatched. The config sets `mac-option-modifier 'none`, so that was
    /// backwards — the composed character *is* the thing being asked for. Option
    /// is no longer a modifier here (see `key_from_keydown`) and the text it
    /// produces is now inserted like any other.
    keys: Vec<Key>,
    /// At most one window closes per iteration: a close shifts every later frame
    /// index down, and the rest of this batch was routed against the old ones.
    /// Deferring it keeps the whole batch consistent.
    closing: Option<usize>,
    /// When the earliest keystroke of this batch was stamped, for the perf
    /// report to subtract from the present at the bottom.
    typed: Option<u64>,
}

impl Batch {
    /// Empty, for a new iteration. `keys` is cleared rather than replaced, so
    /// the one allocation lasts the session.
    fn start(&mut self) {
        self.keys.clear();
        self.closing = None;
        self.typed = None;
    }

    /// A keystroke or a composed character arrived, stamped on SDL's own clock.
    /// The earliest wins: what the perf report wants is how long the *first* key
    /// of the batch waited, which is the one that waited longest.
    fn stamp(&mut self, timestamp: u64) {
        self.typed = Some(self.typed.map_or(timestamp, |t| t.min(timestamp)));
    }
}

impl App {
    fn new(
        sdl: sdl3::Sdl,
        video: &sdl3::VideoSubsystem,
        renderers: Vec<Renderer>,
        lisp: Lisp,
        init_path: PathBuf,
        remote: Remote,
    ) -> Self {
        // Not stopped here: SDL3 wants a window to stop it *on*, and the windows
        // are about to be moved into `self`. The first `sync_text_input` of the
        // loop does it, from `text_input_on: false` against a dashboard that
        // wants no text input — which is the state this call was asserting.
        let text_input = video.text_input();
        Self {
            sdl,
            lisp,
            init_path,
            renderers,
            magit: Magit::default(),
            dired: Dired::default(),
            term: Term::default(),
            project: Project::default(),
            mouse: Mouse::default(),
            hovered: Default::default(),
            cursors: Cursors::new(),
            clipboard: Clipboard::new(video),
            text_input,
            text_input_on: false,
            last_revision: u64::MAX,
            told_lisp: None,
            told_syntax: None,
            last_point: None,
            last_file_query: None,
            last_grep: None,
            last_autosave: Instant::now(),
            last_revert: Instant::now(),
            drawn_generation: u64::MAX,
            last_draw: Instant::now(),
            revert_watch: Revert::default(),
            remote,
        }
    }

    /// Which frame an SDL event belongs to. `None` for a window that has already
    /// been closed but still has events queued behind it.
    fn frame_for(&self, window_id: u32) -> Option<usize> {
        frame_for_window(self.renderers.iter().map(Renderer::window_id), window_id)
    }

    /// The same, for an event that also carries a position: the frame, and where
    /// in it *in pixels*. One call rather than two because every mouse arm below
    /// wanted exactly this pair and none of them wanted one without the other —
    /// SDL reports points, and everything past here counts pixels.
    fn pointer(&self, window_id: u32, x: i32, y: i32) -> Option<(usize, i32, i32)> {
        let i = self.frame_for(window_id)?;
        let (x, y) = self.renderers[i].to_pixels(x, y);
        Some((i, x, y))
    }

    /// What the pointer at `(x, y)` in frame `i` is resting on, into
    /// [`Editor::tooltip`] — a diagnostic's message when it is a mark in the
    /// gutter, and nothing at all anywhere else.
    ///
    /// **This runs once per pixel of pointer movement**, which is the whole
    /// shape of it. Resolving a pixel to a buffer line means walking the visible
    /// lines through folds, wraps and image rows — [`Renderer::click_target`],
    /// the drag path's own arithmetic — and doing that per pixel would allocate
    /// a cell vector per line per pixel. So the work is behind
    /// [`Renderer::gutter_row`], which answers in a pane lookup, one compare and
    /// one division, and the expensive half runs **once per row crossed**: a
    /// gutter mark is a row tall, so an answer cannot change inside one.
    ///
    /// `hovered` remembers what that cheap question last answered *and whether a
    /// box was up when it did*, and the second half is not decoration. Anything
    /// else may take the box down — a keystroke does, in `handle_key` — and
    /// without noticing that, the pointer sitting inside one row after a
    /// keystroke would be stuck: the cheap answer matches, the work is skipped,
    /// and the box only returns when you cross into the next row.
    fn hover(&mut self, editor: &mut Editor, i: usize, x: i32, y: i32) {
        let hit = self.renderers[i].gutter_row(editor, i, x, y);
        let seen = (hit.map(|(w, row)| (i, w, row)), editor.tooltip.is_some());
        if seen == self.hovered {
            return;
        }
        // Through `click_target` rather than a second walk of its own: it is the
        // draw loop's twin, it already lands on column zero of the row for a
        // pointer in the gutter, and a hover box that disagreed with a click
        // about which line you are on would be worse than no box.
        let text = hit
            .and_then(|_| self.renderers[i].click_target(editor, i, x, y))
            .and_then(|(_, at)| editor.help_echo_at(at))
            .map(str::to_owned);
        // The pointer's position when the box *appeared*, not wherever it is
        // now: re-anchoring it per pixel would make it jitter under the hand
        // that is holding still to read it.
        editor.tooltip = text.map(|text| zemacs_core::Tooltip { frame: i, x, y, text });
        self.hovered = (hit.map(|(w, row)| (i, w, row)), editor.tooltip.is_some());
    }

    /// [`lisp_call`], sent. Four of the five signals build and send in the same
    /// breath; the fifth is `after-edit-hook`, whose payload has to be sliced
    /// where the lock is.
    fn call_lisp(&self, name: &str, args: &str) {
        self.lisp.eval(lisp_call(name, args));
    }

    /// Tell the child what the left button did, at cell `(col, row)`. Answers
    /// whether the program running in it wanted the report.
    ///
    /// Left only, and one function for all three of press, release and drag,
    /// because the three differ in exactly one field and getting *fewer* than
    /// three of them right is the bug: a program that only ever hears the press
    /// has a button held down forever. The right button never arrives here at
    /// all — it is the context menu's, in every mode.
    fn term_mouse(
        &self,
        editor: &Editor,
        kind: zemacs_term::MouseKind,
        col: usize,
        row: usize,
    ) -> bool {
        self.term.mouse(editor, zemacs_term::Mouse {
            button: zemacs_term::Button::Left,
            kind,
            col,
            row,
        })
    }

    /// The one place a command becomes an action. Everything the pure core can do
    /// goes to `apply`; the three effects it can't perform are handled here.
    ///
    /// `CloseFrame` is a fourth: core removes the frame, but only this layer can
    /// take the window down with it, and `M-x delete-frame` reaches core through
    /// here just like the window's close button does.
    fn dispatch(&mut self, editor: &mut Editor, cmd: EditorCommand) {
        match cmd {
            EditorCommand::CallLisp(form) => self.lisp.eval(form),
            // `paste-image` before the rest, because it is the one terminal verb
            // that needs something `Term` has not got: the clipboard belongs to
            // the window system, and this is the layer that owns a window.
            EditorCommand::Term(verb) if verb == "paste-image" => {
                self.paste_image(editor);
            }
            EditorCommand::Term(verb) => self.term.run(editor, &verb),
            EditorCommand::Project(verb) => self.project.run_verb(editor, &verb),
            EditorCommand::PromptSource(spec) => self.project.fill_prompt(editor, &spec),
            // Straight back into the image as `NAME` and `PATH` pairs, because
            // the picker that asked is Lisp and the thing that can answer is a
            // font library. Scanned on demand rather than at startup: it costs a
            // few hundred `open`s, nobody who never runs `M-x choose-font`
            // should pay them, and a font installed mid-session should show up.
            EditorCommand::ListFonts => {
                let pairs: String = zemacs_render::monospace_fonts()
                    .iter()
                    .map(|(name, path)| {
                        format!(
                            "({} . {})",
                            zemacs_rpc::lisp::string(name),
                            zemacs_rpc::lisp::string(&path.to_string_lossy())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                self.call_lisp("%FONTS-LISTED", &format!("'({pairs})"));
            }
            // Dropped when no shell is running: a keystroke aimed at something that
            // is not there is nothing, not an error worth reporting on every key.
            EditorCommand::TermKey(key) => {
                self.term.key(editor, key);
            }
            // dired borrows the file prompt for rename/copy/mkdir, so an answer to
            // one of those is a filename for *it* rather than a file to open.
            EditorCommand::OpenFile(path) if self.dired.awaiting_input() => {
                self.dired.supply(editor, &path.to_string_lossy())
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
                self.dired.run(editor, "open");
            }
            // A remote name reaches here rather than the arm above, because
            // `/ssh:host:/etc` is not a directory *on this machine* — which is
            // the only question `is_dir` can answer. `open_file` sorts it out.
            EditorCommand::OpenFile(path) => {
                open_file(editor, &path, &self.init_path, &mut self.remote)
            }
            EditorCommand::OpenAt(hit) => {
                let root = self.project.search_root(editor);
                open_at(editor, &root, &hit, &self.init_path, &mut self.remote);
            }
            EditorCommand::SaveFile(path) => {
                save_file(editor, path, Save::Guarded, &mut self.remote)
            }
            EditorCommand::Git(verb) => {
                self.magit.run(editor, &verb);
                // `RET` on a file in the status buffer leaves magit; opening a
                // buffer is this layer's job, so magit asks rather than doing
                // it — the same shape, and the same reason, as dired below.
                if let Some(path) = self.magit.open_file.take() {
                    open_file(editor, &path, &self.init_path, &mut self.remote);
                }
            }
            // The far side of a `yes`. Each arm goes to the *same* worker its
            // guarded twin does, with the guard spent — so the question is asked in
            // exactly one place and answered in exactly one place.
            EditorCommand::Confirmed(inner) => match *inner {
                EditorCommand::SaveFile(path) => {
                    save_file(editor, path, Save::Forced, &mut self.remote)
                }
                EditorCommand::Git(verb) => self.magit.run_confirmed(editor, &verb),
                EditorCommand::Dired(verb) => self.dired.run_confirmed(editor, &verb),
                // Nothing else parks a command, so this is a confirmation for
                // something that never asked — a bug in the caller, not in the
                // answer, and worth saying rather than running.
                other => editor.apply(EditorCommand::Message(format!(
                    "confirmed a command that is not guarded: {other:?}"
                ))),
            },
            // What dired *asks* for — a file to open, a directory to list, a
            // write to send — is drained once a frame in `housekeep` rather
            // than here. Every verb that asks for something arrives on the far
            // side of a confirmation or a prompt, so "here" was the one arm
            // none of them come through.
            EditorCommand::Dired(verb) => self.dired.run(editor, &verb),
            EditorCommand::CloseFrame => {
                let focused = editor.focus_frame;
                close_frame(editor, &mut self.renderers, focused);
            }
            other => editor.apply(other),
        }
    }

    /// One SDL event, routed. `Break` is the window server saying quit — the one
    /// answer that ends the loop rather than the iteration.
    ///
    /// Everything that has to survive the *rest* of the batch goes into `batch`
    /// rather than being acted on here; see [`Batch`] for why each of the three
    /// does.
    fn route_event(
        &mut self,
        editor: &mut Editor,
        event: Event,
        batch: &mut Batch,
    ) -> ControlFlow<()> {
        // Every event, before the match decides which one it was. Most arms end
        // in a command and would say so anyway; the ones that do not are the
        // ones that matter here — a resize writes each pane's width straight
        // onto the editor from inside the *draw*, so a window the loop had
        // stopped drawing would keep its old geometry for as long as it took the
        // safety net to notice. See [`Editor::generation`].
        editor.touch();
        match event {
            Event::Quit { .. } => return ControlFlow::Break(()),
            // Event-driven rather than polled: reading the clipboard is a
            // trip through the window server, and SDL is already watching
            // it for us. ponytail: if a platform turns out not to raise
            // this, the fallback is a once-a-second pull beside the
            // auto-save timer — same call, worse latency.
            Event::ClipboardUpdate { .. } => self.clipboard.pull(editor),
            // How a file arrives from *outside* the process. Finder's "Open
            // With" and a double-click on a file zemacs is the default for
            // do not use argv — macOS sends an `odoc` Apple event, which SDL
            // turns into this. Dragging a file onto the window is the same
            // event, so both work off one arm.
            // A file from *outside* the process. Finder's "Open With" and a
            // double-click on a file zemacs is the default for do not use argv —
            // macOS sends an `odoc` Apple event, which SDL turns into this.
            // Dragging a file onto the window is the same event.
            //
            // Onto a *terminal* it means something else, and that is the whole
            // of this arm: dragging a screenshot onto a coding agent is how you
            // show it what is wrong, and opening the PNG in an editor pane is
            // not what anybody meant by the gesture. So the path is typed into
            // the child, which is what every harness takes a picture as.
            Event::DropFile { filename, .. } => {
                let path = PathBuf::from(filename);
                let typed = editor.mode == zemacs_core::Mode::Terminal
                    && self.term.paste_path(editor, &path);
                if !typed {
                    open_file(editor, &path, &self.init_path, &mut self.remote);
                }
            }
            Event::Window {
                window_id,
                win_event,
                ..
            } => {
                let frame = self.frame_for(window_id);
                // Anything the *window system* says happened is a reason to
                // put the frame up again whether or not the editor moved:
                // exposed, resized, un-minimised, dragged to another
                // display. This is the one case where "the picture is
                // identical" is not the same as "the screen is right", and
                // it is what the skipped present below is safe *because* of.
                // Window events are rare, so the extra present costs nothing
                // anyone can measure.
                if let Some(i) = frame {
                    self.renderers[i].invalidate();
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
                            self.dispatch(editor, EditorCommand::FocusFrame(i));
                        }
                    }
                    WindowEvent::CloseRequested => {
                        if let Some(i) = frame {
                            batch.closing.get_or_insert(i);
                        }
                    }
                    // The one way the pointer can leave a hover box behind that
                    // motion cannot clean up after: the last motion event is at
                    // the edge, and there is no motion *outside* the window to
                    // notice with. Without this, walking the mouse off the side
                    // of the frame while over a diagnostic leaves the message
                    // painted there until the next keystroke.
                    // Nothing has to touch `hovered`: it records whether a box
                    // was up, so taking one down is itself the invalidation.
                    WindowEvent::MouseLeave => editor.tooltip = None,
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
                batch.stamp(timestamp);
                batch
                    .keys
                    .extend(key_from_keydown(kc, keymod, !self.text_input_on));
            }
            // The right button, whose one job is the menu. Deliberately not
            // sent to a terminal child even in Terminal mode: a right-click
            // is how you reach the *window manager* here, and there is no
            // other gesture that opens a second frame with the mouse.
            Event::MouseButtonDown {
                window_id,
                mouse_btn: MouseButton::Right,
                x,
                y,
                ..
            } => {
                if let Some((i, x, y)) = self.pointer(window_id, x as i32, y as i32) {
                    // Focused first, for the left click's reason: the menu's
                    // verbs act on the focused frame, and right-clicking an
                    // unfocused window and getting a split in another one is
                    // the one outcome nobody means.
                    self.dispatch(editor, EditorCommand::FocusFrame(i));
                    editor.open_context_menu(x, y);
                }
            }
            Event::MouseButtonDown {
                window_id,
                mouse_btn: MouseButton::Left,
                x,
                y,
                clicks,
                ..
            } => {
                let Some((i, x, y)) = self.pointer(window_id, x as i32, y as i32) else {
                    return ControlFlow::Continue(());
                };
                // A box that only says what is under the pointer has nothing to
                // add once the pointer has been *used*, and a click is very
                // often the start of something that moves the text under it.
                editor.tooltip = None;
                let area = self.renderers[i].content_area();
                // A menu is modal to the pointer: while one is up, the
                // left button belongs to it and to nothing else. Picking
                // takes it down, and so does a click that missed — which
                // is what a click outside a menu means everywhere.
                if editor.context_menu.is_some() {
                    let row = self.renderers[i].context_menu_row(editor, x, y);
                    let picked = editor.pick_context_menu(row);
                    if let Some(verb) = picked {
                        // Through `run_action`, the door a keybinding
                        // uses: a menu entry cannot do anything a key
                        // could not.
                        for cmd in editor.run_action(verb) {
                            self.dispatch(editor, cmd);
                        }
                    }
                    return ControlFlow::Continue(());
                }
                // A click anywhere in a window focuses its frame. The
                // `FocusGained` above would do it a moment later anyway;
                // doing it here means the `FocusWindow` below addresses
                // the frame that was actually clicked.
                self.dispatch(editor, EditorCommand::FocusFrame(i));
                let pressed = self.mouse.press(&editor.frames[i], i, area, x, y);
                if let Some(window) = pressed {
                    self.dispatch(editor, EditorCommand::FocusWindow(window));
                    // A pane showing a scene has no character to land
                    // on: there is no point in a scene and no offset a
                    // click could name, so the gesture is a hit test and
                    // what it means is Lisp's — the same division of
                    // labour an `OverlayId` has. Same guard as the
                    // terminal's click and as a mode hook: a config that
                    // never defined the handler is silence, not an
                    // error. Nothing is escaped because a tag is an
                    // integer, which is the reason a tag *is* an
                    // integer.
                    //
                    // ponytail: `hit` also answers the node id, and this
                    // drops it — so "you clicked a figure, which means
                    // nothing" and "you clicked outside the page" both
                    // arrive as NIL. Ceiling: a mode wanting to react to
                    // an untagged node. The upgrade path is a second
                    // argument, since the id is already in hand here.
                    // Resolved before the match so the borrow of the
                    // renderer ends with the statement: the other arm
                    // dispatches, and dispatching wants every renderer.
                    let on_a_page = self.renderers[i]
                        .scene_layout(editor, i)
                        .map(|(layout, _)| {
                            zemacs_gui::hit(layout, x, y).and_then(|(_, tag)| tag)
                        });
                    match on_a_page {
                        Some(tag) => {
                            let tag = tag.map_or("nil".into(), |t| t.to_string());
                            self.call_lisp("%SCENE-CLICK", &tag);
                        }
                        // ...and then land on the character that was
                        // clicked. Focusing alone made the pointer a way
                        // to pick a *pane* and nothing smaller, which is
                        // the one thing everybody expects a mouse to do.
                        // The arithmetic belongs to the renderer — see
                        // `click_target` for why — and it comes back
                        // after the focus so the window it names is live.
                        None => {
                            let target = self.renderers[i].click_target(editor, i, x, y);
                            if let Some((_, at)) = target {
                                // A click collapses whatever was
                                // selected, the way it does everywhere
                                // else — without this, clicking during
                                // a selection drags its far end instead
                                // of starting again. Left to `MoveTo`
                                // alone the drag below would also find
                                // an anchor it never set.
                                if editor.mode.is_visual() {
                                    let cmd = EditorCommand::SetMode(zemacs_core::Mode::Normal);
                                    self.dispatch(editor, cmd);
                                }
                                self.dispatch(editor, EditorCommand::MoveTo(at));
                            }
                        }
                    }
                }
                // A program that asked for mouse events gets the click:
                // that is what makes vim, htop and tmux usable in here.
                // Focusing the pane happened first, so clicking into an
                // unfocused terminal both focuses it and reaches the
                // program in one gesture.
                if editor.mode == zemacs_core::Mode::Terminal {
                    let (col, row, right) = cell_at(editor, &self.renderers, x, y);
                    // Shift takes the mouse back off the child, which is
                    // the convention in every terminal emulator there is
                    // and the only way to copy text out of a full-screen
                    // program that has claimed it. Read from SDL rather
                    // than carried on the event because SDL3's button
                    // events do not carry a modifier field.
                    let shift = self
                        .sdl
                        .keyboard()
                        .mod_state()
                        .intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
                    let taken = !shift
                        && self.term_mouse(editor, zemacs_term::MouseKind::Press, col, row);
                    // Nobody wanted it. A shell never turns mouse
                    // reporting on, so this is the gesture that used to
                    // do nothing at all: it starts a selection in the
                    // grid, and if it turns out to have been a click
                    // rather than a drag, the release below hands it to
                    // Lisp instead.
                    self.mouse.selecting = !taken;
                    if taken {
                        self.term.select_clear(editor);
                    } else {
                        let kind = zemacs_term::Select::from_clicks(clicks);
                        self.term.select_start(editor, col, row, right, kind);
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
                let selecting = self.mouse.release();
                // A press with no release is half a gesture. SGR reporting
                // makes the two separate events, so a program that only
                // ever hears the press has a button held down forever —
                // which is why clicking in `vim` or `htop` in here did
                // nothing until the *next* click.
                if editor.mode == zemacs_core::Mode::Terminal {
                    if let Some((_, x, y)) = self.pointer(window_id, x as i32, y as i32) {
                        let (col, row, _) = cell_at(editor, &self.renderers, x, y);
                        if !selecting {
                            self.term_mouse(editor, zemacs_term::MouseKind::Release, col, row);
                        } else if !self.term.copy_selection(editor) {
                            // Selected nothing, so the gesture was a click
                            // after all: it goes to Lisp with the row it
                            // landed on and whatever OSC 8 link the child
                            // hung on that cell, and which of the two is a
                            // link is decided there. Same guard as a mode
                            // hook: a config that never defined it is
                            // silence.
                            if let Some((line, uri)) = self.term.click_context(editor, col, row) {
                                let uri =
                                    uri.map_or("nil".into(), |u| zemacs_rpc::lisp::string(&u));
                                self.call_lisp(
                                    "%TERMINAL-CLICK",
                                    &format!("{} {col} {uri}", zemacs_rpc::lisp::string(&line)),
                                );
                            }
                        }
                    }
                }
            }
            Event::MouseMotion {
                window_id,
                x,
                y,
                mousestate,
                ..
            } => match self.mouse.dragging() {
                // A held divider keeps its own frame rather than the event's:
                // SDL captures the mouse for the press, so the coordinates
                // stay in that window's space even once the pointer has left
                // it, and letting go of the drag at the edge is exactly the
                // bug this avoids.
                Some(i) => {
                    if let Some(renderer) = self.renderers.get(i) {
                        let (x, y) = renderer.to_pixels(x as i32, y as i32);
                        self.mouse.motion(&mut editor.frames, x, y);
                    }
                }
                None => {
                    if let Some((i, x, y)) = self.pointer(window_id, x as i32, y as i32) {
                        let area = self.renderers[i].content_area();
                        if let Some(cursors) = &mut self.cursors {
                            cursors.hover(editor.frames[i].divider_at(area, x, y).map(|d| d.dir));
                        }
                        // A menu row lights up under the pointer, which is
                        // the only thing that makes it read as clickable.
                        if editor.context_menu.is_some() {
                            let row = self.renderers[i].context_menu_row(editor, x, y);
                            if let Some(m) = editor.context_menu.as_mut() {
                                m.hover = row;
                            }
                        }
                        // ...and a mark in the gutter says what it is about.
                        // Not while a button is down: that is a drag, a
                        // gesture with a destination, and a box appearing
                        // under the hand halfway through one is noise.
                        if !mousestate.left() {
                            self.hover(editor, i, x, y);
                        }
                        // Held-button motion is a drag, which is how a
                        // selection is made in `vim` or a pane resized in
                        // `tmux`. The terminal drops it unless the child
                        // asked for drag reporting, so this costs nothing
                        // for a program that did not — and when the press
                        // was ours rather than the child's, the same
                        // motion drags out the grid selection instead.
                        if mousestate.left() && editor.mode == zemacs_core::Mode::Terminal {
                            let (col, row, right) = cell_at(editor, &self.renderers, x, y);
                            if self.mouse.selecting {
                                self.term.select_update(editor, col, row, right);
                            } else {
                                self.term_mouse(editor, zemacs_term::MouseKind::Drag, col, row);
                            }
                        }
                        // ...and everywhere else a drag is a selection. The
                        // press already put the cursor where the gesture
                        // started, so entering Visual anchors it there and
                        // every motion after it moves only the far end —
                        // the machinery `v` uses, which is the whole reason
                        // `y`, `d`, `gv` and the modeline all work on what
                        // was dragged without knowing a mouse was involved.
                        //
                        // `click_target` answers `None` over a dashboard or
                        // a terminal, so neither needs excluding here: one
                        // is a menu and the other is a grid the child owns.
                        if mousestate.left() && editor.mode != zemacs_core::Mode::Terminal {
                            let target = self.renderers[i].click_target(editor, i, x, y);
                            if let Some((window, at)) = target {
                                // Only over the pane the press landed in. A
                                // drag that wanders into the next split
                                // would otherwise read an offset out of
                                // *that* buffer and apply it to this one.
                                let same = window == editor.frames[i].current;
                                // A click that wobbles by a pixel is still a
                                // click. Measured in characters rather than
                                // pixels, because a character is the unit a
                                // selection is actually made of.
                                let moved = at != editor.buffer.cursor;
                                if same && (moved || editor.mode.is_visual()) {
                                    if !editor.mode.is_visual() {
                                        let cmd = EditorCommand::SetMode(zemacs_core::Mode::Visual);
                                        self.dispatch(editor, cmd);
                                    }
                                    self.dispatch(editor, EditorCommand::MoveTo(at));
                                }
                            }
                        }
                    }
                }
            },
            // SDL reports natural-scroll wheels with `direction: Flipped`
            // and the raw sign, so undo that first; then negate, because a
            // wheel push (positive y) moves the view *up* the file.
            //
            // ponytail: and then negate *again*, below, because the wheel
            // that reads right on this desk is the other one. Hard-coded
            // rather than a setting — a `set-scroll-direction` primitive is
            // a C shim, an extern, a defprim and an export for one bool.
            // Add it when a second person disagrees about which way is up.
            // `integer_y`, not `y`. SDL3 made `y` the *precise* delta — a
            // trackpad delivers a stream of fractions — where SDL2's `y` was the
            // quantised notch count this arm is written against. `integer_y` is
            // that notch count, so it is the port of the old field and not the
            // similarly-named new one; taking `y` compiles and then scrolls
            // either not at all or wildly, depending on where you truncate.
            Event::MouseWheel {
                window_id,
                integer_y: y,
                direction,
                mouse_x,
                mouse_y,
                ..
            } => {
                let y = -match direction {
                    MouseWheelDirection::Flipped => -y,
                    _ => y,
                };
                // The one gesture that moves the text under a pointer that has
                // not moved, so nothing else will notice the box has gone stale.
                editor.tooltip = None;
                let frame = self.pointer(window_id, mouse_x as i32, mouse_y as i32);
                if let (true, Some((i, px, py))) = (y != 0, frame) {
                    // Scroll the pane under the pointer — by focusing it
                    // first. `ScrollLines` moves the *live* window, and
                    // focusing is the only way to make a pane live without
                    // duplicating core's scroll clamping out here. It also
                    // matches the click: pointing at a pane and acting on it
                    // is the same gesture either way.
                    let area = self.renderers[i].content_area();
                    editor.focus_frame = i;
                    let window = editor.frames[i].window_at(area, px, py);
                    if let Some(window) = window {
                        self.dispatch(editor, EditorCommand::FocusWindow(window));
                    }
                    // A terminal has its own scrollback, and while the shell
                    // has the keyboard the buffer holds only the *visible*
                    // grid — scrolling that would move through a screenful
                    // that is already all there is. `wheel` also knows to
                    // hand the notch to a program that asked for mouse
                    // events, or to send arrow keys to `less`.
                    if editor.mode == zemacs_core::Mode::Terminal {
                        let (col, row, _) = cell_at(editor, &self.renderers, px, py);
                        self.term.wheel(editor, -y * SCROLL_LINES, col, row);
                    } else if let Some(at) = editor.buffer.scene.as_ref().map(|s| s.scroll) {
                        // A scene scrolls in pixels and has no viewport of
                        // lines for `ScrollLines` to move — the cursor does
                        // not exist here, so there is nothing to keep on
                        // screen and nothing for core to clamp against. The
                        // notch is the same three lines the document gets,
                        // in the pane's own line height.
                        let step = -y * SCROLL_LINES * self.renderers[i].cell_size().1;
                        scroll_scene(editor, &mut self.renderers[i], i, at + step);
                        // No `invalidate()` here. The offset reaches the
                        // frame digest — `draw_scene` folds it in, and the
                        // boxes it moved were folded in anyway — so a notch
                        // that changed the picture presents and a notch
                        // against the end of the document does not, which
                        // is what an unconditional invalidate got wrong in
                        // the second case.
                    } else {
                        self.dispatch(editor, EditorCommand::ScrollLines(-y * SCROLL_LINES));
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
                batch.stamp(timestamp);
                batch.keys.extend(text.chars().map(Key::Char));
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    /// Toggle SDL text input to match the mode. This is what stops macOS
    /// press-and-hold: the accent panel is a text-input-client feature, so
    /// with text input off, holding `j` repeats the keystroke natively
    /// instead of offering ĵ. Insert mode and prompts keep it on, where
    /// layout-correct characters and dead keys are what you actually want.
    fn sync_text_input(&mut self, editor: &Editor) {
        let want_text = wants_text_input(editor);
        if want_text == self.text_input_on {
            return;
        }
        self.apply_text_input(want_text);
        self.text_input_on = want_text;
    }

    /// Tell *every* window, because SDL3 made text input a property of a window
    /// rather than of the application.
    ///
    /// Two consequences the guarded `sync_text_input` above cannot see on its
    /// own: a frame opened later starts at SDL's default rather than at ours, so
    /// the loop re-applies this after pushing a renderer; and the mode is global
    /// to the editor, so telling only the focused window would leave Insert mode
    /// dead the moment you clicked into another frame.
    fn apply_text_input(&self, on: bool) {
        for renderer in &self.renderers {
            if on {
                self.text_input.start(renderer.window());
            } else {
                self.text_input.stop(renderer.window());
            }
        }
    }

    /// Everything the image and the parser are told about this frame.
    ///
    /// One phase because the two of them ask the same question — what has
    /// happened since I last looked — and answer it from watermarks that have to
    /// move in step with the thing they describe. Under the lock, necessarily:
    /// the delta is *sliced* here so that whatever reads it a turn later is
    /// reading a record rather than a document that has moved on. See
    /// [`after_edit_form`], which is where that argument is written down.
    fn tell_readers(&mut self, editor: &mut Editor, highlighter: &zemacs_syntax::Worker) {
        // Hand each new revision to the syntax thread and carry on drawing.
        let mut edit_form = None;
        if editor.revision != self.last_revision {
            self.last_revision = editor.revision;
            // The image's one signal that the document moved. Core reports mode
            // *entry* and nothing else, so without this a language server could
            // never learn that a buffer changed — and neither could anything
            // else a config wants to hang off an edit. Queued through
            // `pending_hooks` so it takes the same route, and the same
            // `fboundp` guard, as every other hook.
            //
            // Not for a generated buffer: a terminal rewrites itself as fast as
            // the shell prints, and a Lisp form per frame of `ls -R` would
            // starve the image's queue for work no config can act on anyway —
            // every generated buffer is read-only.
            if !editor.buffer.kind.is_generated() {
                editor.pending_hooks.push("after-change-hook".into());
                edit_form = after_edit_form(editor, &mut self.told_lisp);
            }
            match &editor.buffer.language {
                Some(lang) => highlighter.request(zemacs_syntax::Request {
                    // The buffer's own change count, not the editor's revision:
                    // the answer has to be checked against *this* buffer when it
                    // lands, and a global counter that a keystroke anywhere else
                    // moves cannot say whether this text is still this text.
                    seen: editor.buffer.change_count(),
                    buffer: editor.buffer.id,
                    lang: lang.clone(),
                    text: editor.buffer.text.to_string(),
                    // What the parser's tree has not been told yet. `None` —
                    // a different buffer, or a reader that fell further behind
                    // than core's log reaches — is "assume nothing", which
                    // costs the full parse every revision used to cost.
                    //
                    // Advanced only when a request is actually sent, and that
                    // is the point of it being its own watermark: a buffer with
                    // no language is highlighted by nobody, and sharing the
                    // image's watermark would hand the parser, the moment a
                    // language was set, a list of edits starting after text it
                    // never saw.
                    edits: self
                        .told_syntax
                        .filter(|(b, _)| *b == editor.buffer.id)
                        .and_then(|(_, seen)| editor.buffer.changes_since(seen))
                        .map(<[_]>::to_vec),
                }),
                // A generated buffer colours itself — dired and magit hand their
                // own spans straight to the editor — so clearing here would
                // wipe them one frame after they were produced.
                None if !editor.buffer.kind.is_generated() => editor.buffer.highlights.clear(),
                None => {}
            }
            if editor.buffer.language.is_some() {
                self.told_syntax = Some((editor.buffer.id, editor.buffer.change_count()));
            }
        }
        request_pending_parses(editor, highlighter);
        // ...and the cursor half of the same report, before the queue below is
        // drained so it goes out with this frame's hooks. Read against the live
        // buffer and not the focused window's, because a switch moves point too
        // even when the number does not change; see [`queue_point_moved`].
        queue_point_moved(editor, &mut self.last_point);

        // Mode hooks: core records that one is due, the image runs it. Guarded
        // with `fboundp` so a mode with no hook defined is silence rather than
        // an "undefined function" every time you open a file.
        for hook in std::mem::take(&mut editor.pending_hooks) {
            self.call_lisp(&hook.to_uppercase(), "");
        }
        // ...and the one hook that carries arguments, queued after the
        // no-argument ones so that a config which uses both sees the order it
        // has always seen. It does not go through `pending_hooks` because that
        // channel carries a *name*: core asks for hooks by name and has no
        // business holding a Lisp form, and this call is only meaningful with
        // the delta baked in.
        if let Some(form) = edit_form {
            self.lisp.eval(form);
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
            self.call_lisp("%RPC-EVENT", &format!("{conn} {kind} '{form}"));
        }
    }

    /// The per-frame housekeeping: everything core holds but cannot fill in
    /// itself.
    ///
    /// One phase and not eight because they are all the same shape and all have
    /// the same reason — core does no IO, owns no window and has no parser, and
    /// this is the one layer with all three. The order is the order they depend
    /// on each other in, which is the only thing that stops them being eight
    /// separate methods: a frame gets its window before a terminal inside it is
    /// measured against the pane it now has.
    fn housekeep(
        &mut self,
        editor: &mut Editor,
        highlighter: &zemacs_syntax::Worker,
    ) -> anyhow::Result<()> {
        // First, because a reply can replace the whole document — the highlight
        // adoption and the parse request below should see the buffer this frame
        // rather than the next one.
        self.remote.poll(editor, &mut self.dired);

        // ...and immediately after it, because dired owns no ssh worker and no
        // buffers: it sets a field and someone else spends it. Here rather than
        // under `EditorCommand::Dired`, where the listing's ask used to be
        // drained, because every *write* verb reaches dired through
        // `EditorCommand::Confirmed` or through the file prompt instead — so a
        // per-arm drain was a request that sat unsent until the next unrelated
        // verb, and a verb added later through a fourth door would have been
        // unsent again. Once a frame, unconditionally, is the shape that cannot
        // be arrived at from the wrong side. See [`Remote::take_from`].
        if let Some(path) = self.dired.open_file.take() {
            open_file(editor, &path, &self.init_path, &mut self.remote);
        }
        self.remote.take_from(&mut self.dired);

        refresh_file_completions(editor, &mut self.last_file_query);
        refresh_grep(editor, &self.project, &mut self.last_grep);

        adopt_highlights(editor, highlighter);

        highlight_completion_doc(editor);

        // Where `M-x new-frame` becomes a window. Core pushes the frame, the
        // loop notices it has no renderer for it. Emacs spells extra frames
        // `<2>`, `<3>`; so do we, so they are tellable apart in the dock.
        while self.renderers.len() < editor.frames.len() {
            let title = format!("zemacs <{}>", self.renderers.len() + 1);
            let mut renderer = Renderer::new(&self.sdl, &title, WINDOW_W, WINDOW_H)?;
            // Keys are routed to `focus_frame`, not to whichever window the OS
            // considers frontmost, so a new frame has to claim it here. macOS
            // does not always send `FocusGained` for a window the application
            // opened itself, and without this every keystroke kept going to the
            // frame you opened the new one *from*.
            renderer.focus();
            editor.focus_frame = self.renderers.len();
            self.renderers.push(renderer);
            // Text input is per-window in SDL3, and this one was born with
            // SDL's default rather than the editor's mode — so a frame opened
            // from Insert mode would take keys and compose nothing.
            self.apply_text_input(self.text_input_on);
        }

        // Size each session to the pane it is actually shown in, then let them
        // all catch up. `sync` is also what answers a child's queries, so it has
        // to run every frame whether or not the terminal is on screen — a
        // program asking how big its window is blocks until it is told, and a
        // background agent that nobody polls is a background agent that hangs.
        //
        // Measured out here because `Term` owns no renderer: it says which
        // buffers it has, this says how big each one's pane is.
        if self.term.is_live() {
            let sizes: Vec<(zemacs_core::BufferId, usize, usize)> = self
                .term
                .buffers()
                .into_iter()
                .map(|id| {
                    let (cols, rows) = terminal_size(editor, &self.renderers, id);
                    (id, cols, rows)
                })
                .collect();
            self.term.sync(editor, &sizes);
        }

        // Wall-clock rather than keystroke-counted: the loop already runs at
        // vsync, so an elapsed check is free, and a crash costs at most this
        // interval's worth of typing either way.
        if self.last_autosave.elapsed() >= AUTOSAVE_EVERY {
            autosave_all(editor);
            self.last_autosave = Instant::now();
        }

        // Same shape, same argument, one order of magnitude more often: this one
        // is a `stat` rather than a write, and five seconds is how long a file
        // is allowed to be stale on screen.
        if self.last_revert.elapsed() >= REVERT_EVERY {
            self.revert_watch.poll(editor);
            self.last_revert = Instant::now();
        }

        // After the whole batch, so a yank and the delete before it push once
        // rather than twice — the register only has to be right by the time
        // anyone outside can ask.
        self.clipboard.push(editor);
        Ok(())
    }

    /// Put the clipboard's image somewhere, and type its path into the child.
    ///
    /// The one gesture a coding agent needs that a text editor has no reason to
    /// have: you take a screenshot of the thing that is wrong and hand it over.
    /// Every harness takes one the same way — a path in its prompt — so the
    /// whole feature is *write the bytes down and type where they went*.
    ///
    /// Reported when there is no image, unlike the plain paste beside it, and
    /// the difference is what the gesture means: `paste` with an empty register
    /// is a key that did nothing, and this is a key you pressed *because* you
    /// had just copied a picture. Being told the clipboard has text in it is the
    /// answer to "why did nothing happen".
    fn paste_image(&mut self, editor: &mut Editor) {
        let Some(path) = write_clipboard_image(&self.clipboard) else {
            // Text in the clipboard is the overwhelmingly likely case, and
            // pasting it is what was meant by anyone who pressed this by
            // mistake. So do that rather than refuse.
            if self.clipboard.util.has_clipboard_text() {
                self.term.paste(editor);
                return;
            }
            editor.apply(EditorCommand::Message(
                "no image in the clipboard".into(),
            ));
            return;
        };
        if !self.term.paste_path(editor, &path) {
            editor.apply(EditorCommand::Message(format!(
                "no terminal here — the image is at {}",
                display_path(&path)
            )));
        }
    }

    /// Draw every window, and answer how many draw calls it took.
    ///
    /// Every window, every iteration, and `render` syncs the font itself.
    /// Drawing is the cheap half — measured at 2.4 ms for a full screen of
    /// text, against 14 ms of vertical blank below — so nothing here is
    /// conditional. What it produces besides pixels is a digest of every draw
    /// call it made, which is what lets the present be skipped.
    ///
    /// ponytail: an idle editor still redraws at the refresh rate, throwing
    /// the frame away when the digest says it was identical. That is a couple
    /// of milliseconds of CPU per display frame doing nothing, and it buys
    /// the one thing a cheaper test cannot: correctness without a list of
    /// "fields that mean a redraw" to keep in step with the renderer. The
    /// upgrade path is core stamping a generation on every mutation — the
    /// *only* signal that also catches a Lisp primitive editing the buffer
    /// through the shared mutex, which raises no event here — and then this
    /// loop can skip the draw as well as the present, and sleep properly.
    ///
    /// The scroll fixup at the top is the third writer named on `scroll_scene`,
    /// and the one core cannot do for itself: a scene is swapped in whole and
    /// carries the outgoing page's offset across, so a page that re-rendered
    /// *shorter* is left scrolled past its own end — until here, because the
    /// height that says so belongs to a laid-out scene and laying one out needs
    /// a font. A no-op on every frame where the offset was already legal, and
    /// the layout it asks for is the one the pane loop below is about to want
    /// anyway.
    fn draw(
        &mut self,
        editor: &mut Editor,
        screens: &[(BufferId, zemacs_term::Screen)],
    ) -> anyhow::Result<u32> {
        let focus = editor.focus_frame;
        let at = editor.buffer.scene.as_ref().map(|s| s.scroll);
        if let (Some(renderer), Some(at)) = (self.renderers.get_mut(focus), at) {
            scroll_scene(editor, renderer, focus, at);
        }

        for (i, renderer) in self.renderers.iter_mut().enumerate() {
            renderer.render(editor, i, screens)?;
        }
        Ok(self.renderers.iter().map(Renderer::draw_calls).sum())
    }

    /// Put on screen what changed, and answer how many windows that was.
    ///
    /// The one method here with no `Editor`, and deliberately: `main` drops the
    /// lock immediately above the call, because presenting parks this thread
    /// until the display's next vertical blank and holding the editor across
    /// that would put every Lisp primitive behind the display.
    fn present(&mut self) -> u32 {
        let mut presents = 0;
        for renderer in self.renderers.iter_mut() {
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
                // Not a bare `continue`. The frame we are refusing to show is
                // still sitting in SDL's command list, and only a flush takes it
                // out — leaving it there leaks every draw call of every skipped
                // frame, for as long as the editor is open. See
                // [`Renderer::discard`].
                renderer.discard();
                continue;
            }
            renderer.present();
            presents += 1;
        }
        presents
    }
}

// --- highlighting ----------------------------------------------------------
//
// The two ends of the syntax thread, as free functions rather than methods on
// `App`, because between them they are the whole answer to "is this buffer the
// right colour" and a test that cannot construct an `App` — it owns a window —
// still has to be able to ask.

/// Ask the syntax thread for a fresh parse of every buffer core has named.
///
/// Core names one when it replaces a buffer's text without anyone typing —
/// auto-revert, which is now the common case rather than the exotic one, since
/// a coding agent rewrites the files you are *not* looking at.
///
/// The live buffer is skipped, and not because it does not need a parse: the
/// revision block in [`App::tell_readers`] has already asked, and asked better.
/// It has the change log and this does not, so re-asking here would trade an
/// incremental parse for a full one on the one buffer whose latency is felt.
fn request_pending_parses(editor: &mut Editor, highlighter: &zemacs_syntax::Worker) {
    for id in std::mem::take(&mut editor.pending_highlight) {
        if id == editor.buffer.id {
            continue;
        }
        // Killed between the revert and this frame, or a buffer with no
        // language — a `.txt` file is highlighted by nobody.
        let Some(buffer) = editor.buffer_by_id(id) else {
            continue;
        };
        let Some(lang) = buffer.language.clone() else {
            continue;
        };
        highlighter.request(zemacs_syntax::Request {
            seen: buffer.change_count(),
            buffer: id,
            lang,
            text: buffer.text.to_string(),
            // No log to hand over, and none wanted: a revert replaces the whole
            // document, so there is no edit small enough to be worth describing.
            edits: None,
        });
    }
}

/// Put each finished parse on the buffer it was a parse *of*.
///
/// Every result, not only the newest. "The newest" — which is what a single
/// slot keyed by the editor's revision amounted to — was right while the live
/// buffer was the only thing ever parsed, and became the bug the moment it was
/// not: a parked buffer's colours were thrown away by whatever the live buffer
/// finished next.
///
/// The safety property the revision compare had is kept, and made per-buffer,
/// which is what it should have been all along: spans are adopted only if the
/// buffer's own change count *and* its language are still the ones the text was
/// snapshotted under. A parse of text that has since moved is dropped, because
/// colouring the wrong characters is worse than colouring none — and a newer
/// parse is already in flight for exactly that buffer.
fn adopt_highlights(editor: &mut Editor, highlighter: &zemacs_syntax::Worker) {
    while let Some(done) = highlighter.poll() {
        let mut adopted = false;
        if let Some(buffer) = editor.buffer_by_id_mut(done.buffer) {
            if buffer.change_count() == done.seen && buffer.language.as_deref() == Some(&done.lang) {
                buffer.highlights = done.spans;
                adopted = true;
            }
        }
        // A parse landing is the classic thing that happens between keystrokes:
        // the file arrives grey and turns colour a frame or two later, off a
        // worker thread that raises no event of its own. Nothing else would
        // tell the draw loop about it.
        if adopted {
            editor.touch();
        }
    }
}

// --- file effects --------------------------------------------------------

/// `@init` is the sentinel the dashboard's "Edit configuration" item uses —
/// the core has no idea where the config lives, this layer does.
fn open_file(editor: &mut Editor, path: &Path, init_path: &Path, remote: &mut Remote) {
    let path = if path == Path::new("@init") {
        init_path.to_path_buf()
    } else {
        path.to_path_buf()
    };
    // `/ssh:user@host:/etc/nginx.conf`. `None` is every other name there has
    // ever been, and getting it is one `memcmp` against `/ssh:` — see
    // [`Remote`] for why the remote half cannot happen on this line.
    if let Some(there) = tramp::parse(&path.to_string_lossy()) {
        remote.open(editor, there);
        return;
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let lang = zemacs_syntax::language_for_path(&path);
            let shown = display_path(&path);
            editor.load(&text, Some(path.clone()), lang);
            editor.buffer.file_mode = file_mode(&path);
            // The buffer and the file agree as of now, which is what a later
            // `:w` compares against to find out whether anyone else has been in.
            editor.buffer.visited = disk_stamp(&path);
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
fn open_at(editor: &mut Editor, root: &Path, hit: &str, init_path: &Path, remote: &mut Remote) {
    let mut parts = hit.splitn(3, ':');
    let (Some(path), Some(line)) = (parts.next(), parts.next()) else {
        editor.apply(EditorCommand::Message(format!("not a match: {hit}")));
        return;
    };
    // Relative, because ripgrep ran with the project root as its directory.
    // Always local — ripgrep searched this machine — but it goes through the
    // same door, so a `remote` handle travels with it rather than a second
    // spelling of "open a file" growing here.
    open_file(editor, &root.join(path), init_path, remote);
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

/// Whether `save_file` still has to ask about a file that moved underneath it.
///
/// `Forced` is only ever produced by answering the question, so the check
/// cannot be skipped by accident — see [`EditorCommand::Confirmed`].
#[derive(Clone, Copy, PartialEq)]
enum Save {
    Guarded,
    Forced,
}

fn save_file(editor: &mut Editor, path: Option<PathBuf>, guard: Save, remote: &mut Remote) {
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
    // Is this the file the buffer is already visiting, or a save-as? Almost
    // everything below turns on the difference.
    let visiting = editor.buffer.path.as_ref() == Some(&target);

    // Emacs' `save-buffer`: nothing to write means nothing is written. A save
    // that always writes moves the mtime of every file you look at, which is
    // what `make`, a file watcher and a git status all read as a change — and
    // it costs you the backup of the last version that *was* different, since
    // `backup` copies whatever is there now.
    //
    // Only when it is this buffer's own file. `:w <elsewhere>` is a save-as and
    // the whole point of one is to produce a file that is not there yet, so
    // "unmodified" says nothing about whether the target needs writing.
    if visiting && !editor.buffer.modified {
        editor.apply(EditorCommand::Message(format!(
            "{}: no changes need to be saved",
            display_path(&target)
        )));
        return;
    }

    // `*scratch*` is a *named, permanent* buffer you come back to, so naming it
    // after a file is the one save-as that must not consume the buffer it
    // saves. It duplicates instead: the file gets an ordinary buffer of its own
    // and the scratchpad stays a scratchpad, with its text, its undo and its
    // name. Every other pathless buffer — `*untitled*` — is named in place,
    // which is what Emacs' `write-file` does and is right for a buffer whose
    // name was only ever a placeholder.
    let duplicating = editor.buffer.kind == BufferKind::Scratch && !visiting;
    // The duplicate needs somewhere to land, and a buffer already visiting this
    // file is not it: `Editor::load` would switch to *that* buffer rather than
    // adopt the text, and stamping it saved would leave a buffer claiming to
    // hold a file it does not. Refused whole rather than half-done — writing
    // the file and then failing to duplicate would leave the open buffer
    // silently stale against a disk that had moved under it.
    if duplicating
        && editor
            .buffers()
            .any(|b| b.id != editor.buffer.id && b.path.as_ref() == Some(&target))
    {
        editor.apply(EditorCommand::Message(format!(
            "{} is already open — switch to it and paste, or pick another name",
            display_path(&target)
        )));
        return;
    }

    // Emacs' "has changed since visited; save anyway?". Only for the file this
    // buffer is actually visiting: `:w somewhere-else` is a save-as, and the
    // stamp on record describes the *original*, so comparing the two would be
    // asking about the wrong file.
    if guard == Save::Guarded && visiting {
        if let (Some(seen), Some(now)) = (editor.buffer.visited, disk_stamp(&target)) {
            if seen != now {
                let shown = display_path(&target);
                let question = format!("{shown} changed on disk — save anyway?");
                editor.confirm(
                    &question,
                    EditorCommand::Confirmed(Box::new(EditorCommand::SaveFile(Some(target)))),
                );
                return;
            }
        }
    }
    // The scratchpad forks into two buffers here, above the local/remote fork
    // so that both kinds of file get it, and above the write so that a write
    // which fails leaves a buffer holding the text and the name — which is
    // exactly the state visiting a file that does not exist yet puts you in,
    // and the state a second `:w` can finish from.
    //
    // `Editor::load` is the whole of it: it stacks the outgoing buffer into the
    // buffer list rather than discarding it, adopts the text into a fresh one,
    // and sets the kind to `Text` — so the scratchpad keeps its own undo
    // history and its own name while the file gets a buffer with none of a
    // scratchpad's properties. Marked modified until the write says otherwise;
    // `load` reasonably assumes what it adopted is on disk, and here it is not
    // yet.
    if duplicating {
        let language = zemacs_syntax::language_for_path(&target);
        let text = editor.buffer.text.to_string();
        editor.load(&text, Some(target.clone()), language);
        editor.buffer.modified = true;
    }
    // The fork, and it is *here* rather than in `dispatch` so that everything
    // above is asked once for both kinds of file: the refusal to write a
    // rendered view, `:w <path>` as a save-as, and the changed-on-disk
    // question. That last one self-disables for a remote name, because
    // `disk_stamp` is a local `stat` and answers `None` — which is the honest
    // answer and the affordable one; asking it properly is a round trip per
    // save. Below is the local half and nothing in it means anything off this
    // machine — see [`Remote`] for what a remote save does instead.
    if let Some(there) = tramp::parse(&target.to_string_lossy()) {
        remote.save(editor, there);
        return;
    }
    let text = editor.buffer.text.to_string();
    // Before the old contents stop existing. Taken here rather than inside
    // `write_file` so the two promises stay separable: one is about the write
    // not tearing, the other about the previous version surviving, and a test
    // for either should not have to arrange the other.
    backup(&target);
    match write_file(&target, &text) {
        Ok(()) => {
            editor.buffer.modified = false;
            editor.buffer.file_mode = file_mode(&target);
            editor.buffer.visited = disk_stamp(&target);
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

// --- remote files --------------------------------------------------------
//
// `/ssh:user@host:/etc/nginx.conf` in `find-file`, in `:w` and in dired.
// `crates/tramp` does all of the work — the syntax, the ssh, the quoting, the
// atomic remote write. What is left, and all that is here, is the one thing a
// library cannot decide: *when*.
//
// # Why none of it happens on the call
//
// An ssh round trip is tens of milliseconds on a warm control socket, a few
// hundred cold, and up to `tramp::TIMEOUT` — a minute — against a host that
// accepted a connection and then went away. `docs/threading.org` is the law:
// the main thread holds the editor lock for one iteration of input and
// drawing, and every Lisp primitive on the image's thread waits on that same
// lock. So a blocking read here would freeze the window *and* stop the config
// dead, for a minute, over a typo in a hostname.
//
// So it is the shape the rest of the editor already uses for slow work:
// `crates/rpc` runs its language servers on their own threads and delivers
// replies through this loop, `zemacs_syntax::Worker` highlights the same way,
// and `tramp::Worker` was built to match them. A request goes out, `poll`
// picks the answer up in `housekeep`, and the effect lands on a later frame.
// `find-file` from Lisp already lands a frame late (`docs/threading.org` says
// so out loud); this makes it several frames late, which is the same promise
// with a worse constant and no new rule to learn.
//
// # What the local path pays
//
// `tramp::parse` on the name, which is `strip_prefix("/ssh:")`, and nothing
// else. The worker thread is not spawned until the first remote name of the
// session, so a user who never types `/ssh:` has one extra `memcmp` per
// `find-file` and not one thread, socket or allocation.
//
// # What a remote buffer does not get
//
// Written down because each is a deliberate answer, not an omission:
//
//   * **auto-revert: off.** The sweep `stat`s every open file four times a
//     second. Over ssh that is a network round trip per buffer per 250ms,
//     which is a performance bug rather than a feature — and the sweep's own
//     header already named this crate as the condition for changing its
//     answer. See `Revert::poll`.
//   * **numbered backups: none.** `backup` is `fs::copy` of the file being
//     replaced, and the remote copy of that is a whole transfer per save.
//     ponytail: the upgrade is one line in `tramp`'s `write_script`, which
//     already `cp -p`s the target aside — it throws the copy away instead of
//     numbering it. The tearing half of the promise is *not* lost: the remote
//     write is temp-then-`mv`, exactly as `write_file` is here.
//   * **the changed-on-disk save guard: off**, since `visited` is a local
//     `stat`. Asking properly is a round trip per save.
//   * **auto-save: on, and local.** `~/.zemacs.d/auto-save/#!ssh:host:!etc!x#`
//     — the path mangler needs no help, the copy is on the machine that would
//     have crashed, and it is offered back on the next open because the
//     `stat` that opening does already carries the host's mtime. That is the
//     data-loss half, and it is the one that could not be skipped.
//
// All four are listed in `docs/boundary.org`.

/// One remote operation in flight, and what its answer is for.
enum Job {
    /// `find-file` on a name that cannot be classified without asking. One
    /// `stat` says file, directory, or nothing, and each answer queues its own
    /// follow-up. Two round trips to open a file — the second on a connection
    /// the first one opened, which is what `tramp`'s control socket is for.
    Probe(tramp::RemotePath),
    /// The read behind a `Probe`, carrying the metadata that `stat` already
    /// learned so the mode bits and the mtime are not asked for twice.
    Open(tramp::RemotePath, Option<tramp::RemoteEntry>),
    /// A directory, for dired.
    List(tramp::RemotePath),
    /// A write, and the buffer that asked for it.
    ///
    /// Named by id *and* by change count, because a save is the one operation
    /// whose reply changes state: it lands hundreds of milliseconds later, by
    /// which time the user may have switched buffers and typed. Both have to
    /// still be true or the flag stays where it is.
    Save {
        buffer: BufferId,
        at: u64,
        path: tramp::RemotePath,
        bytes: usize,
    },
    /// One of dired's write verbs — `rm`, `mv`, `cp`, `mkdir`, `: >`. Carries
    /// the file it acted on rather than the operation, because that is all the
    /// answer is for: `Dired` counts it, and a failure has to name something.
    Dired(tramp::RemotePath),
}

impl Job {
    /// What to blame when the operation fails. The remote path is the whole of
    /// the context a user needs: it names the host and the file.
    fn path(&self) -> &tramp::RemotePath {
        match self {
            Job::Probe(p)
            | Job::Open(p, _)
            | Job::List(p)
            | Job::Save { path: p, .. }
            | Job::Dired(p) => p,
        }
    }
}

/// The file a dired operation acts on — `from` for the two-name verbs, because
/// that is the line the cursor was on and the name the user will recognise.
///
/// Mechanical, and here rather than in `tramp` because it is this layer's
/// question: `Op` is a message, and only the thing reporting failures needs one
/// path out of it.
fn acted_on(op: &tramp::Op) -> tramp::RemotePath {
    match op {
        tramp::Op::Read(p)
        | tramp::Op::Write(p, _)
        | tramp::Op::List(p)
        | tramp::Op::Stat(p)
        | tramp::Op::Mkdir(p)
        | tramp::Op::CreateFile(p)
        | tramp::Op::Delete { path: p, .. }
        | tramp::Op::Rename { from: p, .. }
        | tramp::Op::Copy { from: p, .. } => p.clone(),
    }
}

/// The editor's end of `tramp::Worker`.
#[derive(Default)]
struct Remote {
    /// `None` until the first remote name of the session — see the header.
    worker: Option<tramp::Worker>,
    next: u64,
    /// By request id, which is the worker's contract: every request produces
    /// exactly one reply, tagged with the id it went out with, and nothing is
    /// coalesced or dropped.
    jobs: std::collections::HashMap<u64, Job>,
}

impl Remote {
    fn request(&mut self, op: tramp::Op, job: Job) {
        let id = self.next;
        self.next += 1;
        self.jobs.insert(id, job);
        self.worker
            .get_or_insert_with(tramp::spawn_worker)
            .request(id, op);
    }

    /// `find-file` on a remote name.
    fn open(&mut self, editor: &mut Editor, path: tramp::RemotePath) {
        editor.apply(EditorCommand::Message(format!("opening {path}…")));
        self.request(tramp::Op::Stat(path.clone()), Job::Probe(path));
    }

    /// A directory dired asked for. No message: dired said "listing …" when it
    /// decided to move, and this is the same event.
    fn list(&mut self, path: tramp::RemotePath) {
        self.request(tramp::Op::List(path.clone()), Job::List(path));
    }

    /// Everything dired has queued this frame, in the order it queued it.
    ///
    /// Called once per frame from [`App::housekeep`] and from nowhere else,
    /// which is the whole of why it is a method rather than four lines in a
    /// `match` arm. Every one of dired's write verbs arrives on the far side of
    /// a confirmation or a file prompt — `EditorCommand::Confirmed`,
    /// `EditorCommand::OpenFile` — so a drain sitting under
    /// `EditorCommand::Dired` reached none of them, and the next verb to be
    /// added through a door nobody has built yet would be silently unsent too.
    /// A drain that runs every frame cannot be arrived at from the wrong side.
    ///
    /// The writes before the listing, because the worker is one thread: the
    /// re-read that dired queued behind them is then the directory as they
    /// left it.
    fn take_from(&mut self, dired: &mut Dired) {
        for op in dired.want_op.drain(..) {
            let job = Job::Dired(acted_on(&op));
            self.request(op, job);
        }
        if let Some(dir) = dired.want_list.take() {
            self.list(dir);
        }
    }

    /// `:w` on a remote buffer.
    ///
    /// The buffer stays `modified` until the host confirms, which is the whole
    /// difference from a local save: there is a window here in which the text
    /// has been *sent* and not *written*, and a buffer that looked saved during
    /// it would be lying about a file on another machine.
    fn save(&mut self, editor: &mut Editor, path: tramp::RemotePath) {
        let text = editor.buffer.text.to_string();
        let job = Job::Save {
            buffer: editor.buffer.id,
            at: editor.buffer.change_count(),
            path: path.clone(),
            bytes: text.len(),
        };
        self.request(tramp::Op::Write(path.clone(), text.into_bytes()), job);
        editor.apply(EditorCommand::Message(format!("writing {path}…")));
    }

    /// Everything that finished since the last frame.
    ///
    /// Drained until empty rather than one per frame: every reply here matters,
    /// and one of them is a save. Costs a `try_recv` on an empty channel in a
    /// session that never typed `/ssh:` — and not even that, since there is no
    /// channel until there is a worker.
    fn poll(&mut self, editor: &mut Editor, dired: &mut Dired) {
        while let Some((id, reply)) = self.worker.as_ref().and_then(tramp::Worker::poll) {
            let Some(job) = self.jobs.remove(&id) else {
                continue;
            };
            match job {
                // dired's verbs report as a *set* — "deleted 3, failed 1" is
                // one sentence about however many round trips it took — so a
                // failure goes to its tally rather than to the status line on
                // its own. Everything below has one answer and one message.
                Job::Dired(path) => dired.did(editor, &path, reply),
                job => match reply {
                    Ok(reply) => self.finish(editor, dired, job, reply),
                    // Errors are values. `tramp::Error`'s own words are better
                    // than anything this layer could write — they distinguish
                    // "add your key to the agent" from "accept the host key"
                    // from "cannot reach" — so they go to the status line
                    // unedited and the editor carries on. A failed save leaves
                    // the buffer modified, because it is.
                    Err(e) => {
                        editor.apply(EditorCommand::Message(format!("{}: {e}", job.path())));
                    }
                },
            }
        }
    }

    fn finish(&mut self, editor: &mut Editor, dired: &mut Dired, job: Job, reply: tramp::Reply) {
        match (job, reply) {
            // A directory: dired's, and the listing is the second round trip.
            (Job::Probe(path), tramp::Reply::Stat(Some(meta))) if meta.is_dir => {
                dired.adopt_remote(&path);
                self.list(path);
            }
            (Job::Probe(path), tramp::Reply::Stat(meta @ Some(_))) => {
                self.request(tramp::Op::Read(path.clone()), Job::Open(path, meta));
            }
            // Not there. Emacs visits the name in an empty buffer, and so do we:
            // it is the only way to *create* a remote file, and the save that
            // follows is the same save as any other.
            (Job::Probe(path), tramp::Reply::Stat(None)) => {
                visit(editor, &path, String::new(), None);
                editor.apply(EditorCommand::Message(format!("{path} (new file)")));
            }
            (Job::Open(path, meta), tramp::Reply::Read(bytes)) => match String::from_utf8(bytes) {
                Ok(text) => visit(editor, &path, text, meta),
                // Same refusal as `tramp::read_to_string`, and for the reason
                // given there: replacing undecodable bytes with U+FFFD in a
                // buffer somebody will save destroys the file.
                Err(_) => editor.apply(EditorCommand::Message(format!(
                    "{path} is not UTF-8 text"
                ))),
            },
            (Job::List(path), tramp::Reply::List(entries)) => dired.listed(editor, &path, entries),
            (
                Job::Save {
                    buffer,
                    at,
                    path,
                    bytes,
                },
                tramp::Reply::Done,
            ) => {
                let name = PathBuf::from(path.to_string());
                let mut named = false;
                if let Some(b) = editor.buffer_by_id_mut(buffer) {
                    // Only the text that went out is on the host. Anything
                    // typed since is not, and the buffer has to keep saying so.
                    if b.change_count() == at {
                        b.modified = false;
                        // ...and only then is the recovery copy stale. Cleared
                        // by name rather than through `autosave_forget`,
                        // because that reads a buffer we are already holding.
                        if let Some(copy) = autosave_file(&name) {
                            let _ = std::fs::remove_file(copy);
                        }
                    }
                    if b.path.is_none() {
                        b.language = zemacs_syntax::language_for_path(&name);
                        b.path = Some(name);
                        named = true;
                    }
                }
                // The text did not change but the *language* did, and the
                // highlight request is gated on the revision — the same reason
                // the local `save_file` bumps it.
                if named {
                    editor.revision += 1;
                }
                editor.apply(EditorCommand::Message(format!("wrote {path} ({bytes} bytes)")));
            }
            // `execute` answers each `Op` with its own `Reply` variant, so this
            // is a bug in this file rather than anything a host can cause.
            (job, reply) => editor.apply(EditorCommand::Message(format!(
                "tramp: {} answered with {reply:?}",
                job.path()
            ))),
        }
    }
}

/// A remote file's text, in a buffer.
///
/// The `Ok` arm of [`open_file`], minus the parts that mean nothing off this
/// machine and plus the two the `stat` already paid for.
fn visit(
    editor: &mut Editor,
    path: &tramp::RemotePath,
    text: String,
    meta: Option<tramp::RemoteEntry>,
) {
    // The tramp name *is* the buffer's path, so it survives into the modeline,
    // into `:w`, into dired's `locate`, and back through `tramp::parse` —
    // `Display` is an exact inverse of `parse`, which is what makes that safe.
    let name = PathBuf::from(path.to_string());
    let lang = zemacs_syntax::language_for_path(&name);
    editor.load(&text, Some(name.clone()), lang);
    editor.buffer.file_mode = meta.as_ref().map(|m| m.mode);
    // Left `None` deliberately: `visited` is only ever compared against
    // `disk_stamp`, which is a local `stat`, and a remote one would be a round
    // trip per save. See the header.
    editor.buffer.visited = None;
    let recovery = meta
        .as_ref()
        .and_then(|m| m.modified)
        .and_then(|on_host| recovery_against(&name, on_host));
    editor.apply(EditorCommand::Message(match recovery {
        Some(copy) => format!("opened {path} — newer auto-save at {}", copy.display()),
        None => format!("opened {path} ({} bytes)", text.len()),
    }));
    remember_recent(&name);
}

/// Colour the documentation panel beside the completion popup.
///
/// Mirrors [`refresh_file_completions`] exactly: core holds the state and
/// cannot fill this field in, because `zemacs-syntax` depends on core rather
/// than the other way round, and the renderer has no parser either. So the app
/// — the one layer that has both — does it, the same way it does the IO.
///
/// Highlighted as the *buffer's* language, which is right for the thing being
/// documented: a docstring for a Rust function is a Rust signature and some
/// prose, and the prose falls out of the parse uncoloured, which the renderer
/// draws in the comment face. A buffer with no language leaves the spans empty
/// and gets plain text.
///
/// Runs once per doc change and not per frame: `CompletionEdit::Doc` clears the
/// spans, and a non-empty list is the flag that says this has already been
/// done. ponytail: on the main thread rather than through the highlighter's
/// worker, because a docstring is a dozen lines against a buffer's thousands.
/// The upgrade path is the same `request`/`poll` pair, if a server ever sends a
/// page of prose.
fn highlight_completion_doc(editor: &mut Editor) {
    let Some(doc) = editor.completion_doc_to_colour() else {
        return;
    };
    let text = doc.join("\n");
    // No language means nothing to parse *with*, and the empty span list is
    // still recorded: it says "parsed, nothing to colour", which is what stops
    // this running again on the next frame.
    let spans = match &editor.buffer.language {
        Some(lang) => zemacs_syntax::highlight(lang, &text),
        None => Vec::new(),
    };
    editor.set_completion_doc_spans(spans);
}

// --- file completion -----------------------------------------------------

/// Feed directory listings to an open find-file prompt.
///
/// This lives here rather than in the prompt itself because core does no IO.
/// The listing is refreshed only when the *directory* part of the typed path
/// changes — the filename part is what the fuzzy matcher filters on, so
/// re-reading the directory per keystroke would be pure waste.
///
/// A `bare` prompt is one asking for a *name* — dired's `+` and `C-c n`, which
/// create in the directory on screen. Listing the filesystem into it is the
/// whole bug those two had: `Prompt::value` answers with the highlighted
/// candidate, so `+` typed `notes` and submitted a path, which
/// `zemacs_dired::child` then refused for having a separator in it.
fn refresh_file_completions(editor: &mut Editor, last_query: &mut Option<String>) {
    let Some(prompt) = editor.prompt.as_mut() else {
        *last_query = None;
        return;
    };
    if prompt.kind != PromptKind::File || prompt.bare {
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
            // The name is the label and the directory is the hint, which is the
            // split the eye actually wants: you recognise a file by its name,
            // and you only need the path to tell two `mod.rs` apart. Whole path
            // in the label made every row a different length and buried the one
            // distinguishing word at the far end of it.
            label: p
                .file_name()
                .map_or_else(|| display_path(&p), |n| n.to_string_lossy().into_owned()),
            action: format!("open:{}", p.display()),
            hint: p.parent().map(display_path).map_or_else(String::new, elide),
        })
        .collect();
    editor.dashboard.footer = format!(
        "{} · {backend} · j/k to move, RET to open",
        display_path(init_path)
    );
}

/// Longest directory the dashboard will set in its hint column, in characters.
/// The block is as wide as its widest row, so one deeply-nested recent file
/// would otherwise stretch the whole menu across the window.
const HINT_MAX: usize = 44;

/// `…` and the tail, when a path is too long. The tail rather than the head:
/// what distinguishes two paths is nearly always the end of them, and `/Users/
/// you/Code/some-org/…` names no directory at all.
fn elide(s: String) -> String {
    if s.chars().count() <= HINT_MAX {
        return s;
    }
    let tail: String = s
        .chars()
        .skip(s.chars().count() - (HINT_MAX - 1))
        .collect();
    format!("…{tail}")
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
    util: sdl3::clipboard::ClipboardUtil,
    /// The revision already pushed. Also updated after a *pull*, so adopting
    /// the clipboard's text does not read back as a register change and bounce
    /// straight out again.
    pushed: u64,
}

impl Clipboard {
    fn new(video: &sdl3::VideoSubsystem) -> Self {
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

    /// The clipboard's *image*, if it is holding one, as bytes and the
    /// extension to save them under.
    ///
    /// A coding agent takes screenshots — that is most of what you say to one
    /// about a UI — and the way every one of them takes it is a **path** typed
    /// into its prompt. So this reads the bytes and [`paste_image`] writes them
    /// somewhere the agent can open. Nothing here decodes the image; the editor
    /// has a decoder (`crates/figure`) and no reason to run it on the way past.
    ///
    /// Not in `sdl3`'s safe wrapper, which stops at text — so this is the raw
    /// call, and the two rules that come with it are that the buffer is SDL's
    /// to free and that it must be called on the main thread. Both hold here:
    /// this is the loop's own thread and the copy is made before the free.
    ///
    /// PNG first because it is what a screenshot is on every platform this
    /// runs on, and because it is lossless — a JPEG re-save of a screenshot is
    /// a screenshot with artefacts around the text.
    fn image(&self) -> Option<(Vec<u8>, &'static str)> {
        // Both spellings of each type, because SDL passes the name straight to
        // the platform's clipboard and the platforms do not agree on what a
        // picture is called. macOS stores *uniform type identifiers* —
        // `public.png` — and it is Finder and the screenshot key that put them
        // there, so asking only for the MIME name finds an empty clipboard on
        // the one platform this gesture matters most on. X11 and Wayland use
        // the MIME name.
        for (mime, ext) in [
            ("image/png", "png"),
            ("public.png", "png"),
            ("image/jpeg", "jpg"),
            ("public.jpeg", "jpg"),
            ("image/gif", "gif"),
            ("com.compuserve.gif", "gif"),
            ("image/bmp", "bmp"),
            // TIFF last, and only as a fallback: macOS puts one on the
            // pasteboard beside almost every image, and it is the format an
            // agent is least likely to take. Anything above this is preferred
            // precisely because something else is holding the same picture.
            ("image/tiff", "tiff"),
            ("public.tiff", "tiff"),
        ] {
            let name = std::ffi::CString::new(mime).ok()?;
            // SAFETY: `name` outlives the call, `size` is written before the
            // pointer is read, and the buffer is copied and then freed exactly
            // once. A NULL is "no data of that type", which is the common case.
            let bytes = unsafe {
                let mut size: usize = 0;
                let data = sdl3::sys::clipboard::SDL_GetClipboardData(name.as_ptr(), &mut size);
                if data.is_null() || size == 0 {
                    continue;
                }
                let copy = std::slice::from_raw_parts(data as *const u8, size).to_vec();
                sdl3::sys::stdinc::SDL_free(data);
                copy
            };
            return Some((bytes, ext));
        }
        None
    }
}

/// Where a pasted image goes. Beside the auto-saves, and not in the project:
/// the same argument `autosave_dir` makes, one step further — a screenshot
/// dropped into an agent is scratch, and a tree that grows `Screenshot 3.png`
/// every time you describe a bug is a tree with a `.gitignore` problem.
fn image_dir() -> Option<PathBuf> {
    let dir = config_dir()?.join("images");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// The clipboard's image when SDL cannot reach it, which on a Mac is always.
///
/// SDL3 keeps its own record of what is on the clipboard and answers
/// `SDL_GetClipboardData` out of it, so it can hand back a picture *this*
/// process copied and nothing else — and the picture that matters here was put
/// there by Preview, by Finder, or by ⌘⇧4. The pasteboard genuinely holds it
/// (`osascript -e 'clipboard info'` lists PNG, TIFF, GIF, JPEG and BMP for a
/// screenshot); SDL simply has no route to another application's data.
///
/// So: the platform's own tool, which is the same answer this app already gives
/// for ripgrep and for git. AppleScript writes the PNG straight out rather than
/// handing back hex for us to decode, and a clipboard with no picture in it
/// makes the script fail, which is the `None` this wants.
#[cfg(target_os = "macos")]
fn platform_clipboard_image() -> Option<(Vec<u8>, &'static str)> {
    let tmp = std::env::temp_dir().join(format!("zemacs-clipboard-{}.png", std::process::id()));
    let out = std::process::Command::new("osascript")
        .arg("-e")
        .arg(format!(
            "set f to open for access POSIX file \"{}\" with write permission",
            tmp.display()
        ))
        .arg("-e")
        .arg("set eof f to 0")
        // PNG and not TIFF: macOS puts both on the pasteboard for almost every
        // picture, and TIFF is the one an agent is least likely to accept.
        .arg("-e")
        .arg("write (the clipboard as «class PNGf») to f")
        .arg("-e")
        .arg("close access f")
        .output()
        .ok()?;
    let bytes = std::fs::read(&tmp).ok();
    let _ = std::fs::remove_file(&tmp);
    match out.status.success() {
        true => bytes.filter(|b| !b.is_empty()).map(|b| (b, "png")),
        false => None,
    }
}

/// The same on Linux, where the tool depends on the display server. Both are
/// asked for `image/png` by name and both print it to stdout.
#[cfg(all(unix, not(target_os = "macos")))]
fn platform_clipboard_image() -> Option<(Vec<u8>, &'static str)> {
    for (program, args) in [
        ("wl-paste", vec!["--no-newline", "--type", "image/png"]),
        ("xclip", vec!["-selection", "clipboard", "-t", "image/png", "-o"]),
    ] {
        let Ok(out) = std::process::Command::new(program).args(&args).output() else {
            continue;
        };
        if out.status.success() && !out.stdout.is_empty() {
            return Some((out.stdout, "png"));
        }
    }
    None
}

#[cfg(not(unix))]
fn platform_clipboard_image() -> Option<(Vec<u8>, &'static str)> {
    None
}

/// Write the clipboard's image out and answer where it went.
///
/// Named by *content*, so pasting the same screenshot twice is one file: the
/// directory is a cache rather than a log, and an agent asked about the same
/// picture twice should be given the same path.
// ponytail: nothing ever evicts. A screenshot is a few hundred kilobytes and
// the naming collapses repeats, so this grows by what you actually showed an
// agent — but it grows. The upgrade is the sweep `autosave_dir` will want too:
// drop anything older than a month on the way past.
fn write_clipboard_image(clipboard: &Clipboard) -> Option<PathBuf> {
    let (bytes, ext) = clipboard.image().or_else(platform_clipboard_image)?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&bytes, &mut hash);
    let name = format!("{:016x}.{ext}", std::hash::Hasher::finish(&hash));
    let path = image_dir()?.join(name);
    if !path.exists() {
        std::fs::write(&path, &bytes).ok()?;
    }
    Some(path)
}

// --- auto-save -----------------------------------------------------------
//
// Emacs writes `#foo.rs#` beside the file; this writes into
// `~/.zemacs.d/auto-save/` instead, so a project tree never grows litter
// that its `.gitignore` has to know about. The name is the absolute path with
// `/` turned into `!` — Emacs' own mangling, and the reason it is recoverable
// by eye when you need to go looking.

const AUTOSAVE_EVERY: Duration = Duration::from_secs(30);

/// How long a frame may go undrawn when [`Editor::generation`] says nothing has
/// changed.
///
/// The safety net under the skip, and not a frame clock: nothing the user does
/// waits for it, because everything the user does moves the generation. What it
/// bounds is the cost of a *missing* `touch` — a writer added later in this
/// layer that reaches around `apply` and forgets to say so. Half a second of a
/// stale pane is a lag somebody notices and reports; a pane that silently stops
/// updating is the bug nobody can describe.
///
/// Two draws a second while idle, against sixty. The draw is the couple of
/// milliseconds `App::draw` documents, so this is the difference between an
/// editor that costs a seventh of a core doing nothing and one that costs about
/// half a percent.
const DRAW_AT_LEAST: Duration = Duration::from_millis(500);

fn autosave_dir() -> Option<PathBuf> {
    Some(config_dir()?.join("auto-save"))
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
fn autosave_all(editor: &Editor) {
    // `sync_focused_window` has not run yet this iteration, but auto-save only
    // reads text and path, and neither is window state — which is why this
    // takes the editor by reference at all.
    for buffer in editor.buffers() {
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
    recovery_against(path, std::fs::metadata(path).ok()?.modified().ok()?)
}

/// The half of [`recovery_for`] that does not do the `stat`.
///
/// Split out for a remote file, whose mtime arrived over ssh with the rest of
/// its metadata — asking this machine about it answers `None`, which is how a
/// remote buffer came to auto-save faithfully and never offer the copy back.
fn recovery_against(path: &Path, on_disk: std::time::SystemTime) -> Option<PathBuf> {
    let copy = autosave_file(path)?;
    let saved = std::fs::metadata(&copy).ok()?.modified().ok()?;
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

// --- writing a file ------------------------------------------------------
//
// Two separate promises, and they fail in different ways, which is why they are
// two mechanisms rather than one.
//
// **The write must not tear.** `fs::write` truncates the target and then fills
// it, so a crash, a full disk or a killed process in between leaves a file that
// is neither the old text nor the new one. Auto-save does not cover this: it
// copies the *buffer* aside, and the thing that just became rubble is the file.
// So the text goes to a sibling temp file, is flushed to the platter, and is
// then `rename`d over the target — atomic within a filesystem, which reduces
// the outcomes to "the old file" or "the new file" and deletes "half of either"
// from the list.
//
// **The previous saved version must survive.** That is a different question —
// nothing tore, you simply want back what was there before — and no amount of
// care during the write answers it. Hence numbered backups, taken before the
// rename.

/// The file's modification time, or `None` if it cannot be stat'd. What
/// [`zemacs_core::Buffer::visited`] is compared against.
fn disk_stamp(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Write `text` to `target` so that a crash can never leave it half-written.
///
/// ponytail: the directory entry is not itself fsync'd after the rename, so a
/// power cut in the instant after a save can still lose the *whole* save — but
/// never half of one, which is the property being bought here. `File::sync_all`
/// on the parent directory is the upgrade, and it costs a second syscall per
/// save. ponytail: `rename` also breaks a hard link, where Emacs would copy —
/// rare enough to name rather than solve, and the fix is
/// `backup-by-copying-when-linked`'s: write in place when `st_nlink > 1`.
fn write_file(target: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;

    // Follow a symlink rather than replacing it. `rename` swaps a *name*, so
    // without this a save over `~/.zshrc -> dotfiles/zshrc` would leave a real
    // file where the link had been and the repository untouched — the exact
    // opposite of the reason that link exists. `canonicalize` needs the file to
    // be there, so a file being created keeps the name it was given.
    let target = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    // Beside the target, never in a temp directory: `rename` is only atomic
    // within one filesystem, and `/tmp` is routinely a different one — which
    // would silently turn this back into a copy that can tear.
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    // The pid keeps two zemacs processes saving the same file out of each
    // other's way. Two *frames* of one process cannot collide: saving happens
    // on the main thread, one command at a time.
    let tmp = dir.join(format!(".#{name}.zemacs{}#", std::process::id()));

    let write = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        // The whole point. Without it the rename can reach the disk before the
        // bytes do, and a power cut leaves an empty file under the right name —
        // which is worse than a torn one, because it looks fine.
        file.sync_all()?;
        // A fresh file is created at the umask's mercy, so a script's
        // executable bit — and anything else the file's own mode said — has to
        // be put back by hand. Nothing to restore for a file being created.
        if let Ok(meta) = std::fs::metadata(&target) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, &target)
    };
    let outcome = write();
    if outcome.is_err() {
        // The target has not been touched at this point — the rename is last —
        // so all that is left is not to litter the directory.
        let _ = std::fs::remove_file(&tmp);
    }
    outcome
}

/// How many past versions of a file to keep. The config keeps 20; so does this.
///
/// ponytail: a copy per *save*, not per session as Emacs does — so the twenty
/// are your last twenty saves rather than twenty points from whenever each
/// buffer was first written. Strictly the more useful of the two, and it costs
/// twenty times the file on disk. The condition for changing it is a real one:
/// if this is ever pointed at files big enough for that to matter, the fix is
/// Emacs' `buffer-backed-up` flag, not a smaller number.
const BACKUP_KEEP: usize = 20;

/// In `~/.zemacs.d/backup/` and not beside the file, for the reason
/// auto-save is not beside it either: a project tree must not grow litter that
/// its `.gitignore` has to know about. Same `!`-mangled naming, so the two
/// directories read the same way when you go looking by eye.
fn backup_dir() -> Option<PathBuf> {
    Some(config_dir()?.join("backup"))
}

/// Copy the current contents of `target` aside as the next numbered version.
///
/// Silent on every failure, like auto-save and `remember_recent`: the save
/// itself is about to report whether *it* worked, and a second line about the
/// backup directory in front of someone mid-sentence is noise. A file being
/// created has nothing to preserve and takes the same quiet path.
fn backup(target: &Path) {
    if let Some(dir) = backup_dir() {
        backup_into(&dir, target);
    }
}

/// The half of [`backup`] that does not need to know where backups live, which
/// is what lets a test point it at a directory of its own instead of at `$HOME`.
fn backup_into(dir: &Path, target: &Path) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let stem = format!("#{}#", target.to_string_lossy().replace('/', "!"));
    // Versions of *this* file, by number. Read fresh each save rather than
    // counted in memory, so the numbering survives a restart and a directory
    // somebody has pruned by hand.
    let mut versions: Vec<u32> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix(&stem)?
                .strip_prefix(".~")?
                .strip_suffix('~')?
                .parse()
                .ok()
        })
        .collect();
    versions.sort_unstable();

    let next = versions.last().copied().unwrap_or(0) + 1;
    if std::fs::copy(target, dir.join(format!("{stem}.~{next}~"))).is_err() {
        return;
    }
    // Oldest first, so a file saved for years keeps its last twenty saves
    // rather than the first twenty it ever had.
    let excess = (versions.len() + 1).saturating_sub(BACKUP_KEEP);
    for old in versions.into_iter().take(excess) {
        let _ = std::fs::remove_file(dir.join(format!("{stem}.~{old}~")));
    }
}

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
        let watched: Vec<(zemacs_core::BufferId, PathBuf)> = editor.buffers()
            .filter(|b| !b.kind.is_generated())
            .filter_map(|b| b.path.clone().map(|p| (b.id, p)))
            // The condition this header named, arrived. A remote buffer's
            // `stat` is an ssh round trip, and this sweep is four a second per
            // open file — so a remote file is not swept at all, and one that
            // moves under you is noticed when you next open it. ponytail: the
            // upgrade is a second, much slower timer going through the tramp
            // worker, where the answer arrives as a reply rather than as a
            // syscall — which is a different `check` and not a longer interval
            // on this one.
            .filter(|(_, p)| tramp::parse(&p.to_string_lossy()).is_none())
            .collect();

        for (id, path) in watched {
            self.check(editor, id, &path);
        }

        // Forget files nothing has open any more, so a long session's map does
        // not grow by every file ever visited. Cheap: it is the same length as
        // the sweep that just ran.
        self.seen.retain(|path, _| {
            editor.buffers().any(|b| b.path.as_deref() == Some(path.as_path()))
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
        // Saving over the newer file afterwards is what `save_file`'s own guard
        // asks about — this message and that question are the two halves of the
        // same event, and the stamp deliberately left stale here is what the
        // question is asked from.
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
            // Nothing moved, so the buffer is still in sync and the save prompt
            // has nothing to warn about. Said here as well as in `revert`
            // because `touch` reaches this arm and not that one, and a stamp
            // left stale would ask "changed on disk — save anyway?" about a
            // change that did not alter a byte.
            let stamp = disk_stamp(path);
            if let Some(buffer) = editor.buffer_by_id_mut(id) {
                buffer.visited = stamp;
            }
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
    // And so does the stamp: this buffer has just taken the file's own text, so
    // it is back in sync and a later `:w` has nothing to ask about. Without
    // this, every auto-revert would arm the save prompt for the next save.
    let stamp = disk_stamp(path);
    if let Some(buffer) = editor.buffer_by_id_mut(id) {
        buffer.file_mode = mode;
        buffer.visited = stamp;
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
        // A remote name cannot be checked without an ssh round trip, and the
        // dashboard is drawn before the first frame — so it is trusted, and the
        // open reports if the host or the file has gone. Local entries keep
        // being pruned, which is what stops the list filling with deleted files.
        .filter(|p| p.exists() || tramp::parse(&p.to_string_lossy()).is_some())
        .collect()
}

/// Most-recent-first, deduplicated, capped. Failures are silent: a missing
/// recents file must never get between the user and their editor.
fn remember_recent(path: &Path) {
    let Some(file) = recent_path() else { return };
    // A tramp name is already absolute and already exact — `Display` is the
    // inverse of `parse` — and `canonicalize` would ask *this* machine about a
    // file on another one, fail, and drop it. Which is why remote files reached
    // this list exactly never before the check was here.
    let canonical = match tramp::parse(&path.to_string_lossy()) {
        Some(_) => path.to_path_buf(),
        None => match path.canonicalize() {
            Ok(p) => p,
            Err(_) => return,
        },
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

// --- scenes ----------------------------------------------------------------

/// Put the focused pane's scene at `offset` pixels, clamped to what there is to
/// scroll: `[0, content_height - viewport]`.
///
/// The one place this side writes a scroll offset, and a function rather than
/// two lines at the wheel because there are three writers and one rule. The
/// wheel is the first. The scroll-into-view a curriculum's "next problem" needs
/// is the second. The third is the swap, and it is called once a frame from the
/// main loop: core carries the outgoing scene's offset onto the incoming one so
/// that a document re-rendering under the reader does not throw them back to the
/// top, and core *cannot* clamp it — clamping needs a height only a laid-out
/// scene has, and core cannot measure a font. So a `Scene::scroll` core wrote is
/// a request rather than a position, and stays one until this has run.
///
/// The layout comes from the renderer's cache, so the three callers between them
/// cost one relayout per change rather than one per call.
fn scroll_scene(editor: &mut Editor, renderer: &mut Renderer, frame_index: usize, offset: i32) {
    let Some(limit) = renderer
        .scene_layout(editor, frame_index)
        .map(|(l, v)| (l.content_height() - v.h).max(0))
    else {
        return;
    };
    if let Some(scene) = editor.buffer.scene.as_mut() {
        scene.scroll = offset.clamp(0, limit);
    }
}

// --- input translation ---------------------------------------------------

/// The cell a pixel lands on, relative to the pane showing the terminal, and
/// whether it landed in that cell's right half.
///
/// The terminal thinks in cells and knows nothing about panes or HiDPI, so this
/// is where those stop being its problem. The half is there for one caller: a
/// *selection* has to know whether the character under the press is inside it,
/// which is the difference between dragging out `hello` and `ello`. A mouse
/// report names a cell and has no use for it.
fn cell_at(editor: &Editor, renderers: &[Renderer], x: i32, y: i32) -> (usize, usize, bool) {
    let index = editor.focus_frame.min(renderers.len().saturating_sub(1));
    let Some(renderer) = renderers.get(index) else {
        return (0, 0, false);
    };
    let (cell_w, line_h) = renderer.cell_size();
    // The mouse only ever reaches the session whose buffer is live, so that is
    // the pane the pixels are relative to.
    let (rect, _) = terminal_rect(editor, renderers, editor.buffer.id);
    let cell_w = cell_w.max(1);
    let dx = (x - rect.x).max(0);
    let col = (dx / cell_w) as usize;
    let row = ((y - rect.y).max(0) / line_h.max(1)) as usize;
    (col, row, dx % cell_w >= cell_w / 2)
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
    // The renderer's own arithmetic, not a second copy of it: this used to
    // model the modeline as one text row and ignore the document padding, so
    // the child was told it had a row and a column that the draw loop then
    // refused to draw — which is why the bottom line of a shell was never on
    // the screen.
    renderer.terminal_grid(&editor.settings, rect)
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
    // but `C-<ret>` and `C-M-<ret>` are the window splits and `M-<ret>` is org's
    // "another one of these". Without the third arm `⌘⏎` fell through to the
    // `Meta(char)` case below, which has no character to make and answered
    // `None`, so the keystroke reached nothing at all.
    //
    // Shift joins them for the same reason one step later: org's `M-S-<ret>` is
    // "another one of these, but a task", and a modifier the vocabulary cannot
    // spell is a binding nobody can write. Ctrl still ignores it — `C-<ret>` and
    // `C-⇧<ret>` are one key, as `C-a` and `C-A` are.
    if matches!(kc, Keycode::Return | Keycode::KpEnter) {
        match (ctrl, meta, shift) {
            (true, true, _) => return Some(Key::CtrlMetaEnter),
            (true, false, _) => return Some(Key::CtrlEnter),
            (false, true, true) => return Some(Key::MetaShiftEnter),
            (false, true, false) => return Some(Key::MetaEnter),
            (false, false, true) => return Some(Key::ShiftEnter),
            _ => {}
        }
    }
    // Same reason, for `⌘⌫` — kill the word before point.
    if kc == Keycode::Backspace && meta {
        return Some(Key::MetaBackspace);
    }
    // ...and for the modified arrows. Named keys, so `combo_char` below cannot
    // spell them and they would otherwise be dropped on the floor — which is
    // exactly what `⌘←` used to do.
    //
    // One arm per key that exists rather than a table over both modifiers:
    // `M-<up>`/`M-<down>` and `M-S-<up>`/`M-S-<down>` are the four this
    // deliberately does *not* name, since a key nothing binds is a match arm in
    // four crates for nothing. Ctrl is excluded from the whole block, as above.
    if !ctrl {
        let modified = match (kc, meta, shift) {
            (Keycode::Left, true, false) => Some(Key::MetaLeft),
            (Keycode::Right, true, false) => Some(Key::MetaRight),
            (Keycode::Left, true, true) => Some(Key::MetaShiftLeft),
            (Keycode::Right, true, true) => Some(Key::MetaShiftRight),
            (Keycode::Left, false, true) => Some(Key::ShiftLeft),
            (Keycode::Right, false, true) => Some(Key::ShiftRight),
            (Keycode::Up, false, true) => Some(Key::ShiftUp),
            (Keycode::Down, false, true) => Some(Key::ShiftDown),
            _ => None,
        };
        if modified.is_some() {
            return modified;
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
    // F1–F12, by position rather than by twelve match arms below. `combo_char`
    // cannot spell `"F1"` any more than it can spell `"Left"`, so before this
    // every F-key answered `None` and no config could bind one at all.
    const FN_KEYS: [Keycode; 12] = [
        Keycode::F1,
        Keycode::F2,
        Keycode::F3,
        Keycode::F4,
        Keycode::F5,
        Keycode::F6,
        Keycode::F7,
        Keycode::F8,
        Keycode::F9,
        Keycode::F10,
        Keycode::F11,
        Keycode::F12,
    ];
    if let Some(i) = FN_KEYS.iter().position(|&f| f == kc) {
        return Some(Key::F(i as u8 + 1));
    }
    match kc {
        Keycode::Escape => Some(Key::Esc),
        Keycode::Return | Keycode::KpEnter => Some(Key::Enter),
        Keycode::Backspace => Some(Key::Backspace),
        // Shift is consulted here and nowhere else among the named keys: `⇧⇥`
        // is not a shifted Tab but a key of its own, and a shell, a pager and
        // an agent's input box all read it as "backwards". Discarding the bit
        // meant `⇧⇥` typed a plain tab into whatever was running.
        Keycode::Tab if shift => Some(Key::BackTab),
        Keycode::Tab => Some(Key::Tab),
        Keycode::Left => Some(Key::Left),
        Keycode::Right => Some(Key::Right),
        Keycode::Up => Some(Key::Up),
        Keycode::Down => Some(Key::Down),
        // The block above them, dropped on the floor until now for the arrows'
        // reason — a name `combo_char` cannot spell. Worst in a shell, where
        // Home and End are readline's line-start and line-end, the page keys
        // move a full-screen TUI and `⌦` is forward-delete.
        Keycode::Home => Some(Key::Home),
        Keycode::End => Some(Key::End),
        Keycode::PageUp => Some(Key::PageUp),
        Keycode::PageDown => Some(Key::PageDown),
        Keycode::Delete => Some(Key::Delete),
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

/// What `launchd` hands an application started from the Dock, the Finder or
/// Spotlight. Not a guess: it is the compiled-in default `_PATH_DEFPATH`, and
/// `launchctl getenv PATH` on a normal Mac prints nothing at all, which is how
/// you can tell nobody has overridden it.
const LAUNCHD_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// Take `$PATH` from a login shell when we plainly did not inherit one.
///
/// A GUI application on macOS does not get your shell environment. Started from
/// a terminal, zemacs inherits everything `~/.zshrc` set up; started from the
/// Dock it gets [`LAUNCHD_PATH`] and nothing else — so `claude`, `cargo`, `rg`
/// and anything else installed under `~/.local/bin` or Homebrew simply is not
/// there. The editor is then subtly and inexplicably less capable depending on
/// how it was launched, which is among the oldest complaints in Mac Emacs and is
/// what `exec-path-from-shell` exists to answer.
///
/// Done here, in the process environment, rather than in Lisp, because three
/// separate things have to agree about it and only this reaches all of them:
/// `executable-find` in `runtime/modes/ai.lisp` reads `$PATH` through ECL, the
/// `which` in `crates/term` reads it through Rust, and a shell or an agent on a
/// PTY inherits it from the process. One `set_var` before anything starts and
/// all three are right.
///
/// Only when the inherited `PATH` is exactly the launchd default. Overwriting a
/// `PATH` we really did inherit would throw away whatever the surrounding
/// terminal had added to it — a `direnv` shim, a virtualenv, a toolchain a
/// project put there on purpose — and those are deliberate in a way a login
/// shell's profile cannot know about.
///
/// Silent on every failure: a login shell that errors, hangs up or prints
/// nothing usable leaves the launchd `PATH` in place, which is exactly where we
/// already were. ponytail: no timeout on the subprocess, so a profile that
/// blocks forever hangs startup. `$SHELL -l -c` is what every editor on this
/// platform already does and a wedged profile hangs those too; the upgrade is a
/// thread and a channel, on the day someone's `.zprofile` actually does it.
/// Start in `$HOME` rather than wherever the launcher happened to leave us.
///
/// `inherit_login_path`'s sibling, and the same bug from the other end: an
/// application bundle opened from the Dock or from Finder inherits `/` as its
/// working directory, so `SPC f f` opened on the root of the disk and a
/// terminal session started there. Nobody keeps their code in `/`.
///
/// Only from `/`, for the reason the `PATH` fix is conditional: a `zemacs`
/// typed at a shell is *in* a directory on purpose, and moving out of it would
/// be worse than the bug. `/` is the one cwd nothing chooses.
fn start_in_home() {
    if std::env::current_dir().is_ok_and(|d| d != Path::new("/")) {
        return;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let _ = std::env::set_current_dir(home);
    }
}

fn inherit_login_path() {
    if std::env::var("PATH").unwrap_or_default() != LAUNCHD_PATH {
        return;
    }
    let Some(shell) = std::env::var_os("SHELL") else {
        return;
    };
    // `-l` so the profile that sets the interesting parts of `PATH` is read at
    // all. `printf` and not `echo` because a `PATH` is not a line of text and
    // some shells' `echo` will happily mangle a backslash in one.
    let Ok(out) = std::process::Command::new(shell)
        .args(["-l", "-c", "printf %s \"$PATH\""])
        .output()
    else {
        return;
    };
    let Ok(out) = String::from_utf8(out.stdout) else {
        return;
    };
    if let Some(path) = usable_path(&out) {
        std::env::set_var("PATH", path);
    }
}

/// The `PATH` in a login shell's output, or `None` if it does not look like
/// one.
///
/// A profile that prints a banner, warns about a missing tool or asks a question
/// is the normal reason this is not a path, and the answer is to keep the one we
/// have. Deliberately not "take the last line": a `PATH` that arrived with
/// somebody's fortune cookie glued to the front of it is not a `PATH` to trust
/// the rest of.
fn usable_path(out: &str) -> Option<&str> {
    let path = out.trim();
    (path.contains('/') && !path.contains('\n')).then_some(path)
}

/// Everything zemacs keeps for you: the config, and the state it writes beside
/// it. `~/.zemacs.d`, the way Emacs spells `~/.emacs.d`, rather than under
/// `~/.config` — a Lisp machine's directory is a place you *live in* and open
/// files from, not a settings folder you visit twice a year, and Lisp on this
/// side of the boundary reaches it as `(zemacs-file "repl.lisp")`.
///
/// `None` only with no `$HOME`, which in practice means a build sandbox. Every
/// caller degrades to doing nothing rather than to writing somewhere else.
///
/// ponytail: no XDG fallback and no migration from the old `~/.config/zemacs`.
/// Moving the directory is a `mv`, saying so once beats a lookup order nobody
/// can predict, and a fallback that silently kept finding the old path would
/// make this change invisible — which is the one outcome it must not have.
pub fn config_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".zemacs.d"))
}

/// Where the *shipped* Lisp lives — `library.lisp`, `themes/`, `modes/`, the LSP
/// client. Distinct from [`config_dir`], and the distinction is the whole point:
/// your `init.lisp` lives in `~/.zemacs.d` and has none of that beside it, so
/// the image cannot find the runtime by looking next to the config it just read.
///
/// `$ZEMACS_RUNTIME` first, so one binary can be pointed at a checkout, an
/// install prefix or a bundle; the compiled-in path otherwise, so `cargo run`
/// works out of the box. Handed to the image through the environment — see
/// `PATHS_FORM` in `crates/lisp/src/shim.c`, which is the other half of this.
fn runtime_dir() -> PathBuf {
    if let Some(explicit) = std::env::var_os("ZEMACS_RUNTIME") {
        return PathBuf::from(explicit);
    }
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../runtime"))
}

/// Put your config where you can edit it, once, the first time zemacs runs.
///
/// The shipped `runtime/init.lisp` is a *default*, not the config: copying it to
/// `~/.zemacs.d/init.lisp` is what makes "edit your configuration" mean editing
/// a file that belongs to you rather than one inside a checkout that `git pull`
/// will overwrite. Only the config moves — the library, the themes and the modes
/// it calls stay in the runtime and are upgraded with the editor, which is why
/// this copy is a one-line file to keep rather than a fork of the whole runtime.
///
/// Never overwrites: an existing file is the answer, whatever is in it. Every
/// failure is silent and leaves the shipped copy in play — a read-only `$HOME`
/// is a reason to run with the defaults, not a reason not to start.
///
/// Deliberately *not* called from [`resolve_init_path`], which the test module
/// calls a dozen times: seeding there would have `cargo test` writing into the
/// developer's own home directory.
fn seed_user_init() {
    let Some(dir) = config_dir() else { return };
    let user = dir.join("init.lisp");
    if user.exists() {
        return;
    }
    let shipped = runtime_dir().join("init.lisp");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::copy(&shipped, &user);
    }
}

/// `$ZEMACS_INIT`, else `~/.zemacs.d/init.lisp` if it exists, else the copy
/// shipped in the repo — so a build sandbox with no `$HOME` still starts.
fn resolve_init_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("ZEMACS_INIT") {
        return PathBuf::from(explicit);
    }
    if let Some(user) = config_dir().map(|d| d.join("init.lisp")) {
        if user.exists() {
            return user;
        }
    }
    runtime_dir().join("init.lisp")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A login shell's output is not always a `PATH`. Asserted on the parsing
    /// rather than on the environment: `inherit_login_path` sets a process-wide
    /// variable and forks a shell, and a test doing either would be deciding
    /// what `$PATH` is for every other test in the binary.
    #[test]
    fn only_something_that_looks_like_a_path_replaces_the_one_we_have() {
        assert_eq!(
            usable_path("/opt/homebrew/bin:/usr/bin\n"),
            Some("/opt/homebrew/bin:/usr/bin"),
        );
        // A profile that greets you. The path is in there, and taking it would
        // mean trusting the rest of a shell that is clearly doing other things.
        assert_eq!(usable_path("Welcome!\n/usr/bin:/bin\n"), None);
        // Nothing at all: a shell that failed, or one with no profile.
        assert_eq!(usable_path(""), None);
        assert_eq!(usable_path("   \n"), None);
        // Output with no separator in it is a message, not a path.
        assert_eq!(usable_path("command not found"), None);
    }

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
            (Keycode::_4, '4'),
        ] {
            assert_eq!(
                key_from_keydown(kc, Mod::NOMOD, true),
                Some(Key::Char(want)),
                "{kc:?} should produce {want} with text input off"
            );
        }
        // shifted punctuation and letters, so `$`, `:` and `ZZ` still work
        assert_eq!(
            key_from_keydown(Keycode::_4, Mod::LSHIFTMOD, true),
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

    /// `⇧⇥` is the one named key that reads its shift bit. Both raw states,
    /// because Insert mode takes the other path and an agent's input box is
    /// typed into exactly like one.
    #[test]
    fn shift_tab_is_backtab_and_plain_tab_is_not() {
        for raw in [true, false] {
            assert_eq!(
                key_from_keydown(Keycode::Tab, Mod::LSHIFTMOD, raw),
                Some(Key::BackTab)
            );
            assert_eq!(
                key_from_keydown(Keycode::Tab, Mod::RSHIFTMOD, raw),
                Some(Key::BackTab)
            );
            assert_eq!(key_from_keydown(Keycode::Tab, Mod::NOMOD, raw), Some(Key::Tab));
        }
    }

    /// The block above the arrows, every one of which used to arrive here and
    /// leave as `None`: their key names are words, so `combo_char` could not
    /// spell them and nothing downstream ever saw the keystroke. Both raw
    /// states, because none of them produces text and Insert takes the other
    /// path — a shell reached through Insert must still get its Home key.
    #[test]
    fn the_navigation_block_survives_the_keydown() {
        for raw in [true, false] {
            let m = |kc| key_from_keydown(kc, Mod::NOMOD, raw);
            assert_eq!(m(Keycode::Home), Some(Key::Home));
            assert_eq!(m(Keycode::End), Some(Key::End));
            assert_eq!(m(Keycode::PageUp), Some(Key::PageUp));
            assert_eq!(m(Keycode::PageDown), Some(Key::PageDown));
            assert_eq!(m(Keycode::Delete), Some(Key::Delete));
            // Numbered from one, and the table is in order — an off-by-one here
            // is a config binding `<f5>` and getting F6.
            assert_eq!(m(Keycode::F1), Some(Key::F(1)));
            assert_eq!(m(Keycode::F5), Some(Key::F(5)));
            assert_eq!(m(Keycode::F12), Some(Key::F(12)));
            // Forward delete is not Backspace, whatever the key is labelled.
            assert_ne!(m(Keycode::Delete), m(Keycode::Backspace));
        }
    }

    /// Shift on the keys that have no character to carry it. Without these the
    /// keystroke reached nothing at all: `combo_char` cannot spell `<ret>` or an
    /// arrow, so org's `M-S-<ret>` was a binding that could not be written down.
    #[test]
    fn shift_is_a_modifier_on_the_named_keys() {
        let m = |kc, keymod| key_from_keydown(kc, keymod, false);
        assert_eq!(m(Keycode::Return, Mod::LSHIFTMOD), Some(Key::ShiftEnter));
        assert_eq!(
            m(Keycode::Return, Mod::LGUIMOD | Mod::LSHIFTMOD),
            Some(Key::MetaShiftEnter)
        );
        assert_eq!(m(Keycode::Left, Mod::RSHIFTMOD), Some(Key::ShiftLeft));
        assert_eq!(m(Keycode::Up, Mod::LSHIFTMOD), Some(Key::ShiftUp));
        assert_eq!(
            m(Keycode::Right, Mod::LGUIMOD | Mod::LSHIFTMOD),
            Some(Key::MetaShiftRight)
        );
        // Ctrl still swallows the bit, so `C-<ret>` splits a window however the
        // hand that pressed it was holding shift.
        assert_eq!(
            m(Keycode::Return, Mod::LCTRLMOD | Mod::LSHIFTMOD),
            Some(Key::CtrlEnter)
        );
        // ...and the unshifted keys are untouched.
        assert_eq!(m(Keycode::Left, Mod::LGUIMOD), Some(Key::MetaLeft));
        assert_eq!(m(Keycode::Left, Mod::NOMOD), Some(Key::Left));
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
        open_file(&mut ed, Path::new("@init"), &init, &mut Remote::default());
        // Either it loaded the config or it reported why; it must never be the
        // literal path "@init".
        assert_ne!(ed.buffer.path.as_deref(), Some(Path::new("@init")));
    }

    #[test]
    fn file_prompt_completes_from_the_filesystem() {
        let dir = std::env::temp_dir().join(fixture("completion"));
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
        let dir = std::env::temp_dir().join(fixture("hidden"));
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

    /// A fixture directory nobody else is writing into.
    ///
    /// The process id is the whole of it, and it is not paranoia: these tests
    /// used to build `$TMPDIR/zemacs_revert_test/unsaved.txt` at a path fixed at
    /// compile time, so a second `cargo test` running at the same time — two
    /// agents, a watch loop, a CI shard — wrote the same files under the same
    /// names while the first was asserting about them. The failures that
    /// produced were spectacular and useless: a saved file reading
    /// `"three three two"`, a backup numbered v8 where v20 was expected, an
    /// atomic write that had "lost" its exec bit to somebody else's `0644`.
    ///
    /// It looked exactly like a real bug in save, revert and backup, which is
    /// the expensive part — two agents chased it today before it was pinned to
    /// the path. A suite that goes red for reasons unrelated to your change
    /// teaches you to stop reading red.
    fn fixture(name: &str) -> String {
        format!("zemacs_{name}_test-{}", std::process::id())
    }

    fn scratch(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(fixture("revert"));
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
        open_file(&mut ed, &path, &resolve_init_path(), &mut Remote::default());
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
        open_file(&mut ed, &path, &resolve_init_path(), &mut Remote::default());
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
        open_file(&mut ed, &path, &resolve_init_path(), &mut Remote::default());
        let mut watch = Revert::default();
        watch.poll(&mut ed);

        ed.buffer.move_to_line_col(2, 0);
        ed.apply(EditorCommand::InsertText("delta ".into()));
        let at = ed.buffer.cursor;
        save_file(&mut ed, None, Save::Guarded, &mut Remote::default());
        touch_forward(&path); // a save moves the stamp; make sure it is noticed
        watch.poll(&mut ed);

        assert_eq!(ed.buffer.text.to_string(), "alpha\nbeta\ndelta gamma\n");
        assert_eq!(ed.buffer.cursor, at, "point did not move");
        assert!(!ed.status.contains("reverted"), "{}", ed.status);
        let _ = std::fs::remove_file(&path);
    }

    // --- saving --------------------------------------------------------

    /// Emacs' `save-buffer`, and the reason it is worth having: a save that
    /// always writes moves the mtime of every file you so much as look at,
    /// which `make`, a file watcher and `git status` all read as a change — and
    /// it spends the backup slot on a copy identical to what is already there.
    #[test]
    fn saving_a_buffer_with_no_changes_writes_nothing() {
        let path = scratch("untouched.txt", "one\n");
        let mut ed = Editor::new();
        open_file(&mut ed, &path, &resolve_init_path(), &mut Remote::default());
        assert!(!ed.buffer.modified);

        // Something else wrote the file after we opened it. If the save went
        // through it would clobber this with the buffer's copy; the point is
        // that it does not go through at all.
        std::fs::write(&path, "theirs\n").unwrap();
        touch_forward(&path);
        save_file(&mut ed, None, Save::Guarded, &mut Remote::default());

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs\n");
        assert!(ed.status.contains("no changes"), "{}", ed.status);
        // ...and it is not the changed-on-disk question either. There is
        // nothing to ask about, because there is nothing to write.
        assert!(ed.prompt.is_none(), "asked about a save it was not making");

        // One keystroke later there is something to save, and it saves.
        ed.apply(EditorCommand::InsertText("mine ".into()));
        save_file(&mut ed, None, Save::Forced, &mut Remote::default());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine one\n");
        let _ = std::fs::remove_file(&path);
    }

    /// The exception, and the reason the check is not simply `!modified`: a
    /// save-as is asking for a file that does not exist yet, so whether the
    /// buffer has been touched says nothing about whether the target needs
    /// writing.
    #[test]
    fn a_save_as_writes_even_when_nothing_was_typed() {
        let from = scratch("original.txt", "content\n");
        let to = from.with_file_name("copy.txt");
        let _ = std::fs::remove_file(&to);
        let mut ed = Editor::new();
        open_file(&mut ed, &from, &resolve_init_path(), &mut Remote::default());
        assert!(!ed.buffer.modified);

        save_file(&mut ed, Some(to.clone()), Save::Guarded, &mut Remote::default());
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "content\n");
        let _ = std::fs::remove_file(&from);
        let _ = std::fs::remove_file(&to);
    }

    /// `*scratch*` is a named buffer you come back to, so naming it after a
    /// file must not consume it. It forks: the file gets an ordinary buffer and
    /// the scratchpad stays a scratchpad.
    #[test]
    fn saving_the_scratchpad_forks_it_instead_of_consuming_it() {
        let path = scratch("from_scratch.rs", "");
        let _ = std::fs::remove_file(&path);
        let mut ed = Editor::new();
        // Buffer 1 is `*scratch*` in a fresh editor — go and stand on it.
        ed.switch_buffer_id(1);
        assert_eq!(ed.buffer.kind, BufferKind::Scratch);
        ed.apply(EditorCommand::InsertText("fn main() {}\n".into()));
        let scratch_id = ed.buffer.id;

        save_file(&mut ed, Some(path.clone()), Save::Guarded, &mut Remote::default());

        // The file is on disk...
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn main() {}\n");
        // ...the live buffer is an ordinary one visiting it, with none of a
        // scratchpad's properties left on it...
        assert_eq!(ed.buffer.kind, BufferKind::Text);
        assert_eq!(ed.buffer.path.as_ref(), Some(&path));
        assert_eq!(ed.buffer.name(), "from_scratch.rs");
        assert!(!ed.buffer.modified);
        assert_ne!(ed.buffer.id, scratch_id, "the scratchpad was consumed");
        // ...the language followed the extension, which is the thing the old
        // in-place rename had to bump the revision by hand to get...
        assert_eq!(ed.buffer.language.as_deref(), Some("rust"));
        // ...and `*scratch*` is still there, still itself, still holding what
        // was typed into it.
        let kept = ed
            .buffers()
            .find(|b| b.id == scratch_id)
            .expect("the scratchpad survived");
        assert_eq!(kept.kind, BufferKind::Scratch);
        assert_eq!(kept.name(), "*scratch*");
        assert_eq!(kept.text.to_string(), "fn main() {}\n");
        assert!(kept.path.is_none(), "the scratchpad kept no file behind it");
        let _ = std::fs::remove_file(&path);
    }

    /// The fork needs somewhere to land. A buffer already visiting the target
    /// would swallow it — `Editor::load` switches to an open path rather than
    /// adopting text — so this is refused before anything is written rather
    /// than leaving that buffer stale against a file that moved under it.
    #[test]
    fn forking_the_scratchpad_onto_an_open_file_is_refused() {
        let path = scratch("occupied.txt", "theirs\n");
        let mut ed = Editor::new();
        open_file(&mut ed, &path, &resolve_init_path(), &mut Remote::default());
        ed.switch_buffer_id(1);
        assert_eq!(ed.buffer.kind, BufferKind::Scratch);
        ed.apply(EditorCommand::InsertText("mine\n".into()));

        save_file(&mut ed, Some(path.clone()), Save::Guarded, &mut Remote::default());

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs\n");
        assert!(ed.status.contains("already open"), "{}", ed.status);
        assert_eq!(ed.buffer.kind, BufferKind::Scratch, "still on the scratchpad");
        let _ = std::fs::remove_file(&path);
    }

    /// The mode bits have to survive the rename, or every save of a shell
    /// script silently takes its executable bit off.
    #[test]
    #[cfg(unix)]
    fn an_atomic_write_keeps_the_files_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let path = scratch("script.sh", "#!/bin/sh\necho one\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        write_file(&path, "#!/bin/sh\necho two\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "#!/bin/sh\necho two\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "the executable bit survived the save");
        let _ = std::fs::remove_file(&path);
    }

    /// A dotfile is routinely a symlink into a repository. `rename` swaps a
    /// name, so without following the link first a save would put a real file
    /// where the link was and leave the repository untouched.
    #[test]
    #[cfg(unix)]
    fn saving_through_a_symlink_writes_the_file_it_points_at() {
        let real = scratch("real-target.txt", "before\n");
        let dir = real.parent().unwrap().to_path_buf();
        let link = dir.join("link-to-target.txt");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_file(&link, "after\n").unwrap();

        assert_eq!(std::fs::read_to_string(&real).unwrap(), "after\n");
        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "the link is still a link"
        );
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&real);
    }

    /// The promise, stated as a test: a write that fails leaves the file it was
    /// replacing exactly as it was. `fs::write` fails this — it has already
    /// truncated by the time anything can go wrong.
    #[test]
    #[cfg(unix)]
    fn a_failed_write_leaves_the_original_alone() {
        use std::os::unix::fs::PermissionsExt;
        // Its own directory, because making it read-only is the way to force
        // the failure and that must not stop the other tests writing.
        let dir = std::env::temp_dir().join(fixture("failed_write"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("precious.txt");
        std::fs::write(&path, "the original\n").unwrap();

        // No new files in here, so the temp file cannot be created.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let outcome = write_file(&path, "the replacement\n");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(outcome.is_err(), "the write really did fail");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "the original\n",
            "a failed save is a save that did not happen"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The previous saved version is what a backup is for, and the count is
    /// bounded so a long-lived file does not keep every save it ever had.
    #[test]
    fn backups_are_numbered_and_pruned_to_the_last_twenty() {
        let path = scratch("versioned.txt", "v0\n");
        let store = std::env::temp_dir().join(fixture("backup"));
        let _ = std::fs::remove_dir_all(&store);

        // One more save than the cap, so the pruning arm actually runs.
        for version in 0..=BACKUP_KEEP {
            std::fs::write(&path, format!("v{version}\n")).unwrap();
            backup_into(&store, &path);
        }

        let stem = format!("#{}#", path.to_string_lossy().replace('/', "!"));
        let mut kept: Vec<String> = std::fs::read_dir(&store)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(&stem))
            .collect();
        kept.sort();

        assert_eq!(kept.len(), BACKUP_KEEP, "kept exactly the cap");
        // The *oldest* is the one that goes: v0 was pruned, and the newest
        // backup holds the contents from just before the last save.
        assert!(!kept.contains(&format!("{stem}.~1~")), "v0's backup was pruned");
        assert_eq!(
            std::fs::read_to_string(store.join(format!("{stem}.~{}~", BACKUP_KEEP + 1))).unwrap(),
            format!("v{BACKUP_KEEP}\n"),
            "the newest backup is the version the last save replaced"
        );
        let _ = std::fs::remove_dir_all(&store);
        let _ = std::fs::remove_file(&path);
    }

    /// The clobber this closes: edit here, let something else write the file,
    /// then save. The old behaviour overwrote it without a word.
    #[test]
    fn saving_over_a_file_that_changed_underneath_asks_first() {
        let path = scratch("contested.txt", "mine\n");
        let mut ed = Editor::new();
        open_file(&mut ed, &path, &resolve_init_path(), &mut Remote::default());
        ed.apply(EditorCommand::InsertText("edited ".into()));

        // Somebody else — `git pull`, a formatter, an agent.
        std::fs::write(&path, "theirs\n").unwrap();
        touch_forward(&path);
        save_file(&mut ed, None, Save::Guarded, &mut Remote::default());

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "theirs\n",
            "the file was not written while the question was open"
        );
        let prompt = ed.prompt.as_ref().expect("a confirmation is open");
        assert_eq!(prompt.kind, PromptKind::Confirm);
        assert!(prompt.label.contains("changed on disk"), "{}", prompt.label);

        // "no" is anything that is not `yes`, and it must leave the file alone.
        ed.prompt.as_mut().unwrap().text = "n".into();
        for cmd in ed.handle_key(Key::Enter) {
            ed.apply(cmd);
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs\n");
        assert!(ed.pending_confirm.is_none(), "the parked save was dropped");
        let _ = std::fs::remove_file(&path);
    }

    /// And `yes` goes through — the prompt is a speed bump, not a wall.
    #[test]
    fn confirming_the_clobber_writes_the_buffer() {
        let path = scratch("conceded.txt", "mine\n");
        let mut ed = Editor::new();
        open_file(&mut ed, &path, &resolve_init_path(), &mut Remote::default());
        ed.apply(EditorCommand::InsertText("edited ".into()));
        std::fs::write(&path, "theirs\n").unwrap();
        touch_forward(&path);

        save_file(&mut ed, None, Save::Guarded, &mut Remote::default());
        ed.prompt.as_mut().unwrap().text = "yes".into();
        // The answer comes back as `Confirmed(SaveFile(..))`, which is the
        // whole mechanism: dispatching it must reach the *forced* save rather
        // than land back on the guard and ask again.
        let answer = ed.handle_key(Key::Enter);
        assert_eq!(
            answer,
            vec![EditorCommand::Confirmed(Box::new(EditorCommand::SaveFile(
                Some(path.clone())
            )))]
        );
        save_file(&mut ed, Some(path.clone()), Save::Forced, &mut Remote::default());

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited mine\n");
        assert!(!ed.buffer.modified);
        let _ = std::fs::remove_file(&path);
    }

    /// The stamp has to be re-taken on every path that puts the buffer back in
    /// sync, or the *next* save asks about a change that was already settled.
    #[test]
    fn a_save_after_a_revert_does_not_ask() {
        let path = scratch("settled.txt", "one\n");
        let mut ed = Editor::new();
        open_file(&mut ed, &path, &resolve_init_path(), &mut Remote::default());
        let mut watch = Revert::default();
        watch.poll(&mut ed);

        std::fs::write(&path, "two\n").unwrap();
        touch_forward(&path);
        watch.poll(&mut ed); // unmodified, so this reverts and re-syncs

        ed.apply(EditorCommand::InsertText("three ".into()));
        save_file(&mut ed, None, Save::Guarded, &mut Remote::default());

        assert!(ed.prompt.is_none(), "nothing to ask about after a revert");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "three two\n");
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
        open_file(&mut ed, &one, &resolve_init_path(), &mut Remote::default());
        open_file(&mut ed, &two, &resolve_init_path(), &mut Remote::default()); // `one` is now parked
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

    /// ...and it comes back *coloured*, without being visited.
    ///
    /// The whole chain, because every link in it was part of the bug: core has
    /// to name the buffer, the request has to be made for a buffer that is not
    /// live, the queue has to keep it instead of folding it into the live one,
    /// and the result has to land on its own buffer rather than being tested
    /// against a revision that has nothing to do with it. Before this, a file
    /// an agent rewrote came back correct and completely colourless and stayed
    /// that way until you switched to it and typed.
    #[test]
    fn a_parked_buffer_is_recoloured_after_it_reverts() {
        let parked = scratch("recolour_parked.rs", "fn parked() {}\n");
        let live = scratch("recolour_live.rs", "fn live() {}\n");
        let mut ed = Editor::new();
        open_file(&mut ed, &parked, &resolve_init_path(), &mut Remote::default());
        let id = ed.buffer.id;
        open_file(&mut ed, &live, &resolve_init_path(), &mut Remote::default()); // `parked` is now parked

        let highlighter = zemacs_syntax::spawn_worker();
        let mut watch = Revert::default();
        watch.poll(&mut ed); // first sight of both

        let after = "fn parked() { let s = \"two\"; }\n";
        std::fs::write(&parked, after).unwrap();
        touch_forward(&parked);
        watch.poll(&mut ed);
        assert!(
            ed.buffer_by_id(id).unwrap().highlights.is_empty(),
            "the spans it had describe text that is gone"
        );

        request_pending_parses(&mut ed, &highlighter);
        // The parse is on another thread, so this is a frame loop with the
        // frames taken out: the same two calls the app makes, until one lands.
        for _ in 0..200 {
            adopt_highlights(&mut ed, &highlighter);
            if !ed.buffer_by_id(id).unwrap().highlights.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // Not merely "some spans": the colours of the text it now holds.
        assert_eq!(
            ed.buffer_by_id(id).unwrap().highlights,
            zemacs_syntax::highlight("rust", after),
        );
        // ...and the live buffer, which nobody asked about, was left alone.
        assert!(ed.buffer.highlights.is_empty());
        let _ = std::fs::remove_file(&live);
        let _ = std::fs::remove_file(&parked);
    }

    /// The safety property, in the direction that used to be unreachable: an
    /// unsaved *parked* buffer is not clobbered either.
    #[test]
    fn a_modified_parked_buffer_is_left_alone_too() {
        let one = scratch("parked_dirty.txt", "alpha\n");
        let two = scratch("parked_other.txt", "beta\n");
        let mut ed = Editor::new();
        open_file(&mut ed, &one, &resolve_init_path(), &mut Remote::default());
        ed.apply(EditorCommand::InsertText("typed ".into()));
        let mine = ed.buffer.text.to_string();
        open_file(&mut ed, &two, &resolve_init_path(), &mut Remote::default());

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
        open_file(&mut ed, &path, &resolve_init_path(), &mut Remote::default());
        let mut watch = Revert::default();
        watch.poll(&mut ed);
        assert!(watch.seen.contains_key(&path));

        ed.kill_buffer(0);
        watch.poll(&mut ed);
        assert!(!watch.seen.contains_key(&path));
        let _ = std::fs::remove_file(&path);
    }

    // --- remote files ----------------------------------------------------
    //
    // The syntax itself, and every way of writing a local name that merely
    // looks remote, is pinned in `crates/tramp`'s `path` module — see
    // `a_local_path_is_recognised_as_local` there, which covers `/ssh:`, a
    // Windows drive letter, and a relative name full of colons. What is tested
    // here is the *routing*: which branch this layer takes, and what it costs
    // the local path to have the other one exist.
    //
    // `0.0.0.0#1` is the host wherever one is needed. It is not a name that has
    // to resolve and not a port anything listens on, so `connect(2)` refuses at
    // once — the same trick `crates/tramp`'s own always-runs test uses to get a
    // real failure without a real server.

    /// The one that would be a bug in every session: a local file must not pay
    /// for remote files existing. No ssh thread, no queued job, no round trip.
    #[test]
    fn a_local_file_never_reaches_the_ssh_worker() {
        let path = scratch("purely_local.txt", "one\n");
        let mut remote = Remote::default();
        let mut ed = Editor::new();

        open_file(&mut ed, &path, &resolve_init_path(), &mut remote);
        assert_eq!(ed.buffer.text.to_string(), "one\n", "{}", ed.status);
        ed.apply(EditorCommand::InsertText("two ".into()));
        save_file(&mut ed, None, Save::Guarded, &mut remote);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two one\n");

        assert!(remote.worker.is_none(), "a thread was spawned for a local file");
        assert!(remote.jobs.is_empty(), "a job was queued for a local file");

        // ...and so does every near miss. Each of these is a file on *this*
        // machine that happens to be spelled like a host, so each must take the
        // local branch and report the local failure — a missing file — rather
        // than trying to reach somebody. The syntax itself is `tramp`'s to
        // decide; what is asserted here is that this layer believes it.
        for near in [
            "/ssh",
            "/ssh:",             // no host and no second colon
            "/ssh:host",         // never closed
            "/ssh:/etc/passwd",  // empty host
            "/SSH:host:/x",      // the method is spelled in lower case
            "/sudo:root@host:/x",
        ] {
            open_file(&mut ed, Path::new(near), &resolve_init_path(), &mut remote);
            assert!(remote.worker.is_none(), "{near} was taken for a remote name");
            assert!(remote.jobs.is_empty(), "{near} queued a job");
        }
        let _ = std::fs::remove_file(&path);
    }

    /// `find-file /ssh:…` must not read anything on this machine and must not
    /// block: it queues one `stat` and returns, and the buffer is whatever it
    /// was until an answer arrives frames later.
    #[test]
    fn a_remote_name_is_queued_rather_than_opened() {
        let mut remote = Remote::default();
        let mut ed = Editor::new();
        let before = ed.buffer.text.to_string();

        open_file(
            &mut ed,
            Path::new("/ssh:0.0.0.0#1:/etc/hosts"),
            &resolve_init_path(),
            &mut remote,
        );

        assert_eq!(ed.buffer.text.to_string(), before, "the buffer was replaced");
        assert_eq!(remote.jobs.len(), 1, "nothing was queued");
        assert!(matches!(
            remote.jobs.values().next(),
            Some(Job::Probe(p)) if p.host == "0.0.0.0" && p.path == "/etc/hosts"
        ));
        assert!(ed.status.contains("opening"), "{}", ed.status);
    }

    /// The promise a remote save is entirely about: **sent is not written**.
    /// The connection is refused, so this needs no server — only a socket that
    /// says no — and the buffer has to come out of it still dirty.
    #[test]
    fn a_remote_save_that_fails_leaves_the_buffer_modified() {
        let mut remote = Remote::default();
        let mut ed = Editor::new();
        ed.load("secrets\n", Some(PathBuf::from("/ssh:0.0.0.0#1:/tmp/zemacs")), None);
        ed.apply(EditorCommand::InsertText("more ".into()));
        assert!(ed.buffer.modified);

        save_file(&mut ed, None, Save::Guarded, &mut remote);
        assert!(ed.buffer.modified, "cleared before the host had answered");
        assert_eq!(remote.jobs.len(), 1);

        // Wait for the refusal. Generous, because it is a wall clock on
        // somebody else's CI; the point is that it is well inside
        // `tramp::TIMEOUT` and that the answer is a message rather than a
        // panic.
        let mut dired = Dired::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !remote.jobs.is_empty() && Instant::now() < deadline {
            remote.poll(&mut ed, &mut dired);
            std::thread::yield_now();
        }

        assert!(remote.jobs.is_empty(), "no reply inside 30s");
        assert!(ed.buffer.modified, "a buffer that was never written looks saved");
        assert!(
            ed.status.contains("0.0.0.0"),
            "the failure did not name the host: {}",
            ed.status
        );
        // And nothing was written to a *local* file of that name, which is what
        // a missing `parse` would have done.
        assert!(!Path::new("/ssh:0.0.0.0#1:/tmp/zemacs").exists());
    }

    /// The sweep is four `stat`s a second per open file. Over ssh that is a
    /// network round trip on a timer, so a remote buffer is not in it at all.
    #[test]
    fn auto_revert_does_not_sweep_a_remote_buffer() {
        let local = scratch("swept.txt", "x\n");
        let mut ed = Editor::new();
        ed.load("x\n", Some(local.clone()), None);
        ed.load("remote\n", Some(PathBuf::from("/ssh:host:/etc/nginx.conf")), None);

        let mut watch = Revert::default();
        watch.poll(&mut ed);

        assert!(watch.seen.contains_key(&local), "the local buffer is still swept");
        assert_eq!(watch.seen.len(), 1, "a remote path was stat'd: {:?}", watch.seen);
        let _ = std::fs::remove_file(&local);
    }

    /// A remote file's crash copy is a *local* file, which is the whole point —
    /// it is on the machine that crashed. The path mangler needs no help, and
    /// the copy is offered back against the mtime the host sent.
    #[test]
    fn a_remote_buffer_auto_saves_locally_and_is_offered_back() {
        let name = PathBuf::from("/ssh:host:/etc/nginx.conf");
        let copy = autosave_file(&name).expect("a home directory");
        let _ = std::fs::remove_file(&copy);

        let mut ed = Editor::new();
        ed.load("listen 80;\n", Some(name.clone()), None);
        ed.apply(EditorCommand::InsertText("# ".into()));
        autosave_all(&ed);

        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "# listen 80;\n");
        // Older on the host than the copy: recoverable. Newer: superseded.
        assert_eq!(
            recovery_against(&name, std::time::UNIX_EPOCH),
            Some(copy.clone())
        );
        assert_eq!(recovery_against(&name, std::time::SystemTime::now()), None);
        let _ = std::fs::remove_file(&copy);
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

    // --- the after-edit delta ------------------------------------------------

    fn typing(editor: &mut Editor, s: &str) {
        editor.apply(EditorCommand::InsertText(s.into()));
    }

    /// A keystroke reaches the image as the edit it was, with the text it put
    /// there — not as three offsets the hook would have to read back.
    #[test]
    fn a_keystroke_reaches_lisp_as_a_range_and_the_text_that_now_fills_it() {
        let mut editor = Editor::new();
        editor.load("abc", None, None);
        let mut told = None;
        // The first look has no watermark, so it is a resync: the whole buffer,
        // and `nil` for what the image had, because the app cannot know.
        let form = after_edit_form(&editor, &mut told).expect("a first look always reports");
        assert!(form.contains("(funcall h 0 nil 3 \"abc\")"), "{form}");

        editor.apply(EditorCommand::MoveTo(1));
        typing(&mut editor, "XY");
        let form = after_edit_form(&editor, &mut told).expect("an insert is an edit");
        assert!(form.contains("(funcall h 1 1 3 \"XY\")"), "{form}");
        assert_eq!(editor.buffer.text.to_string(), "aXYbc");
    }

    /// The revision moves for reasons that are not edits. Reporting those would
    /// cost a `didChange` per minor-mode toggle and per language change.
    #[test]
    fn a_revision_that_moved_without_the_text_reaches_lisp_as_silence() {
        let mut editor = Editor::new();
        editor.load("(defun f () 1)", None, None);
        let mut told = None;
        after_edit_form(&editor, &mut told);
        editor.apply(EditorCommand::SetLanguage(Some("lisp".into())));
        assert_eq!(after_edit_form(&editor, &mut told), None);
    }

    /// Switching buffers is not an edit to either of them, so the image is told
    /// to start again rather than handed offsets into the wrong document.
    #[test]
    fn a_buffer_switch_reports_a_resync_rather_than_the_other_buffer_s_edits() {
        let mut editor = Editor::new();
        editor.load("first", None, None);
        let mut told = None;
        after_edit_form(&editor, &mut told);
        editor.create_buffer("*second*".into());
        typing(&mut editor, "hi");
        let form = after_edit_form(&editor, &mut told).expect("a new document is news");
        assert!(form.contains("(funcall h 0 nil 2 \"hi\")"), "{form}");
    }

    /// The text is escaped for CL's reader, not Rust's. `\n` inside a Common
    /// Lisp string literal reads as the character `n`, so a newline has to go
    /// through as itself — which is what makes this worth a test rather than a
    /// `{:?}`.
    #[test]
    fn the_text_crosses_as_a_common_lisp_string_and_not_a_rust_debug_one() {
        let mut editor = Editor::new();
        editor.load("", None, None);
        let mut told = None;
        after_edit_form(&editor, &mut told);
        typing(&mut editor, "a\"b\\c\nd");
        let form = after_edit_form(&editor, &mut told).unwrap();
        assert!(form.contains("\"a\\\"b\\\\c\nd\""), "{form}");
    }

    // --- the point-moved report ----------------------------------------------

    /// Offset 0 is where every buffer starts, so a switch between two of them
    /// moves point without moving the number — and an offset alone cannot tell
    /// the difference. Before the buffer travelled with it this was silence, and
    /// anything that re-renders on point (the equation preview, org-appear,
    /// show-paren) went on describing the buffer you had left.
    ///
    /// The order is asserted too, because a config may well have both hooks:
    /// core queues `buffer-switch-hook` from the switch itself and this appends
    /// after it, so the mode's global settings are re-resolved before anything
    /// re-renders against them.
    #[test]
    fn a_switch_between_two_buffers_at_the_same_offset_still_moves_point() {
        let mut editor = Editor::new();
        editor.load("first", None, None);
        let first = editor.buffer.id;
        let mut last = None;
        queue_point_moved(&mut editor, &mut last);

        editor.create_buffer("*second*".into());
        queue_point_moved(&mut editor, &mut last);
        editor.pending_hooks.clear();

        // Back to a buffer that never moved off 0 either, from one that is
        // still sitting on it. Nothing about the *number* has changed since the
        // last report; everything about the text under it has.
        editor.switch_buffer_id(first);
        assert_eq!(editor.buffer.cursor, 0, "both have to be at 0 or this tests nothing");
        queue_point_moved(&mut editor, &mut last);
        assert_eq!(editor.pending_hooks, ["buffer-switch-hook", "point-moved-hook"]);

        // ...and the frame after, with nothing touched, is silent — the pair is
        // a watermark, not a "always report on a switch" flag.
        editor.pending_hooks.clear();
        queue_point_moved(&mut editor, &mut last);
        assert!(editor.pending_hooks.is_empty(), "{:?}", editor.pending_hooks);
    }
}
