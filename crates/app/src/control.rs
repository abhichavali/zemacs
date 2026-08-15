//! `--control` — the editor driven by a program instead of a person.
//!
//! The point is not scripting. It is that an agent trying to fix a bug in this
//! editor has, until now, had exactly one way to find out what the editor did:
//! run it and look at it. That works for a human at a keyboard and for nobody
//! else. This is the same editor — the same event loop, the same Lisp image,
//! the same renderer measuring the same panes — with a pipe where the keyboard
//! and the screen were.
//!
//! # It is the real editor, not a stub
//!
//! `--control` sets `SDL_VIDEODRIVER=dummy` and otherwise changes nothing. SDL
//! still opens a window, the renderer still lays panes out and writes
//! `viewport_lines` and `wrap_cols` back onto every one of them, `term.sync`
//! still has a pane geometry to size a child against, and the frame still
//! presents. All of that has to keep happening or the thing being debugged is
//! not the thing that ships: half the interesting bugs in an editor are in the
//! arithmetic between a font and a rectangle, and a headless mode that skipped
//! drawing would be blind to every one of them.
//!
//! # The protocol
//!
//! Newline-delimited JSON, stdin in and stdout out, and **stdout carries
//! nothing else**. Requests are `{"id": <int>, "op": "<name>", ...}` and are
//! answered `{"id": <int>, "ok": true, "result": ...}` or `{"id": <int>, "ok":
//! false, "error": "..."}`. Anything the editor volunteers is an *event* —
//! `{"event": "<name>", ...}`, never with an `id`, so a client can tell the two
//! apart without tracking what it asked for.
//!
//! # One request per frame
//!
//! [`Control::poll`] takes at most one line off the queue per turn of the main
//! loop, and that is deliberate rather than lazy. A request is answered from
//! the editor as it stands *after* everything the previous request set in
//! motion has been dispatched and drawn, so `keys` then `state` reports the
//! state the keys produced. Draining the queue in one pass would answer both
//! from the same pre-keystroke editor, which is the single most confusing thing
//! a driver of this protocol could be handed.
//!
//! # Waking up
//!
//! The main loop parks in `SDL_WaitEventTimeout`, which knows nothing about a
//! pipe. The reader thread therefore pushes a registered user event after every
//! line it queues, so a request lands as promptly as a keystroke does; and the
//! loop skips the wait entirely while [`Control::pending`] is true, so a burst
//! of requests runs at loop speed instead of one per wakeup. The alternative —
//! shortening the timeout into a poll tick — would have cost a wakeup, a lock
//! and a frame's worth of drawing several hundred times a second to notice a
//! pipe that is usually empty. Trading two lines of SDL for that was easy.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use crossbeam_channel::{Receiver, TryRecvError};
use sdl3::event::Event;
use serde_json::{json, Value};
use zemacs_core::dashboard::Row;
use zemacs_core::{normalize_keys, BufferKind, Editor, Key, MESSAGE_LIMIT};
use zemacs_lisp::Lisp;

use crate::{App, Batch};

/// The message whose arrival means `init.lisp` has finished loading.
///
/// There is no flag to read: `zemacs_lisp::spawn` loads the init file and only
/// *then* starts draining its request channel, so the first thing queued on
/// that channel cannot run until the config is up. That ordering is the signal.
/// We queue one form at startup, before any client can get a word in, and the
/// message it produces is `ready`.
///
/// It is a `message` and not something quieter because a message is the one
/// thing the image can say that reaches [`Editor::messages`], which is the
/// stream this module is watching anyway. The cost is that a client sees this
/// string as message 0; the benefit is no second mechanism.
const READY: &str = "zemacs: control mode ready";

/// How many lines a pane is assumed to show in [`screen`] before the renderer
/// has measured a real one. Only reachable in the first frame or two.
const ASSUMED_VIEWPORT: usize = 24;

pub struct Control {
    /// Held only to keep the event subsystem alive: an `EventSender` does not,
    /// and a shut-down subsystem turns every wakeup into a silent error.
    _events: sdl3::EventSubsystem,
    rx: Receiver<String>,
    /// Every message the editor has produced, oldest first and **uncapped** —
    /// which is the whole reason it exists. [`Editor::messages`] drops its
    /// oldest at [`MESSAGE_LIMIT`], and a client asking "what happened since
    /// message 900" has to be answerable in a session that produced 3000.
    ///
    /// Its last [`MESSAGE_LIMIT`] entries are, by construction, exactly what
    /// `editor.messages` held at the previous poll — which is what makes
    /// [`appended`] able to work out what is new without core keeping a
    /// counter for us.
    ///
    /// ponytail: unbounded. A session that produces enough messages to matter
    /// wants a ring and a `dropped` count in the `messages` reply, not a
    /// bigger `Vec`.
    log: Vec<String>,
    /// Screenshots asked for this frame, answered after the draw. See
    /// [`Control::shoot`].
    shots: Vec<(Value, PathBuf)>,
    ready: bool,
    init: PathBuf,
}

impl Control {
    /// Start reading stdin and queue the form whose message means "ready".
    ///
    /// Called before [`App::new`] takes the `Sdl` handle, and before the loop
    /// starts, so the probe is the first thing in the image's request queue and
    /// no client request can overtake it.
    pub fn start(sdl: &sdl3::Sdl, lisp: &Lisp, init: PathBuf) -> anyhow::Result<Self> {
        let events = sdl
            .event()
            .map_err(|e| anyhow::anyhow!("SDL event subsystem: {e}"))?;
        // Safe in spite of the signature. The only contract on
        // `SDL_RegisterEvents` is that the number it hands back is used as an
        // event type and as nothing else, which is all the reader thread does
        // with it.
        let wake = unsafe { events.register_event() }
            .map_err(|e| anyhow::anyhow!("register the stdin wakeup event: {e}"))?;
        let sender = events.event_sender();
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::Builder::new()
            .name("zemacs-control".into())
            .spawn(move || {
                for line in std::io::stdin().lock().lines() {
                    // A read error and end of input are the same thing here:
                    // the parent is gone. Falling out of the loop drops `tx`,
                    // and that is how the main loop finds out.
                    let Ok(line) = line else { break };
                    if tx.send(line).is_err() {
                        break;
                    }
                    // Pushed *after* the send, so the loop that this wakes
                    // always finds the line it is being woken for.
                    let _ = sender.push_event(Event::User {
                        timestamp: 0,
                        window_id: 0,
                        type_: wake,
                        code: 0,
                        data1: std::ptr::null_mut(),
                        data2: std::ptr::null_mut(),
                    });
                }
            })?;

        // Two things in one form, both of which have to happen before a client
        // can see anything. The `setf` is the stdout discipline: ECL's
        // `*standard-output*` is this process's stdout, so a stray `print` in a
        // config or a command would splice garbage into the protocol stream.
        // Sending it to stderr costs nothing — an agent reads stderr too — and
        // makes the stream forgeable only by this module.
        //
        // ponytail: this runs *after* `init.lisp` has loaded, so a `print` in
        // the init file itself still reaches stdout. Fixing that means binding
        // the stream inside `load-init`, in the image, which is not this file.
        lisp.eval(format!(
            "(progn (setf *standard-output* *error-output*) (message {}))",
            zemacs_rpc::lisp::string(READY)
        ));

        Ok(Self {
            _events: events,
            rx,
            log: Vec::new(),
            shots: Vec::new(),
            ready: false,
            init,
        })
    }

    /// Whether a request is already queued, so the loop can skip its wait.
    pub fn pending(&self) -> bool {
        !self.rx.is_empty()
    }

    /// Stream any new messages, then serve at most one request.
    ///
    /// `false` means stdin closed: the parent is gone, and an editor nobody can
    /// drive and nobody can see is a process holding a font open.
    pub fn poll(&mut self, app: &mut App, editor: &mut Editor, batch: &mut Batch) -> bool {
        self.stream(editor);
        let line = match self.rx.try_recv() {
            Ok(line) => line,
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return false,
        };
        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                self.send(json!({"id": Value::Null, "ok": false, "error": format!("bad JSON: {e}")}));
                return true;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        match self.run(app, editor, batch, &id, &request) {
            // `None` is a request whose answer is not ready yet — a screenshot,
            // which has to wait for this frame to be drawn. It answers itself.
            Ok(None) => {}
            Ok(Some(result)) => self.send(json!({"id": id, "ok": true, "result": result})),
            Err(e) => self.send(json!({"id": id, "ok": false, "error": e})),
        }
        true
    }

    /// One op. Errors are strings because every one of them is going straight
    /// into a JSON field a person will read.
    fn run(
        &mut self,
        app: &mut App,
        editor: &mut Editor,
        batch: &mut Batch,
        id: &Value,
        req: &Value,
    ) -> Result<Option<Value>, String> {
        let op = req.get("op").and_then(Value::as_str).ok_or("no \"op\"")?;
        let ok = |v: Value| Ok(Some(v));
        match op {
            // Into `batch.keys`, which the loop drains four lines below this
            // call — so a synthetic key takes byte for byte the path a real
            // keystroke takes, through `handle_key` and `dispatch`, rather than
            // a second one written to look like it.
            "keys" => {
                let keys = req.get("keys").and_then(Value::as_str).ok_or("no \"keys\"")?;
                // Parsed in full before any of it is fed: half a key sequence
                // leaves the editor in a pending state nobody asked for, and
                // "which token was bad" is only useful if it did not also
                // happen.
                let parsed = normalize_keys(keys)
                    .split_whitespace()
                    .map(|t| Key::from_token(t).ok_or_else(|| format!("unknown key token {t:?}")))
                    .collect::<Result<Vec<_>, _>>()?;
                let fed = parsed.len();
                batch.keys.extend(parsed);
                ok(json!({ "fed": fed }))
            }
            "action" => {
                let name = req.get("name").and_then(Value::as_str).ok_or("no \"name\"")?;
                for cmd in editor.run_action(name) {
                    app.dispatch(editor, cmd);
                }
                ok(json!({"ok": true}))
            }
            // Queued, not run: `Lisp::eval` hands the form to the image's own
            // thread and returns. The *value* — and any error — comes back
            // later as a `message` event, because that is genuinely how the
            // image reports. `eval-string` messages the last form's value and
            // messages `lisp error: ...` on a condition; neither is a return.
            "eval" => {
                let form = req.get("form").and_then(Value::as_str).ok_or("no \"form\"")?;
                app.lisp.eval(form.to_string());
                ok(json!({"queued": true}))
            }
            "state" => {
                let b = &editor.buffer;
                let (line, col) = b.cursor_line_col();
                ok(json!({
                    "mode": editor.mode.label(),
                    "buffer": b.name(),
                    "path": b.path.as_ref().map(|p| p.display().to_string()),
                    "major_mode": b.major_mode,
                    // 1-based, as the modeline counts and as `:42` means.
                    "line": line + 1,
                    "col": col + 1,
                    "lines": b.len_lines(),
                    "modified": b.modified,
                    "status": editor.status,
                    "frames": editor.frames.len(),
                    // Every pane in every frame, not the focused frame's.
                    "windows": editor.frames.iter().map(|f| f.windows.len()).sum::<usize>(),
                }))
            }
            "text" => {
                let want = req.get("buffer").and_then(Value::as_str);
                let buf = match want {
                    Some(name) => editor
                        .buffers()
                        .find(|b| b.name() == name)
                        .ok_or_else(|| format!("no buffer named {name:?}"))?,
                    None => &editor.buffer,
                };
                ok(json!({"text": buf.text.to_string()}))
            }
            "messages" => {
                let since = req
                    .get("since")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(self.log.len() as u64) as usize;
                ok(json!({"messages": &self.log[since..], "total": self.log.len()}))
            }
            // The pointer, in **pixels of the focused frame**. Pixels and not a
            // (line, column), which is the whole reason this op is worth having:
            // everything interesting about a mouse is the arithmetic between a
            // pixel and a character — a gutter's width comes from the font, a
            // wrapped line spends several rows, a folded one spends none — and an
            // op that took a line number would answer for a chain with that
            // arithmetic cut out of it.
            //
            // Through `App::hover`, the same function the SDL motion arm calls,
            // for the reason `keys` goes through `batch.keys`: a synthetic
            // gesture that took its own path would be testing a second
            // implementation of the thing under test. What it does *not* do is
            // move a real pointer — there is none — so this is the motion
            // handler, not the event.
            //
            // `line` is what the pointer is over, so a client can move it
            // somewhere meaningful without knowing this build's font metrics;
            // `tooltip` is what came up, or NIL.
            "mouse" => {
                let at = |k: &str| req.get(k).and_then(Value::as_i64).map(|v| v as i32);
                let x = at("x").ok_or("no \"x\"")?;
                let y = at("y").ok_or("no \"y\"")?;
                let i = editor.focus_frame.min(app.renderers.len().saturating_sub(1));
                if app.renderers.get(i).is_none() {
                    return Err("no window to point at".into());
                }
                app.hover(editor, i, x, y);
                // ponytail: read off `editor.buffer`, so in a split this names the
                // line as the *live* buffer counts it. Right for the focused pane
                // and a lie for any other, and the fix is `buffer_by_id` on the
                // window `click_target` already answers with.
                let line = app.renderers[i].click_target(editor, i, x, y).map(|(_, at)| {
                    editor.buffer.text.char_to_line(at.min(editor.buffer.len_chars())) + 1
                });
                ok(json!({
                    "x": x,
                    "y": y,
                    "line": line,
                    "tooltip": editor.tooltip.as_ref().map(|t| t.text.clone()),
                }))
            }
            "screen" => ok(json!({"screen": screen(editor)})),
            "screenshot" => {
                let path = req
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or("no \"path\"")?;
                self.shots.push((id.clone(), PathBuf::from(path)));
                Ok(None)
            }
            "quit" => {
                editor.should_quit = true;
                ok(json!({"ok": true}))
            }
            other => Err(format!("unknown op {other:?}")),
        }
    }

    /// Take the screenshots this frame asked for, and answer them.
    ///
    /// Called between the draw and the present, because `save_png` reads the
    /// backbuffer and presents it itself — see its doc comment. Answering at
    /// request time instead would save whatever was on screen *before* the
    /// keystroke that the same client had just sent, which is the one picture
    /// nobody wants.
    /// A screenshot is queued, so this frame has to be drawn whether or not
    /// anything changed: [`Control::shoot`] reads the frame buffer back, and
    /// reading back one the loop declined to fill hands out whatever was in it.
    pub fn shooting(&self) -> bool {
        !self.shots.is_empty()
    }

    pub fn shoot(&mut self, app: &mut App, editor: &Editor) {
        for (id, path) in std::mem::take(&mut self.shots) {
            let i = editor.focus_frame.min(app.renderers.len().saturating_sub(1));
            let Some(renderer) = app.renderers.get_mut(i) else {
                self.send(json!({"id": id, "ok": false, "error": "no window to photograph"}));
                continue;
            };
            let area = renderer.content_area();
            let reply = match renderer.save_png(&path) {
                Ok(()) => json!({"id": id, "ok": true, "result": {
                    "path": path.display().to_string(), "w": area.w, "h": area.h,
                }}),
                Err(e) => json!({"id": id, "ok": false, "error": format!("{e:#}")}),
            };
            self.send(reply);
        }
    }

    /// Everything appended to [`Editor::messages`] since the last look, as
    /// `message` events, and the `ready` event if this is where it belongs.
    fn stream(&mut self, editor: &Editor) {
        let seen = &self.log[self.log.len().saturating_sub(MESSAGE_LIMIT)..];
        let fresh = appended(seen, &editor.messages);
        if fresh == 0 {
            return;
        }
        let new: Vec<String> = editor.messages[editor.messages.len() - fresh..].to_vec();
        self.log.extend_from_slice(&new);
        for text in new {
            let ready = !self.ready && text == READY;
            self.send(json!({"event": "message", "text": text}));
            if ready {
                self.ready = true;
                self.send(json!({
                    "event": "ready",
                    "pid": std::process::id(),
                    "init": self.init.display().to_string(),
                }));
            }
        }
    }

    /// The last frame anyone gets. Emitted from `main` on every way out, so a
    /// client waiting on it also hears about the window being closed.
    pub fn exit(&mut self, code: i32) {
        self.send(json!({"event": "exit", "code": code}));
    }

    /// One frame, flushed. Write errors are dropped: a parent that has closed
    /// its end of the pipe has also closed the other one, and the reader thread
    /// is about to end the loop for a better reason than this.
    fn send(&mut self, frame: Value) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{frame}").and_then(|()| out.flush());
    }
}

/// How many entries were pushed onto `now` since it looked like `seen`.
///
/// Not `now.len() - seen.len()`, and the difference is the whole function:
/// [`Editor::messages`] drops its oldest once it is full, so in any session
/// long enough to matter the length stops moving while the contents keep
/// scrolling — and a watermark that is an index into it silently stops
/// reporting anything at all.
///
/// So: a push shifts the vector left by one once it is full, which means `seen`
/// and `now` overlap by `len(now) - k` entries for the true `k`, and the
/// smallest `k` whose overlap matches is that `k`. Repeated identical messages
/// are why this compares the whole overlap rather than looking for the last
/// entry it remembers.
fn appended(seen: &[String], now: &[String]) -> usize {
    let (s, n) = (seen.len(), now.len());
    // `k == n` always matches, with an empty overlap, so there is always an
    // answer — that one being "everything here is new", which is right for the
    // first call and for a gap too big to bridge.
    (n.saturating_sub(s)..=n)
        .find(|&k| seen[s - (n - k)..] == now[..n - k])
        .unwrap_or(n)
}

/// The visible panes as text: what is on screen, for a reader with no screen.
///
/// Built from `Editor` alone rather than from the renderer, and that is the
/// point of it — this has to work when a screenshot cannot, which is every case
/// where the interesting question is "what does the editor think it is showing"
/// rather than "what did the GPU do". It is also the cheap one: an agent will
/// ask for this after every keystroke.
///
/// ponytail: no wrapping and no truncation, so a line longer than the pane
/// reads as one row here and several on screen. Feeding it `wrap_cols` would
/// fix that, and would also mean reimplementing the renderer's line breaking a
/// second time, differently.
///
/// ponytail: a *line* window, not a row one. Folded lines are skipped, and a
/// line carrying a tall image claims rows here that it does not print, so a
/// pane with either shows fewer lines than it has room for. The honest fix is
/// the renderer's `visible_lines`, which is the function that already counts
/// rows properly — and reaching it means this taking a `Renderer`, which is the
/// dependency the whole function exists to avoid.
fn screen(editor: &mut Editor) -> String {
    use std::fmt::Write;
    // The focused pane's cursor and scroll live on the editor while it is being
    // typed into, and only reach its `Window` when the renderer asks. Parking
    // them here first is what lets every pane below be read the same way — and
    // it is the same call the loop makes before drawing, so it costs nothing.
    editor.sync_focused_window();

    let mut out = String::new();
    for (fi, frame) in editor.frames.iter().enumerate() {
        if editor.frames.len() > 1 {
            let _ = writeln!(out, "=== frame {fi}{} ===", mark(fi == editor.focus_frame));
        }
        for window in &frame.windows {
            let buf = editor.buffer_by_id(window.buffer).unwrap_or(&editor.buffer);
            let height = match window.viewport_lines {
                0 => ASSUMED_VIEWPORT,
                n => n,
            };
            let lines = buf.len_lines();
            let top = window.scroll.min(lines.saturating_sub(1));
            let bottom = (top + height).min(lines);
            let cursor = buf.text.char_to_line(window.cursor.min(buf.len_chars()));
            let _ = writeln!(
                out,
                "--- window {}{}  {}{}  lines {}-{}/{lines}  {} ---",
                window.id,
                mark(fi == editor.focus_frame && window.id == frame.current),
                buf.name(),
                if buf.modified { " [+]" } else { "" },
                top + 1,
                bottom,
                buf.major_mode,
            );
            // The dashboard is the one thing on screen with no document behind
            // it — the renderer builds it out of `Editor::dashboard` — so a
            // pane showing it would otherwise read as one blank line, which is
            // exactly the screen a client sees first.
            //
            // ponytail: the same is true of a `Scene` pane (a PDF, a figure)
            // and of the completion popup, and neither is rendered here. A
            // scene has no text to give; the popup wants the same treatment
            // `which_key` gets below, the day something needs it.
            if buf.kind == BufferKind::Dashboard {
                for row in editor.dashboard.rows() {
                    let _ = writeln!(out, "  {}", dashboard_row(&row));
                }
                continue;
            }
            // The gutter is a *decision*, not a constant: the shipped config
            // turns it off for org, prose and the tutor with
            // `set-no-gutter-modes`, and printing a number column regardless
            // made this a report of a screen nobody has. Asked through the
            // renderer's own predicate so the two cannot drift.
            let gutter = zemacs_render::gutter_on(buf, &editor.settings);
            for i in top..bottom {
                // A folded line costs no row on screen, so it costs none here.
                let start = buf.line_start(i);
                if zemacs_core::overlay::fold_hiding(buf.overlays(), start).is_some() {
                    continue;
                }
                // The same layout the renderer lays out with, overlays and all —
                // so a heading's stars read as their bullet, `[X]` as its tick,
                // an avy label as its letter, and a `display` substitution as
                // what it substitutes. Reading the rope raw was reporting the
                // file rather than the screen, which is the one thing this
                // function is for.
                let end = start + buf.line_len(i);
                let text = buf.slice_string(start, end);
                let shown: String = zemacs_core::display::line_cells(
                    &text,
                    editor.settings.tab_width,
                    buf.overlays(),
                    start,
                    end,
                )
                .iter()
                .map(|&(c, _)| c)
                .collect();
                let _ = match gutter {
                    true => writeln!(
                        out,
                        "{}{:>5} {}",
                        if i == cursor { '>' } else { ' ' },
                        i + 1,
                        shown.trim_end_matches('\n')
                    ),
                    false => writeln!(
                        out,
                        "{} {}",
                        if i == cursor { '>' } else { ' ' },
                        shown.trim_end_matches('\n')
                    ),
                };
            }
        }
    }
    // The echo area, which is also the prompt line when one is open — that is
    // `status_line`'s own rule, and the reason a `:` prompt shows up here
    // without this knowing anything about prompts.
    let _ = writeln!(out, "{}", editor.status_line());
    // Drawn over the frame rather than in it, so it is neither a pane nor the
    // modeline, and a client that cannot see it cannot tell why a leader key
    // appeared to do nothing.
    for row in &editor.which_key {
        let _ = writeln!(out, "which-key: {row}");
    }
    // Same argument, one surface along: drawn over the frame, so a client that
    // cannot see it cannot otherwise tell that the pointer is being answered.
    if let Some(tip) = &editor.tooltip {
        let _ = writeln!(out, "tooltip: {}", tip.text);
    }
    out
}

/// One dashboard row, flattened. The `>` on the selected item is the only
/// thing here a client cannot get from `state`, and it is the thing `j` and `k`
/// move.
fn dashboard_row(row: &Row) -> String {
    match row {
        Row::Banner(text) | Row::Footer(text) => text.clone(),
        Row::Heading(text) => (*text).to_string(),
        Row::Blank => String::new(),
        Row::Item {
            key,
            label,
            hint,
            selected,
        } => format!(
            "{} {key}  {label}  {hint}",
            if *selected { '>' } else { ' ' }
        ),
    }
}

fn mark(on: bool) -> &'static str {
    if on {
        " *"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::appended;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The one piece of arithmetic in here that the integration test cannot
    /// reach: getting to the capped regime would mean driving five hundred
    /// messages through a real editor.
    #[test]
    fn appended_survives_the_message_cap() {
        // Below the cap: the vector only grows.
        assert_eq!(appended(&strs(&["a", "b"]), &strs(&["a", "b", "c"])), 1);
        assert_eq!(appended(&strs(&["a", "b"]), &strs(&["a", "b"])), 0);
        // First look: everything is new.
        assert_eq!(appended(&[], &strs(&["a", "b"])), 2);

        // At the cap the length stops moving, which is exactly where a
        // watermark that is an index would go silent for the rest of the
        // session.
        assert_eq!(appended(&strs(&["a", "b", "c"]), &strs(&["b", "c", "d"])), 1);
        assert_eq!(appended(&strs(&["a", "b", "c"]), &strs(&["c", "d", "e"])), 2);
        // Repeats are why the whole overlap is compared rather than the last
        // entry looked up: "x" three times running must count as three.
        assert_eq!(appended(&strs(&["x", "x", "x"]), &strs(&["x", "x", "x"])), 0);
        assert_eq!(appended(&strs(&["a", "x", "x"]), &strs(&["x", "x", "x"])), 1);
        // Straddling the cap: one push filled it and the rest evicted.
        assert_eq!(appended(&strs(&["a", "b"]), &strs(&["c", "d", "e"])), 3);
    }
}
