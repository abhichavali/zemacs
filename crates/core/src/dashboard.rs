//! The startup screen.
//!
//! Content is *data*, not layout: a banner, two lists of rows, and a footer. The
//! renderer decides where to put them, and Lisp decides what the second list is
//! — `init.lisp` can `clear-dashboard-items` and build its own from scratch.
//!
//! Recent files live in their own list precisely so that clearing works:
//! `clear-dashboard-items` is a config verb about *configured* items, and it
//! would be a nasty surprise if calling it silently cost you your history.
//!
//! # Why [`Row`] is an enum and not a string
//!
//! This used to hand the renderer `Vec<(String, bool)>` — a finished line and a
//! flag saying whether it was the selected one — and everything else was
//! recovered by looking at the characters. The key was baked into the label as
//! `[f]`, the marker was a `▸` glued to the front, and telling banner art from
//! prose was a *character-range test* on the line.
//!
//! That is the design that produces the screen it produced. A line that has
//! already been assembled can only be drawn in one colour and can only be
//! aligned by its own width, so the menu was centred row by row and came out
//! ragged down its left edge, with no way to tint a key differently from the
//! label it belongs to. Handing over the pieces instead costs one enum and buys
//! the whole layout: a block aligned as a block, a key in the accent colour, a
//! heading in bold, and a keybinding hint set flush right.

/// One selectable row. `action` is either a built-in verb (`quit`,
/// `find-file`, `scratch`, `config`, `open:<path>`) or the name of a Lisp
/// function, called as `(name)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub key: char,
    pub label: String,
    pub action: String,
    /// How to reach the same command once the dashboard is gone — `"SPC f f"`.
    /// Empty when there is no global binding, which is most recent files.
    ///
    /// A hint rather than a binding: nothing here *makes* the sequence work,
    /// and `init.lisp` fills these in from the leader table it built rather
    /// than from a second list of key names that could drift out of step.
    pub hint: String,
}

/// One line of the screen, in pieces, so the renderer can align and colour it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// Prose above the menu, centred line by line: the tagline, the epigraph,
    /// the version. Centred *individually* because it is prose and a ragged
    /// edge is what centred prose is supposed to look like.
    Banner(String),
    /// A section title. Part of the menu block, so it aligns with the rows
    /// under it rather than with the banner above it.
    Heading(&'static str),
    Blank,
    Item {
        key: char,
        label: String,
        hint: String,
        selected: bool,
    },
    /// Where the config lives, and what is running. The one genuinely secondary
    /// thing on the screen.
    Footer(String),
}

/// Above the recent files. Uppercase because these are labels for the eye to
/// skip past on the way to the rows, and a run of small capitals reads as
/// structure at a glance where title case reads as another sentence.
const RECENT: &str = "RECENT";
/// Above the configured items.
const START: &str = "START";

pub struct Dashboard {
    pub banner: String,
    /// A picture drawn above the banner, or `None` for the text alone.
    ///
    /// An [`ImageId`](crate::ImageId) rather than a path, so the dashboard
    /// borrows the image machinery inline figures already use — one decoder,
    /// one cache, one texture lifetime — instead of learning to read files.
    /// Lisp makes the id with `image-file` and hands it over, which is also
    /// what keeps *which* picture a matter of configuration.
    pub logo: Option<crate::ImageId>,
    /// Recently opened files. Owned by the editor, seeded at startup.
    pub recents: Vec<Item>,
    /// Configured rows. Owned by Lisp.
    pub items: Vec<Item>,
    pub selected: usize,
    /// Shown under the rows — set by the app once it knows where config lives.
    pub footer: String,
}

impl Default for Dashboard {
    fn default() -> Self {
        Self {
            banner: DEFAULT_BANNER.into(),
            logo: None,
            recents: Vec::new(),
            items: vec![
                item('f', "Find file", "find-file", "SPC f f"),
                item('s', "Scratch buffer", "scratch", "SPC b s"),
                item('c', "Edit configuration", "config", ""),
                item('q', "Quit", "quit", "SPC q q"),
            ],
            selected: 0,
            footer: String::new(),
        }
    }
}

fn item(key: char, label: &str, action: &str, hint: &str) -> Item {
    Item {
        key,
        label: label.into(),
        action: action.into(),
        hint: hint.into(),
    }
}

impl Dashboard {
    /// Every selectable row, in display order — which is also the order
    /// `selected` indexes.
    pub fn entries(&self) -> Vec<&Item> {
        self.recents.iter().chain(self.items.iter()).collect()
    }

    pub fn len(&self) -> usize {
        self.recents.len() + self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Keep `selected` pointing at a row that exists.
    pub fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.len().saturating_sub(1));
    }

    /// The screen, top to bottom, as pieces for the renderer to arrange.
    pub fn rows(&self) -> Vec<Row> {
        let mut out: Vec<Row> = self
            .banner
            .lines()
            .map(|l| Row::Banner(l.to_string()))
            .collect();

        let mut row = 0;
        for (heading, items) in [(RECENT, &self.recents), (START, &self.items)] {
            if items.is_empty() {
                // No heading over an empty section, and no blank line where one
                // would have been: a dashboard with no history should look like
                // a dashboard with one section, not like one with a hole.
                continue;
            }
            out.push(Row::Blank);
            out.push(Row::Heading(heading));
            for it in items {
                out.push(Row::Item {
                    key: it.key,
                    label: it.label.clone(),
                    hint: it.hint.clone(),
                    selected: row == self.selected,
                });
                row += 1;
            }
        }

        if !self.footer.is_empty() {
            out.push(Row::Blank);
            for l in self.footer.lines() {
                out.push(Row::Footer(l.to_string()));
            }
        }
        out
    }
}

/// Used only when Lisp sets no banner of its own, which in a normal boot it
/// always does — `init.lisp` builds one that names the running image. Kept
/// deliberately plain: this is what a checkout with a broken config shows, and
/// the thing to say there is what the program is, not artwork.
const DEFAULT_BANNER: &str = "a common lisp machine that edits text";

#[cfg(test)]
mod tests {
    use super::*;

    fn with_recents() -> Dashboard {
        let mut d = Dashboard::default();
        d.recents = vec![item('1', "~/a.lisp", "open:/home/a.lisp", "")];
        d
    }

    fn selected(d: &Dashboard) -> Vec<String> {
        d.rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Item {
                    label,
                    selected: true,
                    ..
                } => Some(label),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn recents_come_first_and_selection_indexes_across_both() {
        let mut d = with_recents();
        assert_eq!(d.len(), 5);
        assert_eq!(d.entries()[0].key, '1');
        assert_eq!(d.entries()[1].key, 'f');

        d.selected = 1;
        assert_eq!(selected(&d), vec!["Find file"]);
    }

    #[test]
    fn clearing_configured_items_keeps_recents() {
        let mut d = with_recents();
        d.items.clear();
        assert_eq!(d.len(), 1);
        assert!(d.rows().iter().any(|r| matches!(r, Row::Item { label, .. } if label == "~/a.lisp")));
    }

    /// An empty section takes no heading and no blank line with it — otherwise
    /// a first boot, which has no history at all, opens on a gap.
    #[test]
    fn a_section_with_nothing_in_it_is_not_drawn() {
        let d = Dashboard::default();
        assert!(d.recents.is_empty());
        let headings: Vec<&str> = d
            .rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Heading(h) => Some(h),
                _ => None,
            })
            .collect();
        assert_eq!(headings, vec![START]);
    }

    /// The banner is prose and the menu is a block, and the renderer tells them
    /// apart by the variant rather than by inspecting the characters — which is
    /// what it used to do, and why a tagline that happened to be box-drawing art
    /// was tinted as art.
    #[test]
    fn the_banner_is_its_own_variant() {
        let mut d = Dashboard::default();
        d.banner = "one\ntwo".into();
        let banners: Vec<String> = d
            .rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Banner(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(banners, vec!["one", "two"]);
    }
}
