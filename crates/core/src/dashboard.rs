//! The startup screen.
//!
//! Content is *data*, not layout: a banner and two lists of rows. The renderer
//! decides where to put them, and Lisp decides what the second list is —
//! `init.lisp` can `clear-dashboard-items` and build its own from scratch.
//!
//! Recent files live in their own list precisely so that clearing works:
//! `clear-dashboard-items` is a config verb about *configured* items, and it
//! would be a nasty surprise if calling it silently cost you your history.

/// One selectable row. `action` is either a built-in verb (`quit`,
/// `find-file`, `scratch`, `config`, `open:<path>`) or the name of a Lisp
/// function, called as `(name)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub key: char,
    pub label: String,
    pub action: String,
}

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
                item('f', "Find file", "find-file"),
                item('s', "Scratch buffer", "scratch"),
                item('c', "Edit configuration", "config"),
                item('q', "Quit", "quit"),
            ],
            selected: 0,
            footer: String::new(),
        }
    }
}

fn item(key: char, label: &str, action: &str) -> Item {
    Item {
        key,
        label: label.into(),
        action: action.into(),
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

    /// Lines to draw, centered by the renderer. Returns `(text, is_selected)`
    /// so the renderer can highlight without knowing the layout rules.
    pub fn lines(&self) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = self
            .banner
            .lines()
            .map(|l| (l.to_string(), false))
            .collect();
        out.push((String::new(), false));

        let mut row = 0;
        for (group, items) in [(0, &self.recents), (1, &self.items)] {
            if items.is_empty() {
                continue;
            }
            if group == 1 && !self.recents.is_empty() {
                out.push((String::new(), false));
            }
            for it in items {
                let selected = row == self.selected;
                let marker = if selected { "▸" } else { " " };
                out.push((format!("{marker} [{}]  {}", it.key, it.label), selected));
                row += 1;
            }
        }

        if !self.footer.is_empty() {
            out.push((String::new(), false));
            for l in self.footer.lines() {
                out.push((l.to_string(), false));
            }
        }
        out
    }
}

const DEFAULT_BANNER: &str = r#"
 ███████╗███████╗███╗   ███╗ █████╗  ██████╗███████╗
 ╚══███╔╝██╔════╝████╗ ████║██╔══██╗██╔════╝██╔════╝
   ███╔╝ █████╗  ██╔████╔██║███████║██║     ███████╗
  ███╔╝  ██╔══╝  ██║╚██╔╝██║██╔══██║██║     ╚════██║
 ███████╗███████╗██║ ╚═╝ ██║██║  ██║╚██████╗███████║
 ╚══════╝╚══════╝╚═╝     ╚═╝╚═╝  ╚═╝ ╚═════╝╚══════╝

        a Common Lisp machine that edits text
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn with_recents() -> Dashboard {
        let mut d = Dashboard::default();
        d.recents = vec![item('1', "~/a.lisp", "open:/home/a.lisp")];
        d
    }

    #[test]
    fn recents_come_first_and_selection_indexes_across_both() {
        let mut d = with_recents();
        assert_eq!(d.len(), 5);
        assert_eq!(d.entries()[0].key, '1');
        assert_eq!(d.entries()[1].key, 'f');

        d.selected = 1;
        let selected: Vec<_> = d.lines().into_iter().filter(|(_, s)| *s).collect();
        assert_eq!(selected.len(), 1);
        assert!(selected[0].0.contains("Find file"));
    }

    #[test]
    fn clearing_configured_items_keeps_recents() {
        let mut d = with_recents();
        d.items.clear();
        assert_eq!(d.len(), 1);
        assert!(d.lines().iter().any(|(l, _)| l.contains("~/a.lisp")));
    }
}
