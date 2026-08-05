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
fn every_theme_sets_every_face() {
    for path in themes() {
        let src = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        for kind in HlKind::ALL {
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
