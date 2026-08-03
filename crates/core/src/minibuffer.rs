//! The minibuffer: one prompt type behind `:`, `/`, `M-x`, find-file and the
//! buffer switcher.
//!
//! Core owns the *model* — what is being asked, what the candidates are, which
//! one is selected — and never the layout. [`CompletionStyle`] says where the
//! renderer should put it, which is how `consult`-style-from-the-bottom and
//! `telescope`-style-in-the-middle are the same code with a different box.
//!
//! Candidates come from wherever the answer lives: commands from the Lisp
//! image, buffers from the editor, files from the app layer (core does no IO).

/// What a prompt is asking for, which decides what Enter does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    /// `:` — an ex command.
    Ex,
    /// `/` — a search pattern.
    Search,
    /// `M-x` — a Lisp function to call.
    Command,
    /// A path to open.
    File,
    /// An open buffer to switch to.
    Buffer,
    /// A line of the current buffer to jump to — `consult-line`.
    Line,
    /// A file (or a project root) from anywhere in the project. Candidates come
    /// from the app, which knows what a project is; accepting one opens it, and
    /// a root opens as a directory, which is dired — so switching project and
    /// finding a file inside one are the same prompt.
    ProjectFile,
    /// A match from anywhere in the project — `consult-ripgrep`. Unlike every
    /// other prompt the candidates come from a subprocess rather than from the
    /// editor, so they are refreshed by the app as the pattern is typed instead
    /// of filtered from a fixed list.
    Grep,
    /// lisp-api: a question asked *by Lisp*, whose answer goes back to a
    /// continuation parked in the image under `id`. This is the only prompt kind
    /// whose destination is not fixed in core, and it is what `read-string` and
    /// `completing-read` are.
    ///
    /// `completing` is carried rather than derived from the candidate list
    /// because the candidates arrive *after* the prompt opens, one
    /// `EditorCommand::PromptItem` each — and the renderer asks
    /// [`PromptKind::completes`] on the frame in between. A `read-string` that
    /// flashed an empty "no matches" box would be a bug nobody could reproduce.
    Lisp { id: u64, completing: bool },
    /// A yes-or-no question guarding something that can lose work, whose "yes"
    /// runs a command parked in [`crate::Editor::pending_confirm`] when the
    /// question was asked.
    ///
    /// The command is parked rather than carried here so this stays `Copy` like
    /// every other kind — and parked in the *editor* rather than in the image,
    /// which is the difference between this and [`PromptKind::Lisp`]: the thing
    /// waiting for the answer is Rust, so there is no continuation to call back.
    ///
    /// Only a full `yes` proceeds. That is Emacs' `yes-or-no-p` rather than
    /// `y-or-n-p`, and the distinction is the whole point: these prompts appear
    /// in front of a discarded rebase and an overwritten file, which are exactly
    /// the two places a reflexive `y` is the failure mode.
    Confirm,
}

impl PromptKind {
    /// Prompts with no candidate list stay a single line whatever the style —
    /// there is nothing to draw in a popup.
    pub fn completes(self) -> bool {
        match self {
            PromptKind::Ex | PromptKind::Search | PromptKind::Confirm => false,
            // Whichever of the two Lisp asked for.
            PromptKind::Lisp { completing, .. } => completing,
            _ => true,
        }
    }
}

/// Where the renderer should draw a completing prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CompletionStyle {
    /// One line at the bottom, no candidate list. Plain vim.
    Minibuffer,
    /// A panel growing upward from the bottom edge, `consult`-style.
    #[default]
    Bottom,
    /// A floating box in the middle of the window, `telescope.nvim`-style.
    Center,
}

impl CompletionStyle {
    pub fn from_name(s: &str) -> Option<CompletionStyle> {
        match s.to_ascii_lowercase().as_str() {
            "minibuffer" | "echo" | "none" => Some(CompletionStyle::Minibuffer),
            "bottom" | "consult" => Some(CompletionStyle::Bottom),
            "center" | "centre" | "telescope" | "popup" => Some(CompletionStyle::Center),
            _ => None,
        }
    }
}

/// An active prompt.
pub struct Prompt {
    pub kind: PromptKind,
    /// Drawn before the input: `":"`, `"/"`, `"M-x "`, `"Find file: "`.
    pub label: String,
    pub text: String,
    /// Every candidate, unfiltered.
    pub items: Vec<String>,
    /// Indices into `items` that match `text`, best first.
    pub matches: Vec<usize>,
    /// Index into `matches`.
    pub selected: usize,
    /// Where the cursor was when the prompt opened, so cancelling can put it
    /// back. Only meaningful for prompts that move the cursor while you type.
    pub origin: Option<usize>,
    /// One buffer id per candidate, for a `Buffer` prompt and empty otherwise.
    ///
    /// An **id** and not the index the candidate sits at, because previewing a
    /// buffer switches to it and switching *reorders* the list — the outgoing
    /// buffer goes to the front, which is what makes the switcher
    /// most-recently-used. Indices taken when the prompt opened would therefore
    /// mean something different after the first arrow key, and the candidate
    /// under the cursor would walk away from you as you scrolled.
    pub ids: Vec<crate::BufferId>,
    /// The buffer that was live when the prompt opened, so cancelling comes
    /// back to it. The counterpart of [`Prompt::origin`] for a prompt whose
    /// preview moves between buffers rather than within one.
    pub origin_buffer: Option<crate::BufferId>,
    /// Characters at the front of every candidate that are *decoration* rather
    /// than content — the right-aligned line number `consult-line` puts in
    /// front of each line. The renderer needs it to line a candidate's text up
    /// with the buffer's own highlight spans, and it is one number rather than
    /// a re-parse per row because the format is decided here.
    pub prefix: usize,
}

impl PromptKind {
    /// True when moving the selection should move the *cursor* too, so the
    /// buffer follows the highlighted candidate as you narrow — consult's
    /// preview. Cancelling then has to restore the original position.
    ///
    /// `/` is here by a different route to the same place: it has no candidate
    /// list, so what follows the input is the *match*, recomputed from scratch
    /// on every keystroke. Both kinds need an `origin` recorded when the prompt
    /// opens, and both need Escape to go back to it — which is the whole of
    /// incremental search, and the reason it is a flag on the prompt rather
    /// than a mode of its own.
    pub fn previews(self) -> bool {
        // `Buffer` is here for the same reason `Line` is, one scale up: what you
        // are choosing between is *documents*, and their names are a poor
        // reminder of which is which. Showing the highlighted one is the whole
        // of consult-buffer's usefulness. It restores through
        // [`Prompt::origin_buffer`] rather than [`Prompt::origin`] — the thing
        // to put back is a buffer, not an offset.
        matches!(
            self,
            PromptKind::Line | PromptKind::Search | PromptKind::Buffer
        )
    }
}

impl Prompt {
    pub fn new(kind: PromptKind, label: &str, items: Vec<String>) -> Self {
        let mut p = Self {
            kind,
            label: label.to_string(),
            text: String::new(),
            items,
            matches: Vec::new(),
            selected: 0,
            origin: None,
            ids: Vec::new(),
            origin_buffer: None,
            prefix: 0,
        };
        p.refilter();
        p
    }

    /// Recompute `matches` for the current `text`.
    ///
    /// The selection resets to the best match. Holding it on whatever was
    /// previously highlighted would mean typing more of a name walks *away*
    /// from it — the top hit would be found and then skipped.
    pub fn refilter(&mut self) {
        // A grep prompt's candidates were matched by ripgrep, against the same
        // pattern and with a far better matcher than this one. Filtering them
        // again drops real hits: `fn main` is not a subsequence of a line
        // holding `fn  main`, and a regex is not a subsequence of anything.
        if self.kind == PromptKind::Grep {
            self.matches = (0..self.items.len()).collect();
            self.selected = 0;
            return;
        }
        let mut scored: Vec<(u32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| score(&self.text, item).map(|s| (s, i)))
            .collect();
        // Best score first, then original order so equal matches stay stable.
        scored.sort_by_key(|&(s, i)| (std::cmp::Reverse(s), i));
        self.matches = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
    }

    /// The highlighted candidate, if the filter matched anything.
    pub fn current(&self) -> Option<&str> {
        self.matches
            .get(self.selected)
            .map(|&i| self.items[i].as_str())
    }

    /// What Enter should act on: the highlighted candidate, or the raw text
    /// when nothing matched (so you can still open a file that doesn't exist).
    pub fn value(&self) -> String {
        self.current().unwrap_or(&self.text).to_string()
    }

    pub fn next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + self.matches.len() - 1) % self.matches.len();
        }
    }

    /// Replace the candidate set — used when the app recomputes file
    /// completions as the path is typed.
    pub fn set_items(&mut self, items: Vec<String>) {
        self.items = items;
        self.refilter();
    }

    /// lisp-api: append one candidate to a live prompt, which is how a Lisp
    /// `completing-read` delivers its list — one `%do` per candidate, because
    /// the write envelope carries a single string.
    ///
    /// Appends to `matches` rather than re-ranking, so building a list of N is N
    /// cheap calls instead of N sorts. The order is therefore the order Lisp
    /// gave, which is what a hand-written list means; the first keystroke
    /// re-ranks properly.
    pub fn push_item(&mut self, item: String) {
        if score(&self.text, &item).is_some() {
            self.matches.push(self.items.len());
        }
        self.items.push(item);
    }

    /// Tab: adopt the highlighted candidate as the input.
    ///
    /// For a file prompt this is how you walk a tree — completing to
    /// `~/src/foo/` makes the app list *that* directory next, so repeated Tab
    /// descends. Completing to something already typed cycles instead, so Tab
    /// never becomes a dead key.
    pub fn complete(&mut self) {
        match self.current() {
            Some(c) if c != self.text => {
                self.text = c.to_string();
                self.refilter();
            }
            _ => self.next(),
        }
    }

    /// The rows to draw and which is selected, capped to what fits. Scrolls
    /// with the selection so it is always on screen.
    ///
    /// Each row carries its index into `items` as well as its text. For a
    /// `Line` prompt that index *is* the buffer line, which is what lets the
    /// renderer colour a candidate out of the highlight spans the buffer
    /// already has rather than re-parsing the line.
    pub fn visible(&self, max: usize) -> Vec<(usize, &str, bool)> {
        if max == 0 || self.matches.is_empty() {
            return Vec::new();
        }
        let first = self.selected.saturating_sub(max - 1).min(
            self.matches.len().saturating_sub(max),
        );
        self.matches
            .iter()
            .enumerate()
            .skip(first)
            .take(max)
            .map(|(row, &i)| (i, self.items[i].as_str(), row == self.selected))
            .collect()
    }

    /// Text as typed, with the label — what a non-completing prompt shows.
    pub fn line(&self) -> String {
        format!("{}{}", self.label, self.text)
    }
}

/// Case-insensitive subsequence match, scored so that better matches sort
/// first: a prefix beats a word-boundary hit, which beats a scattered one.
/// `None` means no match at all.
fn score(needle: &str, haystack: &str) -> Option<u32> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.chars().flat_map(char::to_lowercase).collect();
    let mut score = 0;
    let mut at = 0;
    for want in needle.chars().flat_map(char::to_lowercase) {
        let found = hay[at..].iter().position(|&c| c == want)? + at;
        if found == 0 {
            score += 8; // matches the very start
        } else if !hay[found - 1].is_alphanumeric() {
            score += 4; // start of a word: `s-b` finds `switch-buffer`
        } else if found == at {
            score += 2; // contiguous with the previous match
        }
        at = found + 1;
    }
    // Prefer shorter candidates when scores tie: `find-file` over `find-file-at-point`.
    Some(score * 100 + (100u32.saturating_sub(hay.len() as u32)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(items: &[&str]) -> Prompt {
        Prompt::new(
            PromptKind::Command,
            "M-x ",
            items.iter().map(|s| s.to_string()).collect(),
        )
    }

    #[test]
    fn empty_input_matches_everything() {
        let p = prompt(&["a", "b", "c"]);
        assert_eq!(p.matches.len(), 3);
        assert_eq!(p.current(), Some("a"));
    }

    #[test]
    fn subsequence_matching_finds_scattered_letters() {
        let mut p = prompt(&["text-scale-increase", "find-file", "quit"]);
        p.text = "tsi".into();
        p.refilter();
        assert_eq!(p.matches.len(), 1);
        assert_eq!(p.current(), Some("text-scale-increase"));
    }

    #[test]
    fn word_boundaries_outrank_scattered_hits() {
        let mut p = prompt(&["superbly-fine", "switch-buffer"]);
        p.text = "sb".into();
        p.refilter();
        assert_eq!(p.current(), Some("switch-buffer"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let mut p = prompt(&["Find-File"]);
        p.text = "ff".into();
        p.refilter();
        assert_eq!(p.matches.len(), 1);
    }

    #[test]
    fn no_match_falls_back_to_the_typed_text() {
        let mut p = prompt(&["quit"]);
        p.text = "/etc/hosts".into();
        p.refilter();
        assert!(p.current().is_none());
        assert_eq!(p.value(), "/etc/hosts");
    }

    #[test]
    fn selection_wraps_and_typing_returns_to_the_best_match() {
        let mut p = prompt(&["alpha", "beta"]);
        p.next();
        assert_eq!(p.current(), Some("beta"));
        p.next();
        assert_eq!(p.current(), Some("alpha"));
        p.prev();
        assert_eq!(p.current(), Some("beta"));

        // typing re-ranks and selects the best match, not the old highlight
        p.text = "alp".into();
        p.refilter();
        assert_eq!(p.current(), Some("alpha"));
    }

    #[test]
    fn visible_window_follows_the_selection() {
        let items: Vec<String> = (0..20).map(|i| format!("item{i}")).collect();
        let mut p = Prompt::new(PromptKind::Command, "M-x ", items);
        p.selected = 15;
        let rows = p.visible(5);
        assert_eq!(rows.len(), 5);
        // The item index comes back too, and it is the index into `items` —
        // which is what lets a renderer look the candidate's source up.
        assert!(rows.iter().any(|&(i, t, sel)| sel && t == "item15" && i == 15));
        // and it never runs off the end
        p.selected = 19;
        assert_eq!(p.visible(5).len(), 5);
        assert_eq!(p.visible(0).len(), 0);
    }

    #[test]
    fn style_names_cover_the_words_people_use() {
        assert_eq!(
            CompletionStyle::from_name("telescope"),
            Some(CompletionStyle::Center)
        );
        assert_eq!(
            CompletionStyle::from_name("consult"),
            Some(CompletionStyle::Bottom)
        );
        assert_eq!(CompletionStyle::from_name("nonsense"), None);
    }
}
