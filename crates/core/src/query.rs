//! Reading editor state from Lisp.
//!
//! The write half of the bridge has always existed: a Lisp primitive emits an
//! [`EditorCommand`](crate::EditorCommand) and the app applies it. This is the
//! other direction, and it is what turns the image from a configuration DSL
//! into an extension language — `(point)`, `(region)` and `(buffer-substring)`
//! are what a command like "bold the selection" is made of.
//!
//! ponytail: every answer is a *string of Common Lisp source* that the caller
//! READs, rather than a tagged union marshalled through C. One C signature
//! (`const char *query(name, a, b)`) then serves integers, strings, `NIL`,
//! conses and lists alike, and adding a query is one arm of one `match`. The
//! ceiling is arguments: two integers is all a query gets. Anything needing a
//! string argument is a *command*, and commands already have a channel.
//!
//! Unknown names answer `NIL` rather than erroring, so a config written against
//! a newer zemacs degrades instead of failing to load.

use crate::{CompletionStyle, Editor, HlKind, LineOverflow, Mode};

/// Answer `name`, with up to two integer arguments, as readable Lisp source.
pub fn query(ed: &Editor, name: &str, a: i64, b: i64) -> String {
    let buf = &ed.buffer;
    let n = buf.len_chars();
    let (line, col) = buf.cursor_line_col();
    // A 1-based line number, spelled the way `line-number` answers one. Zero is
    // "the line point is on", which is what the zero-argument call sends — so
    // `(line-start)` keeps meaning what it always did and `(line-start 7)` is new.
    let at = |a: i64| match a {
        v if v > 0 => ((v - 1) as usize).min(buf.len_lines().saturating_sub(1)),
        _ => line,
    };
    match name {
        "point" => buf.cursor.to_string(),
        "point-min" => "0".into(),
        "point-max" => n.to_string(),
        "line-number" => (line + 1).to_string(), // 1-based, as in Emacs
        "column" => col.to_string(),             // 0-based, as in Emacs
        "line-count" => buf.len_lines().to_string(),
        "line-start" => buf.line_start(at(a)).to_string(),
        "line-end" => buf.line_end(at(a)).to_string(),

        "buffer-string" => string(&buf.slice_string(0, n)),
        "buffer-substring" => {
            let (start, end) = clamp(a, b, n);
            string(&buf.slice_string(start, end))
        }
        "line-string" => {
            let l = at(a);
            string(&buf.slice_string(buf.line_start(l), buf.line_end(l)))
        }
        "buffer-size" => n.to_string(),
        "buffer-name" => string(&buf.name()),
        "buffer-file-name" => match &buf.path {
            Some(p) => string(&p.to_string_lossy()),
            None => "nil".into(),
        },
        "buffer-modified-p" => boolean(buf.modified),
        "buffer-read-only-p" => boolean(buf.kind.is_generated()),
        // clipboard: vim's `""`, which is also the system clipboard — writing
        // it has always worked (`copy-region`, `set-register`), and this is the
        // read that `boundary.org` listed as missing. `(TEXT . LINEWISE-P)`
        // rather than two readers, because a linewise register pastes on its
        // own line and text that forgot which it was pastes wrong.
        "register" => match ed.register() {
            ("", _) => "nil".into(),
            (text, linewise) => format!("({} . {})", string(text), boolean(linewise)),
        },
        "buffer-list" => list(ed.buffer_names().iter().map(|s| string(s))),
        // The switcher's list carries a `[+]` on a modified buffer, which is
        // right for a menu and wrong for anything that has to *match* a name.
        // This is the same buffers with their fields apart:
        // `(NAME FILE MODIFIED-P MAJOR-MODE)`, live buffer first.
        "buffer-info" => list(
            std::iter::once(buf)
                .chain(ed.others.iter())
                .map(|b| {
                    format!(
                        "({} {} {} {})",
                        string(&b.name()),
                        match &b.path {
                            Some(p) => string(&p.to_string_lossy()),
                            None => "nil".into(),
                        },
                        boolean(b.modified),
                        string(&b.major_mode),
                    )
                }),
        ),

        "major-mode" => string(&buf.major_mode),
        "minor-modes" => list(buf.minor_modes.iter().map(|m| string(m))),
        "evil-state" => string(state_name(ed.mode)),
        // The keymaps the *next* key will be looked up in, nearest first —
        // `Editor::keymaps` spelled as a list, and usually just `(evil-state)`.
        //
        // It is more than that exactly where dired, magit and the dashboard
        // *layer over* Normal: `SPC` in a listing reaches the leader map, so
        // anything reasoning about what a key will do there has to know that
        // Normal is still in reach. which-key is the reason this exists — it
        // was asked about a pending prefix on the dashboard, looked only in the
        // dashboard's own map, found nothing, and said nothing.
        "evil-keymaps" => list(ed.keymaps().map(|m| string(state_name(m)))),

        // `(BEG . END)`, or NIL with no selection — so `(when (region) ...)`
        // reads the way `(when (use-region-p) ...)` does in Emacs.
        "region" => match ed.selection() {
            Some((start, end)) => format!("({start} . {end})"),
            None => "nil".into(),
        },
        // Block mode selects several disjoint runs; this is all of them.
        "region-ranges" => list(
            ed.selection_ranges()
                .iter()
                .map(|(s, e)| format!("({s} . {e})")),
        ),

        // `a` is a marker handle. NIL once it has been deleted, or while the
        // buffer it names is not the live one, so a holder that kept it too long
        // learns that rather than being handed a position in someone else's text.
        "marker-position" => match u64::try_from(a).ok().and_then(|id| ed.marker_position(id)) {
            Some(p) => p.to_string(),
            None => "nil".into(),
        },

        // --- overlays -------------------------------------------------------
        //
        // The read half. Everything that *changes* an overlay is a command, and
        // making one is `Editor::make_overlay`, because it has to answer with a
        // handle. Same contract as `marker-position`: NIL once it is gone, or
        // while the buffer it belongs to is not the live one.
        "overlay-position" => match u64::try_from(a).ok().and_then(|id| ed.overlay_span(id)) {
            Some((start, end)) => format!("({start} . {end})"),
            None => "nil".into(),
        },
        // Overlapping `[a, b)`, oldest first — which is also the order the
        // renderer resolves them in, so a caller reading this sees them in
        // priority order. `(ID START END)` each, so the common walk needs no
        // second query per overlay.
        "overlays-in" => {
            let (start, end) = clamp(a, b, n);
            list(ed
                .overlays_in(start, end)
                .iter()
                .map(|(id, s, e)| format!("({id} {s} {e})")))
        }

        "window-scroll" => ed.scroll.to_string(),
        "window-height" => ed.viewport_lines.to_string(),
        "frame-count" => ed.frames.len().to_string(),
        // Windows of the *focused* frame — the only one a Lisp command can act
        // on, since `select-window` addresses ids within it. `(ID NAME)` each,
        // in the order the windows were created rather than in tree order:
        // `select-window` takes the id, so the order is a listing detail.
        "window-list" => {
            let frame = ed.frame();
            list(frame.windows.iter().map(|w| {
                let name = ed.buffer_by_id(w.buffer).map(|b| b.name()).unwrap_or_default();
                format!("({} {})", w.id, string(&name))
            }))
        }
        "window-id" => ed.frame().current.to_string(),
        "frame-index" => ed.focus_frame.to_string(),

        // --- introspection: what this editor knows about itself ---
        //
        // Both keymaps in one list, as `(MODE KEYS COMMAND)`. MODE is either an
        // editing state ("normal") or a major/minor mode name ("org-mode") —
        // exactly the two things `define-key` accepts, so a row can be fed
        // straight back to it. Sorted, because a HashMap has no order and a
        // dashboard that reshuffles itself every redraw is unreadable.
        "key-bindings" => {
            let mut rows: Vec<(String, String, String)> = ed
                .keymap
                .iter()
                .map(|((m, k), c)| (state_name(*m).to_string(), k.clone(), c.clone()))
                .chain(
                    ed.mode_keymap
                        .iter()
                        .map(|((m, k), c)| (m.clone(), k.clone(), c.clone())),
                )
                .collect();
            rows.sort();
            list(rows
                .iter()
                .map(|(m, k, c)| format!("({} {} {})", string(m), string(k), string(c))))
        }
        "command-list" => list(ed.commands.iter().map(|c| string(c))),
        "status" => string(&ed.status),
        // `a` messages from the end, or all of them at 0 — the log is capped at
        // 500 and a status-line widget wants the last few, not all of it.
        "messages" => {
            let from = match a {
                v if v > 0 => ed.messages.len().saturating_sub(v as usize),
                _ => 0,
            };
            list(ed.messages[from..].iter().map(|m| string(m)))
        }

        // --- settings ---
        //
        // The write half of every one of these has existed since the beginning;
        // this is the read half, and it closes the hole `runtime/modes/modes.lisp`
        // works around by wrapping the setter to remember what it was handed.
        "font-size" => float(ed.settings.font_size),
        "tab-width" => ed.settings.tab_width.to_string(),
        "line-numbers-p" => boolean(ed.settings.line_numbers),
        "relative-line-numbers-p" => boolean(ed.settings.relative_line_numbers),
        "modeline-relief" => ed.settings.modeline_relief.to_string(),
        "modeline-pad" => ed.settings.modeline_pad.to_string(),
        "line-overflow" => string(match ed.settings.line_overflow {
            LineOverflow::Truncate => "truncate",
            LineOverflow::Wrap => "wrap",
        }),
        "completion-style" => string(match ed.settings.completion_style {
            CompletionStyle::Minibuffer => "minibuffer",
            CompletionStyle::Bottom => "bottom",
            CompletionStyle::Center => "center",
        }),
        "background" => rgb(ed.settings.background),
        "foreground" => rgb(ed.settings.foreground),
        // A face is named by a *string* everywhere else, and a query only takes
        // integers — so `a` is an index into `face-list`, and the Lisp wrapper
        // does the lookup. That keeps the vocabulary in one place: here.
        "face-list" => list(HlKind::ALL.iter().map(|k| string(k.name()))),
        "syntax-color" => match usize::try_from(a).ok().and_then(|i| HlKind::ALL.get(i)) {
            Some(k) => rgb(ed.theme.color(*k, ed.settings.foreground)),
            None => "nil".into(),
        },

        _ => "nil".into(),
    }
}

fn state_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "normal",
        Mode::Insert => "insert",
        Mode::Visual => "visual",
        Mode::VisualLine => "visual-line",
        Mode::VisualBlock => "visual-block",
        Mode::Dashboard => "dashboard",
        Mode::Magit => "magit",
        Mode::Dired => "dired",
        Mode::Terminal => "terminal",
    }
}

/// A Lisp string literal. Only `\` and `"` are special inside one — a newline
/// stands for itself, so unlike C or Rust there is nothing else to escape.
fn string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// lisp-api: the same escaping, for a caller outside this module. `evil.rs` has
/// to spell a prompt's answer as source before handing it to the image, and
/// there is no second right way to write a Lisp string literal.
pub(crate) fn lisp_string(s: &str) -> String {
    string(s)
}

fn boolean(b: bool) -> String {
    if b { "t" } else { "nil" }.into()
}

/// `{:?}` rather than `{}` because it always prints the decimal point: `22`
/// would read back as an integer, and `(set-font-size (font-size))` has to
/// round-trip through a primitive that wants a float.
fn float(v: f32) -> String {
    format!("{v:?}")
}

fn rgb(c: [f32; 3]) -> String {
    format!("({} {} {})", float(c[0]), float(c[1]), float(c[2]))
}

fn list(items: impl Iterator<Item = String>) -> String {
    let body: Vec<String> = items.collect();
    format!("({})", body.join(" "))
}

/// Arguments come from Lisp, so they can be anything. Clamp rather than reject:
/// `(buffer-substring 0 most-positive-fixnum)` should mean "to the end".
fn clamp(a: i64, b: i64, len: usize) -> (usize, usize) {
    let to = |v: i64| v.clamp(0, len as i64) as usize;
    let (a, b) = (to(a), to(b));
    (a.min(b), a.max(b))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::EditorCommand;

    fn editor(text: &str) -> Editor {
        let mut ed = Editor::new();
        ed.load(text, None, None);
        ed.apply(EditorCommand::SetMode(Mode::Normal));
        ed
    }

    fn q(ed: &Editor, name: &str) -> String {
        query(ed, name, 0, 0)
    }

    #[test]
    fn position_queries_agree_with_the_cursor() {
        let mut ed = editor("alpha\nbeta\ngamma\n");
        ed.buffer.move_to_line_col(1, 2);
        assert_eq!(q(&ed, "point"), "8");
        assert_eq!(q(&ed, "line-number"), "2"); // 1-based, like Emacs
        assert_eq!(q(&ed, "column"), "2"); // 0-based, like Emacs
        assert_eq!(q(&ed, "line-start"), "6");
        assert_eq!(q(&ed, "line-end"), "10");
        assert_eq!(q(&ed, "line-string"), "\"beta\"");
        assert_eq!(q(&ed, "point-max"), "17");
    }

    #[test]
    fn substring_arguments_are_clamped_and_ordered() {
        let ed = editor("hello");
        assert_eq!(query(&ed, "buffer-substring", 0, 4), "\"hell\"");
        // reversed
        assert_eq!(query(&ed, "buffer-substring", 4, 0), "\"hell\"");
        // past the end, and before the start
        assert_eq!(query(&ed, "buffer-substring", 0, 9999), "\"hello\"");
        assert_eq!(query(&ed, "buffer-substring", -50, 2), "\"he\"");
    }

    /// The whole point of returning source: it has to survive READ.
    #[test]
    fn strings_are_escaped_for_the_reader() {
        let ed = editor(r#"say "hi\there""#);
        assert_eq!(q(&ed, "buffer-string"), r#""say \"hi\\there\"""#);
        // ...and a newline needs no escape in a Lisp string literal
        let ed = editor("a\nb");
        assert_eq!(q(&ed, "buffer-string"), "\"a\nb\"");
    }

    /// which-key's bug in one assertion: on the dashboard the leader map is
    /// still in reach, so anything asking "what will the next key do here" has
    /// to be told about Normal as well as about the mode's own map.
    #[test]
    fn a_layered_mode_reports_normal_as_a_keymap_too() {
        let mut ed = editor("x");
        ed.apply(EditorCommand::SetMode(Mode::Normal));
        assert_eq!(q(&ed, "evil-keymaps"), r#"("normal")"#);
        ed.apply(EditorCommand::SetMode(Mode::Dashboard));
        assert_eq!(q(&ed, "evil-keymaps"), r#"("dashboard" "normal")"#);
        // Insert and the visual modes replace Normal rather than layering over
        // it, so they answer only for themselves.
        ed.apply(EditorCommand::SetMode(Mode::Visual));
        assert_eq!(q(&ed, "evil-keymaps"), r#"("visual")"#);
    }

    #[test]
    fn region_is_nil_until_something_is_selected() {
        let mut ed = editor("abcdef");
        assert_eq!(q(&ed, "region"), "nil");
        ed.apply(EditorCommand::SetMode(Mode::Visual));
        ed.apply(EditorCommand::MoveTo(3));
        // Inclusive of the character under the cursor, as Visual mode draws it,
        // so this is "abcd" and not "abc".
        assert_eq!(q(&ed, "region"), "(0 . 4)");
        assert_eq!(query(&ed, "buffer-substring", 0, 4), "\"abcd\"");
        assert_eq!(q(&ed, "region-ranges"), "((0 . 4))");
        assert_eq!(q(&ed, "evil-state"), "\"visual\"");
    }

    #[test]
    fn buffer_identity_is_readable() {
        let ed = editor("x");
        assert_eq!(q(&ed, "buffer-file-name"), "nil");
        assert_eq!(q(&ed, "major-mode"), "\"fundamental-mode\"");
        assert_eq!(q(&ed, "minor-modes"), "()");
        assert!(q(&ed, "buffer-list").starts_with('('));
        assert_eq!(q(&ed, "buffer-modified-p"), "nil");
    }

    /// clipboard: an empty register is NIL rather than `("" . NIL)`, so
    /// `(when (register) ...)` reads the way every other optional does here.
    #[test]
    fn the_register_reads_as_text_and_its_linewise_ness() {
        let mut ed = editor("abcd\n");
        assert_eq!(q(&ed, "register"), "nil");

        ed.apply(crate::EditorCommand::Yank {
            start: 0,
            end: 4,
            linewise: false,
        });
        assert_eq!(q(&ed, "register"), "(\"abcd\" . nil)");

        // What the app layer does with text arriving from outside. The newline
        // goes through raw, which READs as itself — Lisp string literals span
        // lines, so there is nothing here to escape.
        ed.adopt_register("line\n".into(), true);
        assert_eq!(q(&ed, "register"), "(\"line\n\" . t)");
    }

    /// The Lisp contract for a handle: an integer while it names something, and
    /// a real NIL — not the string "nil" — once it does not.
    #[test]
    fn a_marker_reads_as_a_position_until_it_is_deleted() {
        let mut ed = editor("alpha beta");
        let m = ed.make_marker(6, crate::Insertion::Stay);
        assert_eq!(query(&ed, "marker-position", m as i64, 0), "6");
        ed.delete_marker(m);
        assert_eq!(query(&ed, "marker-position", m as i64, 0), "nil");
        // Arithmetic in Lisp can produce anything, including a handle no marker
        // ever had.
        assert_eq!(query(&ed, "marker-position", -1, 0), "nil");
    }

    /// An overlay handle has the same contract, and answers a pair rather than a
    /// position because it names a *range*.
    #[test]
    fn overlays_read_back_as_ranges_and_survive_an_edit() {
        let mut ed = editor("alpha beta gamma");
        let o = ed.make_overlay(6, 10);
        assert_eq!(query(&ed, "overlay-position", o as i64, 0), "(6 . 10)");
        assert_eq!(query(&ed, "overlays-in", 0, 100), format!("(({o} 6 10))"));
        // Touching at a boundary is not overlapping.
        assert_eq!(query(&ed, "overlays-in", 0, 6), "()");
        assert_eq!(query(&ed, "overlays-in", 10, 16), "()");

        // The point of the exercise: it moves with the text.
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::MoveTo(0));
        ed.apply(EditorCommand::InsertText("XX".into()));
        assert_eq!(query(&ed, "overlay-position", o as i64, 0), "(8 . 12)");

        ed.apply(EditorCommand::Overlay(crate::OverlayEdit::Delete(o)));
        assert_eq!(query(&ed, "overlay-position", o as i64, 0), "nil");
        // Arithmetic in Lisp can produce anything, including a handle no overlay
        // ever had — and an empty range, which makes no overlay at all.
        assert_eq!(query(&ed, "overlay-position", -1, 0), "nil");
        assert_eq!(ed.make_overlay(4, 4), 0);
    }

    /// A config written against a newer zemacs must still load.
    #[test]
    fn an_unknown_query_is_nil_rather_than_an_error() {
        let ed = editor("x");
        assert_eq!(q(&ed, "no-such-query"), "nil");
    }

    /// The zero-argument spelling has to keep meaning "the line point is on",
    /// or every existing config breaks the day the argument is added.
    #[test]
    fn the_line_readers_take_an_optional_line_number() {
        let mut ed = editor("alpha\nbeta\ngamma\n");
        ed.buffer.move_to_line_col(1, 2);
        assert_eq!(query(&ed, "line-start", 0, 0), "6");
        assert_eq!(query(&ed, "line-string", 0, 0), "\"beta\"");
        // 1-based, agreeing with `line-number`.
        assert_eq!(query(&ed, "line-string", 1, 0), "\"alpha\"");
        assert_eq!(query(&ed, "line-start", 3, 0), "11");
        assert_eq!(query(&ed, "line-end", 3, 0), "16");
        // A line past the end clamps rather than panicking: the argument came
        // from arithmetic in Lisp and can be anything.
        assert_eq!(query(&ed, "line-string", 9999, 0), "\"\"");
    }

    #[test]
    fn buffer_info_gives_names_that_can_be_matched() {
        let mut ed = editor("x");
        ed.load("y", Some(PathBuf::from("/tmp/notes.org")), Some("org".into()));
        let info = q(&ed, "buffer-info");
        assert!(
            info.starts_with(r#"(("notes.org" "/tmp/notes.org" nil "org-mode")"#),
            "live buffer first, with its file and mode; got {info}"
        );
        // ...and unlike `buffer-list`, the name carries no `[+]` to strip.
        ed.apply(EditorCommand::SetMode(Mode::Insert));
        ed.apply(EditorCommand::InsertText("z".into()));
        assert!(q(&ed, "buffer-list").contains("[+]"));
        assert!(q(&ed, "buffer-info").contains(r#"("notes.org" "/tmp/notes.org" t"#));
    }

    /// A Lisp machine has to be able to ask what it is bound to.
    #[test]
    fn both_keymaps_are_readable_as_one_sorted_list() {
        let mut ed = editor("x");
        ed.apply(EditorCommand::BindKey {
            mode: "org-mode".into(),
            keys: "SPC m b".into(),
            command: "org-bold".into(),
        });
        ed.apply(EditorCommand::BindKey {
            mode: "normal".into(),
            keys: "SPC f f".into(),
            command: "find-file".into(),
        });
        // Sorted, so the two maps interleave by mode name rather than by which
        // HashMap happened to be walked first.
        assert_eq!(
            q(&ed, "key-bindings"),
            r#"(("normal" "SPC f f" "find-file") ("org-mode" "SPC m b" "org-bold"))"#
        );
    }

    #[test]
    fn the_message_log_and_the_command_list_read_back() {
        let mut ed = editor("x");
        for m in ["one", "two", "three"] {
            ed.apply(EditorCommand::Message(m.into()));
        }
        assert_eq!(q(&ed, "messages"), r#"("one" "two" "three")"#);
        assert_eq!(query(&ed, "messages", 2, 0), r#"("two" "three")"#);
        assert_eq!(q(&ed, "status"), "\"three\"");
        ed.apply(EditorCommand::RegisterCommand("lisp-version".into()));
        assert_eq!(q(&ed, "command-list"), "(\"lisp-version\")");
    }

    /// Every setting had a writer and no reader, which is why the mode
    /// machinery in `runtime/modes/` has to wrap the writers to remember what
    /// it asked for. Reading them back is the other half.
    #[test]
    fn settings_read_back_as_the_values_that_were_written() {
        let mut ed = editor("x");
        ed.apply(EditorCommand::SetFontSize(22.0));
        ed.apply(EditorCommand::SetTabWidth(2));
        ed.apply(EditorCommand::SetLineOverflow("wrap".into()));
        ed.apply(EditorCommand::SetCompletionStyle("center".into()));
        ed.apply(EditorCommand::SetBackground([0.5, 0.25, 0.0]));
        // A decimal point, always: an integer here would not survive being fed
        // back to `set-font-size`.
        assert_eq!(q(&ed, "font-size"), "22.0");
        assert_eq!(q(&ed, "tab-width"), "2");
        assert_eq!(q(&ed, "line-overflow"), "\"wrap\"");
        assert_eq!(q(&ed, "completion-style"), "\"center\"");
        assert_eq!(q(&ed, "background"), "(0.5 0.25 0.0)");
        assert_eq!(q(&ed, "line-numbers-p"), "t");
    }

    /// A face is named by a string everywhere else, but a query only takes
    /// integers — so the name is looked up in `face-list` and the index comes
    /// back here. The two have to agree or the lookup silently reads the wrong
    /// colour.
    #[test]
    fn a_face_colour_is_addressed_by_its_position_in_the_face_list() {
        let mut ed = editor("x");
        ed.apply(EditorCommand::SetSyntaxColor("string".into(), [0.0, 1.0, 0.5]));
        let faces = q(&ed, "face-list");
        let at = faces
            .split_whitespace()
            .position(|f| f.trim_matches(['(', ')', '"']) == "string")
            .expect("`string` must be in face-list");
        assert_eq!(query(&ed, "syntax-color", at as i64, 0), "(0.0 1.0 0.5)");
        assert_eq!(query(&ed, "syntax-color", 9999, 0), "nil");
    }

    #[test]
    fn windows_are_enumerable_with_the_buffers_they_show() {
        let mut ed = editor("x");
        assert_eq!(q(&ed, "window-id"), "0");
        assert_eq!(q(&ed, "frame-index"), "0");
        ed.apply(EditorCommand::SplitWindow(crate::frame::Split::Columns));
        let windows = q(&ed, "window-list");
        assert_eq!(windows.matches("(").count(), 3, "two windows: {windows}");
        // The split focuses the new window, so this is the id `select-window`
        // would need to get back.
        assert_eq!(q(&ed, "window-id"), "1");
    }
}
