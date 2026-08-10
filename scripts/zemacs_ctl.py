#!/usr/bin/env python3
"""Client for `zemacs --control`'s newline-delimited JSON protocol.

    from zemacs_ctl import Zemacs
    with Zemacs() as z:
        z.keys("SPC p m")
        z.eval('(message "hi")')
        print(z.state())

See docs/control-mode.md for the full protocol. This module is also a CLI;
run `zemacs-ctl --help`.
"""

import json
import os
import queue
import subprocess
import sys
import threading
import time

DEFAULT_TIMEOUT = 10


class ZemacsError(Exception):
    pass


class Zemacs:
    def __init__(self, binary=None, args=None, timeout=DEFAULT_TIMEOUT):
        self.binary = binary or os.environ.get("ZEMACS_BIN") or self._default_binary()
        self.args = args or []
        self.timeout = timeout
        self.pid = None
        self.init_path = None
        self._proc = None
        self._next_id = 1
        self._id_lock = threading.Lock()
        self._pending = {}  # request id -> queue.Queue(maxsize=1) for its response
        self._pending_lock = threading.Lock()
        self._events = queue.Queue()

    @staticmethod
    def _default_binary():
        for p in ("./target/release/zemacs", "./target/debug/zemacs"):
            if os.path.exists(p):
                return p
        return "./target/release/zemacs"  # let Popen's FileNotFoundError say so

    # -- lifecycle
    def start(self):
        self._proc = subprocess.Popen(
            [self.binary, "--control", *self.args],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        threading.Thread(target=self._read_stdout, daemon=True).start()
        threading.Thread(target=self._read_stderr, daemon=True).start()
        self._wait_ready(self.timeout)
        return self

    def __enter__(self):
        return self.start()

    def __exit__(self, *exc_info):
        if self._proc and self._proc.poll() is None:
            try:
                self.quit()
            except Exception:
                pass
            try:
                self._proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self._proc.terminate()
        return False

    def _wait_ready(self, timeout):
        deadline = time.time() + timeout
        stash = []
        while True:
            remaining = deadline - time.time()
            if remaining <= 0:
                break
            try:
                evt = self._events.get(timeout=remaining)
            except queue.Empty:
                break
            if evt.get("event") == "ready":
                for e in stash:
                    self._events.put(e)
                self.pid = evt.get("pid")
                self.init_path = evt.get("init")
                return evt
            stash.append(evt)
        for e in stash:
            self._events.put(e)
        code = self._proc.poll()
        if code is not None:
            raise ZemacsError(
                f"{self.binary} exited (code={code}) before emitting 'ready'; check stderr"
            )
        raise ZemacsError("timed out waiting for 'ready' event")

    # -- background readers
    def _read_stdout(self):
        for line in self._proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if "id" in obj:
                with self._pending_lock:
                    q = self._pending.pop(obj["id"], None)
                if q is not None:
                    q.put(obj)
            elif "event" in obj:
                self._events.put(obj)

    def _read_stderr(self):
        for line in self._proc.stderr:
            sys.stderr.write(line)
            sys.stderr.flush()

    # -- request/response
    def _write(self, req):
        self._proc.stdin.write(json.dumps(req) + "\n")
        self._proc.stdin.flush()

    def _next_req_id(self):
        with self._id_lock:
            self._next_id += 1
            return self._next_id - 1

    def _send(self, op, **kwargs):
        req_id = self._next_req_id()
        resp_q = queue.Queue(maxsize=1)
        with self._pending_lock:
            self._pending[req_id] = resp_q
        self._write({"id": req_id, "op": op, **kwargs})
        try:
            resp = resp_q.get(timeout=self.timeout)
        except queue.Empty:
            with self._pending_lock:
                self._pending.pop(req_id, None)
            raise ZemacsError(f"timed out waiting for response to {op!r} (id={req_id})")
        if not resp.get("ok", False):
            raise ZemacsError(resp.get("error", "unknown error"))
        return resp.get("result")

    # -- ops
    def keys(self, keys):
        return self._send("keys", keys=keys)

    def action(self, name):
        return self._send("action", name=name)

    def eval(self, form):
        """Async: the value/error arrives later as a `message` event, not here."""
        return self._send("eval", form=form)

    def state(self):
        return self._send("state")

    def text(self, buffer=None):
        return self._send("text", **({"buffer": buffer} if buffer else {}))

    def messages(self, since=0):
        return self._send("messages", since=since)

    def mouse(self, x, y):
        """Move the pointer to (x, y) in *pixels* of the focused frame.

        Answers {"x","y","line","tooltip"}: which buffer line it landed on (or
        None outside a pane) and what hovering there put on screen (or None).
        Pixels, because everything interesting about a mouse is the arithmetic
        between a pixel and a character.
        """
        return self._send("mouse", x=x, y=y)

    def screen(self):
        return self._send("screen")

    def screenshot(self, path):
        return self._send("screenshot", path=path)

    def quit(self):
        """Fire-and-forget: the process may exit before a response is written."""
        self._write({"id": self._next_req_id(), "op": "quit"})

    # -- events
    def drain_events(self):
        """Return and remove all events currently queued, without blocking."""
        out = []
        while True:
            try:
                out.append(self._events.get_nowait())
            except queue.Empty:
                return out

    def wait_for_message(self, predicate, timeout=DEFAULT_TIMEOUT):
        """Block for a `message` event whose text satisfies predicate(text).

        Checks the backlog (messages that already arrived) before blocking on
        new ones, so a message that landed before this call is still seen.
        Non-matching events are left in the queue for drain_events()/later calls.
        """
        deadline = time.time() + timeout
        stash = []
        try:
            while True:
                remaining = deadline - time.time()
                if remaining <= 0:
                    raise ZemacsError("timed out waiting for matching message")
                try:
                    evt = self._events.get(timeout=remaining)
                except queue.Empty:
                    raise ZemacsError("timed out waiting for matching message")
                if evt.get("event") == "message" and predicate(evt.get("text", "")):
                    return evt
                stash.append(evt)
        finally:
            for e in stash:
                self._events.put(e)


# -- CLI
USAGE = """usage: zemacs-ctl <command> [args]

  keys <sequence>        feed a key sequence, e.g. "SPC p m"
  action <name>           run a named command/action
  eval <form>              evaluate Lisp (result arrives async, via messages)
  state                    editor state as JSON
  text [buffer]            buffer contents
  messages [since]         *Messages* log entries
  mouse <x> <y>            move the pointer (pixels); answers line + tooltip
  screen                   text rendering of the visible panes
  screenshot <path>        save a PNG (may fail headless; screen still works)
  repl                     interactive: type {"op": ..., ...} lines, watch events

Env: ZEMACS_BIN overrides the binary path (default ./target/release/zemacs,
falling back to ./target/debug/zemacs).
"""


def _repl(z):
    print(f"connected (pid={z.pid}, init={z.init_path})", file=sys.stderr)
    print("type JSON ops like {\"op\": \"state\"}; blank line to quit", file=sys.stderr)

    def print_events():
        while True:
            evt = z._events.get()
            print(f"event: {json.dumps(evt)}", file=sys.stderr)

    threading.Thread(target=print_events, daemon=True).start()
    for line in sys.stdin:
        line = line.strip()
        if not line:
            break
        try:
            req = json.loads(line)
            op = req.pop("op")
            print(json.dumps(z._send(op, **req)))
        except Exception as e:
            print(f"error: {e}", file=sys.stderr)


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if not argv or argv[0] in ("-h", "--help"):
        print(USAGE, end="")
        return
    cmd, rest = argv[0], argv[1:]
    ops = {
        "keys": lambda z: z.keys(" ".join(rest)),
        "action": lambda z: z.action(" ".join(rest)),
        "eval": lambda z: z.eval(" ".join(rest)),
        "state": lambda z: z.state(),
        "text": lambda z: z.text(rest[0] if rest else None),
        "messages": lambda z: z.messages(int(rest[0]) if rest else 0),
        "mouse": lambda z: z.mouse(int(rest[0]), int(rest[1])),
        "screen": lambda z: z.screen(),
        "screenshot": lambda z: z.screenshot(rest[0]),
    }
    if cmd not in ops and cmd != "repl":
        print(f"unknown command: {cmd}\n\n{USAGE}", end="", file=sys.stderr)
        sys.exit(1)
    if cmd == "screenshot" and not rest:
        print("usage: zemacs-ctl screenshot <path>", file=sys.stderr)
        sys.exit(1)
    with Zemacs() as z:
        _repl(z) if cmd == "repl" else print(json.dumps(ops[cmd](z)))


if __name__ == "__main__":
    main()
