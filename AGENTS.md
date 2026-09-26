# AGENTS.md

zemacs is a modal (vim-style) text editor written in Rust whose configuration
and most features are a live Common Lisp image. ECL is embedded through a C
shim, runs on its own thread for the life of the process, and loads
`~/.zemacs.d/init.lisp` as a program. Rust owns the rope, the event loop,
rendering, parsing, processes and IO; Lisp owns key bindings, modes and most
user-facing commands (org, magit's policy, LSP, PDFs, the tutor). Development
happens on macOS with Homebrew.

This file is the entry point for coding agents. Details live in `docs/`, linked
below. It is kept true by the rule in
[Documentation is part of the change](#documentation-is-part-of-the-change).

## Repo map

| Path | Package | Entry | Responsibility |
|---|---|---|---|
| `crates/app` | `zemacs-app` (bin `zemacs`) | `src/main.rs` | SDL3 loop; owns the `Editor`; `App::dispatch` performs app-only effects (files, git, dired, terminals, projects); `--control` (`src/control.rs`) |
| `crates/core` | `zemacs-core` | `src/lib.rs` | Pure model: buffers, windows, frames, `EditorCommand`, `Editor::apply`; vim grammar in `src/evil.rs`; Lisp readers in `src/query.rs` |
| `crates/render` | `zemacs-render` | `src/lib.rs` | Draws a frame with SDL3 + SDL3_ttf: glyphs, panes, modeline, overlays, scenes, PNG screenshots |
| `crates/lisp` | `zemacs-lisp` | `src/lib.rs`, `src/shim.c` | The ECL image: C shim (compiled by `build.rs` via `ecl-config`), the Rust half of every primitive, scene reader |
| `crates/syntax` | `zemacs-syntax` | `src/lib.rs` | tree-sitter highlighting thread; hand-written org scanner (`src/org.rs`) |
| `crates/gui` | `zemacs-gui` | `src/lib.rs` | Scene tree and pixel layout (`src/layout.rs`); no dependencies |
| `crates/term` | `zemacs-term` | `src/lib.rs` | Terminal emulator in a buffer (`alacritty_terminal`) |
| `crates/rpc` | `zemacs-rpc` | `src/lib.rs` | JSON-RPC over a child's stdio; knows no protocol |
| `crates/git` | `zemacs-git` | `src/lib.rs` | git via the `git` binary: status, hunks, rebase |
| `crates/dired` | `zemacs-dired` | `src/lib.rs` | Directory listings and file operations |
| `crates/project` | `zemacs-project` | `src/lib.rs` | Project roots, file lists, recent projects, ripgrep search |
| `crates/tramp` | `zemacs-tramp` | `src/lib.rs` | `/ssh:host:/path` names and remote IO |
| `crates/latex` | `zemacs-latex` | `src/lib.rs` | One LaTeX fragment to a bitmap (`latex` + `dvipng`) |
| `crates/figure` | `zemacs-figure` | `src/lib.rs` | One image file (PNG, JPEG, SVG) to a bitmap |

- `runtime/` — the shipped Lisp, read from disk at run time, not compiled in.
  `init.lisp` (the *default* config only), `library.lisp` (what every config
  calls; holds `*runtime-modules*`), `gui.lisp`, `rpc.lisp`, `lsp.lisp`,
  `modes/` (`modes.lisp` is the mode machinery, `modes/library.lisp` the shipped
  mode definitions, the rest are features), `plugins/project.lisp`, `themes/`,
  `mathsync_transcribe.py`.
- `docs/` — see the [Documentation map](#documentation-map).
- `scripts/` — `install.sh`, `bundle.sh` (the `.app`), `zemacs_ctl.py` and its
  CLI `zemacs-ctl` (control-mode client).
- `examples/math/` — a sample curriculum (`linear-algebra.org`) in the format
  of `docs/curriculum.org`.
- `assets/` — the app icon `bundle.sh` uses.
- `TODO.org` — the running work log and plan: done, open, paused, and why.

Crate graph, threads and the path of one keystroke: `docs/architecture.org`.

## Where a change goes

**The standing rule: features are Lisp (`runtime/*.lisp`) on top of fast
primitives in Rust.** Rust owns the hot path, transport, process lifetimes and
data structures; policy and user-facing commands are Lisp. The exceptions stay
in Rust for latency: the modal state machine (`crates/core/src/evil.rs`) and
anything that runs per keystroke. Before writing Rust, walk the ladder in
`docs/boundary.org` ("How to decide where a new thing goes") and stop at the
first rung that works:

1. **Lisp over what exists** — a function in a runtime file. No Rust.
2. **A reader** (a new question about editor state, answered via `%query`, two
   integer arguments at most): one arm in `zemacs_core::query::query`
   (`crates/core/src/query.rs`) — or in `ask_here` in `crates/lisp/src/lib.rs`
   if the answer needs a crate downstream of core — plus a Lisp name: the
   `*readers*` list in `QUERIES_FORM` in `crates/lisp/src/shim.c` for a
   zero-argument reader, or a `defun` over `(%query "name" a b)` in a runtime
   file. A zero-argument `defun` in a runtime file is offered by `M-x` unless
   its name is pushed onto `*hidden-commands*`, as `runtime/gui.lisp` does
   (`*readers*` is copied into that list once, when `library.lisp` loads).
3. **A writer** that needs no value back (`%do`: verb, optional string, two
   integers): an `EditorCommand` variant with its arm in `Editor::apply`
   (`crates/core/src/lib.rs`), one arm in `command_for`
   (`crates/lisp/src/lib.rs`), and a `defun` over `(%do "verb" arg a b)` (in a
   runtime file, or `LIBRARY_FORM` in `shim.c`). If only the app can perform it,
   also list it in `EditorCommand::needs_app` and handle it in `App::dispatch`
   (`crates/app/src/main.rs`); it then lands a frame later.
4. **A dedicated C primitive** only for what must hand a handle back or cannot
   fit the envelopes: `rs_*` in `lib.rs`, `extern` + `f_*` + a `defprim` line in
   `zemacs_boot` in `shim.c`.
5. **Hot per keystroke** — Rust, with a knob for Lisp rather than a hook.

A built-in verb (a name a key can be bound to, resolved in Rust) is an arm in
`Editor::run_action` plus an entry in `BUILTIN_COMMANDS` (`evil.rs`). The full
table is "Where to add things" at the end of `docs/boundary.org`.

Worked example: `git show 1462e4d --stat` (the PDF reader) — 359 lines of
`runtime/modes/pdf.lisp`, three small readers (`window-pixel-size`,
`scene-scroll`, `image-size` in `runtime/gui.lisp` over `query.rs`), one verb
(`scene-scroll` in `command_for` to `EditorCommand::SceneScroll`), three lines
in the renderer, its place in `*runtime-modules*`, a test and a
`docs/reference.org` section.

**Runtime modules** load in order from `*runtime-modules*` in
`runtime/library.lisp`; the comment above the list says why each is where it
is. A new module: add the file, add it to the list in the right place, add its
entry to that comment, add a `docs/reference.org` section and a test.

**Config vs library.** `runtime/init.lisp` is copied to `~/.zemacs.d/init.lisp`
on first run and never touched again, so a change there reaches no existing
user. Anything every user must get goes in `runtime/library.lisp` or a runtime
module (the user's init loads `library.lisp` and calls
`(load-runtime-modules)`). When a key "does nothing" for the owner, grep
`~/.zemacs.d/init.lisp` for a conflicting binding.

## Build, run, install

Prerequisites: `brew install ecl sdl3 sdl3_ttf pkgconf` and stable Rust.
Features also shell out to `git`, `rg`, `mutool` (PDF), `latex` + `dvipng`
(LaTeX previews) and `python3`.

| Task | Command |
|---|---|
| Build (debug) | `cargo build` → `target/debug/zemacs` |
| Run | `cargo run -p zemacs-app` (or `cargo run --bin zemacs`), `-- FILE` to open one |
| Headless / scripted | `target/debug/zemacs --control` (add `--show` for a real window) |
| Install | `scripts/install.sh` → release build, `~/.local/bin/zemacs`, `~/Applications/zemacs.app` |
| App bundle only | `scripts/bundle.sh [--release]` → `target/zemacs.app` |

The binary reads `runtime/` **from the checkout it was built from** (a path
compiled in; `$ZEMACS_RUNTIME` overrides). So a Lisp change takes effect on
restart (or `M-x reload-config`), with no rebuild; a Rust change needs
`cargo build`, and for the installed app `scripts/install.sh` and a restart.
Skew between the two: `check-required-primitives` in `runtime/library.lisp`
warns only for the names it lists; an unknown `%query` answers `NIL` silently,
and an unknown `%do` verb says `no such command: VERB` in the status line.

Environment: `ZEMACS_INIT`, `ZEMACS_RUNTIME`, `ZEMACS_FONT`,
`ZEMACS_PROSE_FONT`, `ZEMACS_PERF` (all in `README.org`), `ZEMACS_SSH` (ssh
client), `ZEMACS_BIN` (for `zemacs_ctl.py`), `ZEMACS_TRAMP_TEST_HOST` /
`_LOCALHOST` (opt-in live tramp tests).

## Test

- One Lisp integration file — the normal unit of testing:
  `cargo test -p zemacs-lisp --test pdf_mode`
- One crate's unit tests: `cargo test -p zemacs-core --lib [-- name_filter]`
- Protocol end to end: `cargo test -p zemacs-app --test control`
- `cargo test --workspace` takes 10+ minutes on the owner's 8 GB machine. Run
  what the change touches.

Every new primitive, reader or verb gets a new file in `crates/lisp/tests/` —
one `#[test]` per file, because `cl_boot` is process-wide. Copy
`image_mode.rs`. Layout, helpers (`wait`, `pump`, `feed`, tagged `says`) and
pitfalls: `docs/testing.org`.

## Verify in the real editor, headless

`scripts/zemacs_ctl.py` speaks the `--control` protocol (`docs/control-mode.md`):

```python
import sys; sys.path.insert(0, 'scripts')
from zemacs_ctl import Zemacs
with Zemacs(binary='target/debug/zemacs') as z:   # waits for `ready`
    z.eval('(progn (create-buffer "*probe*") (message "READY"))')  # starts on the dashboard
    z.wait_for_message(lambda t: t == "READY")
    z.keys("i h i <esc>")
    z.eval('(message (format nil "PROBE ~s" (buffer-string)))')
    print(z.wait_for_message(lambda t: t.startswith("PROBE"))["text"])  # PROBE "hi"
    print([m for m in z.messages()["messages"] if "not loaded" in m])
    z.screenshot('/tmp/shot.png')                 # works headless
```

- `eval` is asynchronous (answers `{'queued': True}`): read a value through a
  tagged `message`, with `wait_for_message` or `state()["status"]`.
- `keys` takes space-separated tokens (`<esc>`, `<ret>`, `<tab>`, `SPC`, `C-x`);
  a bare `"ESC"` is split into the letters `E S C`.
- It loads the owner's real `~/.zemacs.d/init.lisp`; set
  `ZEMACS_INIT=$PWD/runtime/init.lisp` to test the shipped defaults.
- Bare `--control` runs SDL's dummy video driver, which polls: fine for
  behaviour, wrong for timing. Measure with `--control --show` (window
  frontmost, script in the foreground) and `ZEMACS_PERF=1`.

## Gotchas

- **A runtime module with a reader error is silently skipped.** The editor
  starts; the only trace is `"<file>: not loaded — <error>"` in the message
  log (`M-x messages-buffer`, `SPC h m`, `z.messages()`). The mode then does
  not exist and its keys fall through to evil bindings.
- **Text is characters.** Buffer text reaches Lisp decoded (`f_query` /
  `utf8_string` in `shim.c`); offsets are character indices. Never add a UTF-8
  decode "to be safe" — it double-encodes. Comments mentioning `utf8-text`
  describe history; that function is gone.
- **A Lisp function is not atomic** and app-bound commands land a frame later:
  `(find-file x)` then `(buffer-name)` still names the old buffer. Use one
  `replace-region`, or `with-undo-group` for undo. `docs/threading.org`.
- **A mode keymap is last-writer-wins.** Config keys for a mode go *below*
  `(load-runtime-modules)`. `define-derived-mode` generates `X-hook`; a
  hand-written `(defun X-hook ...)` replaces it and detaches the machinery.
- **Generated buffers are read-only**: `Editor::apply` refuses the edit with a
  status message, not a Lisp error.
- **The image runs one form at a time.** A slow or looping form blocks every
  later hook and binding (not input or drawing), and nothing can interrupt it.
  Never use `mp:interrupt-process` (SIGBUS); see `docs/threading.org`.
- **stdout is the protocol under `--control`.** Diagnostics go to stderr.
- **Tests:** untagged `says` can pass vacuously; `tutor.rs` flakes under load
  (3 s answer timeout). Both in `docs/testing.org`.

## Conventions

- Comments are prose explaining **why** — the constraint, the measurement, the
  failure that motivated it — not what the next line does. Match that density.
- Lisp docstrings are user-facing: `M-x` and which-key show them. Every new
  command gets one.
- **`ponytail:`** marks a deliberate simplification, naming its ceiling and
  upgrade path at the site (e.g. above `spawn_worker` in
  `crates/tramp/src/lib.rs`). Ceilings a Lisp author can feel are also listed
  in `docs/boundary.org`. Find them all: `grep -rn 'ponytail:' crates runtime`.
- Test files start with a `//!` header saying what they pin; test names are
  sentences (`a_pdf_opens_as_pages_that_scroll_into_one_another`).
- Commit subjects say what changed for the user (`pdf: read PDFs in a buffer,
  opened from dired or anywhere a file opens`); the body says why.

## Documentation is part of the change

**Rule: any big change updates the relevant documentation in the same change.**
"Big" means any of: a new feature or mode; a new primitive, reader or `%do`
verb; a new crate or top-level directory; a changed command, key or default; a
changed architecture rule; or anything that makes an existing doc wrong. Docs
that disagree with the code are bugs: when you find one, fix it in your change
or, if the fix is not yours to make, note it in `TODO.org`.

| Kind of change | Update |
|---|---|
| New or changed primitive, reader, `%do` verb or Lisp helper | `docs/reference.org` (its feature's section; new verbs also in the `%do` table under "The escape hatch"); `docs/boundary.org` if it moves the Lisp/Rust line or opens or closes a ceiling |
| New runtime module | its entry in the comment above `*runtime-modules*` in `runtime/library.lisp`, plus a `docs/reference.org` section |
| User-facing command or key | its `docs/reference.org` section, the command's docstring, and `README.org` if it is in the key table there |
| Scenes / pixel GUI | `docs/gui.org` |
| Threads, locking, the Lisp thread's contract | `docs/threading.org`, and the thread table in `docs/architecture.org` |
| Crate graph, keypress path, dispatch, boot order | `docs/architecture.org` |
| Control protocol (ops, events, flags) | `docs/control-mode.md` (and `scripts/zemacs_ctl.py`) |
| Editor model: buffer kinds, windows, frames, modes, modal states, hooks | `docs/model.org` |
| Curriculum format | `docs/curriculum.org` (and `examples/math/` if the format changes) |
| Test layout, helpers, a new flake | `docs/testing.org` |
| New crate, directory, build/test/install command, env var, or gotcha | this file (and the crate table in `docs/architecture.org`) |
| Deliberate ceiling | a `ponytail:` comment at the site; `docs/boundary.org` if a Lisp author feels it; `TODO.org` if it is planned work |
| Finishing or pausing tracked work | its `TODO.org` entry |

## Documentation map

| Doc | Read it when |
|---|---|
| `docs/architecture.org` | before touching Rust: crates, threads, one keystroke end to end, how Lisp reaches the editor |
| `docs/boundary.org` | deciding Lisp vs Rust, adding a reader/verb/primitive, or hitting a known ceiling |
| `docs/threading.org` | writing any Lisp command longer than a line; changing the Lisp thread or locking |
| `docs/model.org` | working with buffers, windows, frames, major/minor modes, modal states or hooks |
| `docs/reference.org` | looking up or documenting a Lisp primitive, reader or helper |
| `docs/recipes.org` | writing config-style Lisp: commands, hooks, minor modes, themes, prompts |
| `docs/gui.org` | working on scenes (`runtime/gui.lisp`, `crates/gui`, scene drawing) |
| `docs/curriculum.org` | touching `runtime/modes/math*.lisp` or curriculum files |
| `docs/control-mode.md` | driving the editor headless or changing `--control` |
| `docs/testing.org` | writing or debugging tests |
| `docs/README.org` | the Lisp reference's front page and reading order |
| `README.org` | the user-facing overview, keys and environment overrides |
| `TODO.org` | before starting work: what is done, open or paused, and the reasoning |

## Local tooling (the owner's machine, not the repo)

A hook rewrites `cargo` to `rtk cargo`, which collapses test output to a
summary and hides panics and diffs; run `rtk proxy cargo test ...` to see
everything. A `tokensave` code index (`.tokensave/`, git-excluded) may be
available for code search. Neither is required by anything in the repo.
