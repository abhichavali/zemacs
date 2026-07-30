//! zemacs — the application.
//!
//! Owns the SDL2 event loop, the single mutable `Editor`, and the renderer;
//! spawns the Common Lisp image and drains its commands each frame.
//!
//! Command flow, in one place: keyboard input and Lisp both produce
//! [`EditorCommand`]s, and every one of them goes through [`dispatch`]. Most
//! land in `Editor::apply` (the single document writer); three are *effects*
//! the pure core cannot perform — evaluating Lisp, reading a file, writing a
//! file — and this layer performs them.

use std::path::{Path, PathBuf};

use crossbeam_channel::{Receiver, Sender};
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::{Keycode, Mod};
use sdl2::mouse::MouseWheelDirection;

use zemacs_core::{Editor, EditorCommand, Key, PromptKind};
use zemacs_lisp::Lisp;
use zemacs_render::Renderer;

const RECENT_LIMIT: usize = 10;
const RECENT_ON_DASHBOARD: usize = 5;
/// Lines per wheel notch.
const SCROLL_LINES: i32 = 3;

fn main() -> anyhow::Result<()> {
    let sdl = sdl2::init().map_err(|e| anyhow::anyhow!("SDL init: {e}"))?;
    let mut renderer = Renderer::new(&sdl, "zemacs", 1100, 760)?;
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
    seed_dashboard(&mut editor, &init_path, &renderer.backend());

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

    'main: loop {
        keys.clear();
        for event in pump.poll_iter() {
            match event {
                Event::Quit { .. } => break 'main,
                Event::Window {
                    win_event: WindowEvent::Resized(..) | WindowEvent::SizeChanged(..),
                    ..
                } => {}
                Event::KeyDown {
                    keycode: Some(kc),
                    keymod,
                    ..
                } => {
                    swallow_text = keymod.intersects(Mod::LALTMOD | Mod::RALTMOD);
                    keys.extend(key_from_keydown(kc, keymod, !text_input_on));
                }
                // SDL reports natural-scroll wheels with `direction: Flipped`
                // and the raw sign, so undo that first; then negate, because a
                // wheel push (positive y) moves the view *up* the file.
                Event::MouseWheel { y, direction, .. } => {
                    let y = match direction {
                        MouseWheelDirection::Flipped => -y,
                        _ => y,
                    };
                    if y != 0 {
                        editor.apply(EditorCommand::ScrollLines(-y * SCROLL_LINES));
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

        for key in keys.drain(..) {
            for cmd in editor.handle_key(key) {
                dispatch(&mut editor, &lisp, cmd, &init_path);
            }
        }
        while let Ok(cmd) = rx.try_recv() {
            dispatch(&mut editor, &lisp, cmd, &init_path);
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

        if editor.should_quit {
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
        refresh_file_completions(&mut editor, &mut last_file_query);

        // Adopt a result only if the buffer hasn't moved on; if it has, a newer
        // parse is already in flight and the current spans stay up meanwhile.
        if let Some((revision, spans)) = highlighter.poll() {
            if revision == editor.revision {
                editor.highlights = spans;
            }
        }

        // `render` syncs the font itself, and the canvas presents on vsync —
        // that is what paces this loop, so there is no sleep here.
        renderer.render(&mut editor)?;
    }
    Ok(())
}

/// The one place a command becomes an action. Everything the pure core can do
/// goes to `apply`; the three effects it can't perform are handled here.
fn dispatch(editor: &mut Editor, lisp: &Lisp, cmd: EditorCommand, init_path: &Path) {
    match cmd {
        EditorCommand::CallLisp(form) => lisp.eval(form),
        EditorCommand::OpenFile(path) => open_file(editor, &path, init_path),
        EditorCommand::SaveFile(path) => save_file(editor, path),
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
}
