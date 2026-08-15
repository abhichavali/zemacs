//! Every shipped theme names every face.
//!
//! Loading a theme does not reset the one before it: the face table is a table,
//! and a theme is a pile of assignments into it. So a face a theme forgets keeps
//! the *previous* theme's colour — and, since faces grew a weight, its bold and
//! its italic too. The symptom is a single stubbornly wrong-coloured token that
//! appears only when you switch themes in one particular order, which is about
//! as hard to catch by looking as a bug gets.
//!
//! Hence a text scan rather than an evaluation. Loading eleven themes into a
//! real ECL image would prove more, but it would also need an editor to load
//! them into; what actually goes wrong here is a face name left out or misspelt,
//! and that is visible in the source. ponytail: a theme that computed its face
//! names — a `dolist` over a list of pairs — would read as zero mentions and
//! fail this. None does, and the day one does is the day this test earns its
//! upgrade to an evaluation.

use std::path::{Path, PathBuf};

use zemacs_core::HlKind;

fn themes() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/themes");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "lisp"))
        .collect();
    // Sorted so a failure names the same theme on every machine.
    out.sort();
    out
}

/// The face name as a theme writes it: `"keyword"`, quotes and all. Matching on
/// the bare word would count the prose in the header comments, which talk about
/// `keyword' and `type' constantly.
fn quoted(kind: HlKind) -> String {
    format!("\"{}\"", kind.name())
}

#[test]
fn there_are_themes_to_check() {
    // The scan below passes vacuously if the directory is empty or moves, and a
    // test that cannot fail is worse than no test.
    assert!(themes().len() >= 3, "{:?}", themes());
}

/// `load-theme` reports a missing theme with `message` and carries on, which is
/// right at the prompt — a typo at `M-x theme' should not be an error dialog —
/// and useless for the one in the shipped config, where the same forgiveness
/// means booting into the fallback palette with a line in the message log
/// nobody reads.
#[test]
fn every_theme_the_config_loads_exists() {
    let init = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/init.lisp");
    let src = std::fs::read_to_string(&init).unwrap();
    let names: Vec<&str> = src
        .match_indices("(load-theme \"")
        .filter_map(|(i, m)| src[i + m.len()..].split('"').next())
        .collect();
    assert!(!names.is_empty(), "no theme is loaded at all");
    for name in names {
        let want = format!("{name}.lisp");
        assert!(
            themes().iter().any(|p| p.file_name().unwrap() == want.as_str()),
            "init.lisp loads {name:?}, which is not in runtime/themes"
        );
    }
}

#[test]
fn every_theme_sets_every_core_face() {
    for path in themes() {
        let src = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        for &kind in HlKind::CORE {
            let n = src.matches(&quoted(kind)).count();
            // Exactly once, not merely at least once: a face assigned twice is a
            // theme where one of the two lines is dead and nobody can tell which
            // by reading it.
            assert_eq!(
                n,
                1,
                "{name}: face {:?} appears {n} times, want 1",
                kind.name()
            );
        }
    }
}

/// The UI faces — `cursor`, `region`, `popup` and the rest of `ALL` past the
/// core 22 — are *optional*, and that is the whole difference between this test
/// and the one above.
///
/// Optional because they fall back to the ratio the renderer mixed before they
/// existed: `mix(bg, fg, 0.28)` for a region, `mix(bg, fg, 0.85)` for a cursor.
/// A theme that names none of them looks exactly as it did, which is what let
/// the cursor become themeable without eleven files being rewritten. The bleed
/// that omission would normally cause — theme A's cursor surviving into theme B
/// — is stopped by `load-theme` emptying the table first (`%forget-faces`), not
/// by insisting here.
///
/// So the only thing left to check is the *other* half of the rule above: named
/// twice is a theme with a dead line in it, and that is a mistake whether the
/// face is required or not.
#[test]
fn no_theme_names_a_ui_face_twice() {
    for path in themes() {
        let src = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        for kind in HlKind::ALL.into_iter().skip(HlKind::CORE.len()) {
            let n = src.matches(&quoted(kind)).count();
            assert!(
                n <= 1,
                "{name}: face {:?} appears {n} times, want 0 or 1",
                kind.name()
            );
        }
    }
}

/// Every palette line carries its own answer key.
///
/// A theme binds `(bg '(0.102 0.106 0.149))  ; #1a1b26`, and the hex is not
/// decoration: it is the value as its upstream publishes it, and the floats are
/// a conversion somebody did by hand. Which means the floats are where the typo
/// goes — a transposed digit, a component copied from the line above, a `0.53`
/// that should have been `0.35`. Nothing downstream can catch it. The editor
/// takes any three numbers in range perfectly happily, the theme loads, and the
/// result is a port that is *nearly* right, which is the only kind of wrong a
/// palette can be that survives being looked at.
///
/// So: reconvert the hex and compare. A half-step of tolerance, because 3
/// decimals is the house convention and 0x1a/255 = 0.10196… rounds to 0.102,
/// but nothing looser — two colours a full step apart are two different
/// colours, and the whole point of writing the hex down was to be able to say
/// which one was meant.
#[test]
fn every_palette_float_matches_the_hex_beside_it() {
    // `(name '(r g b))  ; #rrggbb` — the one shape every theme's palette uses.
    for path in themes() {
        let src = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let mut checked = 0usize;
        for (lineno, line) in src.lines().enumerate() {
            let Some((code, comment)) = line.split_once(';') else {
                continue;
            };
            // The *first* six-digit hex in the comment is the binding's own
            // value; anything after it is provenance. Several themes show their
            // work — `#282c3c  #717cb4 at 15%` for a blend, `#9d9d9d (from
            // #9d9d9daa)` for a composite — and the answer always leads, which
            // is the one rule that reads the same in all of them.
            //
            // Six digits exactly, so the eight-digit form with an alpha byte on
            // the end is skipped rather than misread. Reading eight and
            // comparing three of them is how the alpha ends up in a colour
            // channel, which is the bug this test found on its first run:
            // `modus-vivendi-tinted`'s `fg-dim` had a blue of 0.667 — the `aa` —
            // on a grey whose other two components were 0.616.
            let Some(hex) = comment
                .split('#')
                .skip(1)
                .map(|s| s.chars().take_while(char::is_ascii_hexdigit).collect::<String>())
                .find(|s| s.len() == 6)
            else {
                continue;
            };
            let Some(floats) = code.split_once("'(").and_then(|(_, r)| r.split_once(')')) else {
                continue;
            };
            let got: Vec<f32> = floats.0.split_whitespace().filter_map(|w| w.parse().ok()).collect();
            if got.len() != 3 {
                continue;
            }
            checked += 1;
            for i in 0..3 {
                let byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
                let want = byte as f32 / 255.0;
                assert!(
                    (got[i] - want).abs() <= 0.0005 + f32::EPSILON,
                    "{name}:{}: #{hex} component {i} is {want:.4}, palette says {}\n  {}",
                    lineno + 1,
                    got[i],
                    line.trim()
                );
            }
        }
        // A theme whose palette this shape does not fit passes vacuously, and a
        // vacuous pass on all eleven of them is how the test quietly stops
        // existing. Every shipped theme writes its palette this way; the day one
        // does not is a day to decide on purpose, not to find out from a screen.
        assert!(checked >= 8, "{name}: only {checked} palette lines had a hex to check against");
    }
}
