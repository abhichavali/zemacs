# Control mode

`zemacs --control` runs the editor headless (`SDL_VIDEODRIVER=dummy`, no
window on your screen) and speaks newline-delimited JSON on stdin/stdout:
requests in, responses and events out. Warnings and panics go to stderr,
never stdout — stdout is the wire format. This is how an AI agent (or a
script) drives and debugs the editor without a human at the keyboard.

`zemacs --control --show` runs the same protocol but also opens a real
window, for when you want to watch it work.

## Protocol

```
Request:  {"id": <int>, "op": "<name>", ...args}
Response: {"id": <int>, "ok": true, "result": <json>}
          {"id": <int>, "ok": false, "error": "<string>"}
Event:    {"event": "<name>", ...}          # never carries an id
```

Every request gets exactly one response, matched by `id`. Events arrive
independently and interleaved with responses — a slow `eval` doesn't block a
`message` event from something else.

### Ops

| op | args | result | notes |
|---|---|---|---|
| `keys` | `{"keys": "SPC p m"}` | `{"fed": <int>}` | key sequence in the editor's token vocabulary: `SPC`, `<ret>`, `<tab>`, `<esc>`, `C-c`, `M-x`, `C-M-p`, bare chars |
| `action` | `{"name": "project-make"}` | `{"ok": true}` | run a named command, same door a keybinding or M-x uses |
| `eval` | `{"form": "(+ 1 2)"}` | `{"queued": true}` | **asynchronous** — the value or error arrives later as a `message` event, never in this response |
| `state` | — | `{"mode","buffer","path","major_mode","line","col","lines","modified","status","frames","windows"}` | |
| `text` | `{"buffer": "<name>"}` optional | `{"text": "..."}` | defaults to current buffer |
| `messages` | `{"since": <int>}` optional, default 0 | `{"messages": [...], "total": <int>}` | the *Messages* log; Lisp errors and init.lisp load failures show up here as `"lisp error: ..."` / `"init.lisp error: ..."` |
| `mouse` | `{"x": <px>, "y": <px>}` | `{"x","y","line","tooltip"}` | move the pointer, in **pixels of the focused frame**. `line` is the 1-based buffer line under it (`null` outside a pane), `tooltip` is what hovering there put on screen (`null` for nothing) |
| `screen` | — | `{"screen": "<text rendering of the visible panes>"}` | cheap inspection path; works even when screenshots don't |
| `screenshot` | `{"path": "/tmp/shot.png"}` | `{"path","w","h"}` | may fail with an error response if the headless backend can't read pixels back — expected, not a crash |
| `quit` | — | — | editor exits |

### Events

| event | fields | notes |
|---|---|---|
| `ready` | `pid`, `init` (path to init.lisp) | emitted once, after init.lisp has loaded. A client MUST wait for this before sending anything |
| `message` | `text` | every editor/Lisp message, as it happens — this is where `eval` results land |
| `exit` | `code` | editor process is going down |

## Python quickstart

`scripts/zemacs_ctl.py` is a stdlib-only client. `scripts/zemacs-ctl` is the
same thing as a CLI.

```python
from zemacs_ctl import Zemacs

with Zemacs() as z:              # spawns ./target/release/zemacs (or debug/), waits for `ready`
    z.keys("SPC p m")
    z.eval('(message "hi")')
    print(z.state())
    print(z.messages(since=0))
    print(z.mouse(4, 36))            # hover the gutter: {"line": 2, "tooltip": "error: ..."}
    z.screenshot("/tmp/shot.png")
```

- The binary is `./target/release/zemacs`, falling back to
  `./target/debug/zemacs`, overridable via `Zemacs(binary=...)` or the
  `ZEMACS_BIN` env var.
- A background thread continuously drains stdout so events are never lost
  while a request is in flight; responses are matched to pending requests by
  `id`, events go to a queue you read with `drain_events()` or
  `wait_for_message()`.
- A second thread mirrors the child's stderr to your stderr, so a Rust panic
  is visible instead of swallowed.
- `wait_for_message(predicate, timeout=10)` checks the backlog (messages that
  already arrived) before blocking on new ones — safe to call after the fact
  without racing the event.
- Requests time out after 10s by default and raise `ZemacsError` rather than
  hanging.
- `with` block: `__exit__` sends `quit`, waits briefly, then `terminate()`.

Shell one-liners — each spawns, waits for `ready`, runs one op, prints the
JSON result, quits:

```
scripts/zemacs-ctl keys "SPC p m"
scripts/zemacs-ctl eval '(+ 1 2)'
scripts/zemacs-ctl state
scripts/zemacs-ctl messages 0
scripts/zemacs-ctl screen
scripts/zemacs-ctl repl          # interactive: type {"op": ...} lines, watch events stream
```

## Worked example: reproducing a bug

Say a bug report claims editing `foo.lisp` and pressing `M-x project-make`
throws a Lisp error. Reproduce it headless:

```python
from zemacs_ctl import Zemacs

with Zemacs() as z:
    z.keys("SPC f f")                       # find-file
    z.eval('(insert "foo.lisp")')
    z.keys("<ret>")

    before = z.messages(since=0)["total"]
    z.action("project-make")

    # eval/action side effects land as `message` events; poll for them
    evt = z.wait_for_message(lambda t: "error" in t.lower(), timeout=5)
    print("caught:", evt["text"])

    print(z.screen()["screen"])             # see the state that produced it
    try:
        z.screenshot("/tmp/bug.png")
    except Exception as e:
        print("screenshot unavailable headless:", e)

    print(z.state())
    z.quit()
```

This gets you the exact error text, a text snapshot of what the editor
looked like when it happened, and optionally a pixel screenshot — all
without a window ever appearing, and all scriptable from an agent loop.

## Gotchas

- **stdout is the protocol.** Never `print()` into it from Lisp or anywhere
  else in this pipeline — it corrupts the stream. Debug output belongs on
  stderr.
- **`eval` is asynchronous.** Its response is just `{"queued": true}`; the
  actual value or error shows up later as a `message` event. Poll `messages`
  or use `wait_for_message`, don't expect it in the `eval` response.
- **Always wait for `ready`** before sending anything — `Zemacs.start()` /
  the `with` block does this for you; don't skip it if you're talking to the
  process directly.
- **`screenshot` can legitimately fail headless** if the backend can't read
  pixels back from a dummy video driver. Treat an error response as expected,
  not a crash — fall back to `screen`, which always works.
- **`mouse` takes pixels, not a line and column**, and that is the point of it:
  everything interesting about a pointer is the arithmetic between a pixel and a
  character (a gutter's width comes from the font, a wrapped line spends several
  rows, a folded one spends none). Don't hard-code a pixel for a row — the font
  differs per box. Sweep `y` and read the `line` back, or move to a `y` you got
  from an earlier reply. There is no `xrel`/`yrel` and no click op yet: this is
  the *motion* handler, so it drives hover and nothing else.
