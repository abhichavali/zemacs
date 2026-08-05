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

    /// True when the text is a *query over the candidates* and nothing else, so
    /// splitting it on spaces into components that each match on their own, in
    /// any order — orderless — narrows the list rather than changing what was
    /// asked for.
    ///
    /// False wherever the text is a payload something else reads back whole: `:`
    /// is parsed as an ex command, `/` becomes `last_search`, `Grep` is a regex
    /// handed to ripgrep, `Confirm` is compared against the word `yes`, and a
    /// `read-string` answer is prose on its way to a Lisp continuation. A space
    /// in any of those is content, not a separator.
    ///
    /// Stated by what the text *is* rather than by whether a candidate list
    /// happens to be empty, because most of those kinds open with no items and
    /// that is not the reason: `read-string` is one
    /// [`crate::EditorCommand::PromptItem`] away from having a list, and it
    /// would still be answering with a sentence.
    fn orderless(self) -> bool {
        match self {
            PromptKind::Ex | PromptKind::Search | PromptKind::Grep | PromptKind::Confirm => false,
            // `read-string` is prose; `completing-read` is a picker.
            PromptKind::Lisp { completing, .. } => completing,
            // `Line` is in the yes half, which is worth saying out loud: a
            // consult-line query is a filter and only a filter — the jump goes
            // by candidate *index*, and the text is never searched with — so
            // there is nothing for a space to be literal to.
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
        // again drops real hits: the pattern is a regex, and a regex is not a
        // subsequence of anything — nor is it a list of words, so orderless
        // does not rescue it either.
        if self.kind == PromptKind::Grep {
            self.matches = (0..self.items.len()).collect();
            self.selected = 0;
            return;
        }
        let orderless = self.kind.orderless();
        let mut scored: Vec<(u32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| score(&self.text, item, orderless).map(|s| (s, i)))
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
        if score(&self.text, &item, self.kind.orderless()).is_some() {
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

/// Score `haystack` against a whole query — what the filter actually asks for.
///
/// With `orderless`, the query is split on spaces and every component has to
/// match the candidate on its own, in whatever order they were typed. That is
/// what makes `fn main` find `pub fn main` and `buffer switch` find
/// `switch-to-buffer`, neither of which is a subsequence of the query as one
/// string.
///
/// Scores add rather than being taken best-of, so a candidate that matches each
/// component at a word boundary still outranks one that scrapes each of them
/// together — the existing ranking survives, it just runs several times. Every
/// candidate is scored against the same number of components, so the totals
/// stay comparable.
///
/// Components are matched against the whole candidate rather than against what
/// is left after the previous one. Consuming the match would put them back in
/// order, which is the thing being removed. The price is that `fn fn` is
/// satisfied by a line holding one `fn` — orderless pays it too.
// ponytail: no way to escape a space, so a completing prompt cannot look for
// one. Orderless spells it `\ `; add that when a candidate set with spaces in
// it makes the ceiling hurt.
fn score(needle: &str, haystack: &str, orderless: bool) -> Option<u32> {
    // Lowered once per candidate, not once per component — a `consult-line`
    // prompt runs this over every line in the buffer on every keystroke.
    let hay: Vec<char> = haystack.chars().flat_map(char::to_lowercase).collect();
    if !orderless {
        return subsequence(needle, &hay);
    }
    // No components at all — nothing typed, or nothing but spaces — matches
    // everything, which is what an empty needle already meant.
    let mut total = 0;
    for part in needle.split_whitespace() {
        total += subsequence(part, &hay)?;
    }
    Some(total)
}

/// Case-insensitive subsequence match, scored so that better matches sort
/// first: a prefix beats a word-boundary hit, which beats a scattered one.
/// `None` means no match at all. `hay` arrives lowercased — see [`score`].
fn subsequence(needle: &str, hay: &[char]) -> Option<u32> {
    if needle.is_empty() {
        return Some(0);
    }
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
    fn every_space_separated_component_has_to_match() {
        let mut p = prompt(&["find-file", "find-file-other-window", "save-buffer"]);
        p.text = "file window".into();
        p.refilter();
        assert_eq!(p.matches.len(), 1);
        assert_eq!(p.current(), Some("find-file-other-window"));
    }

    #[test]
    fn components_match_in_any_order() {
        // The point of orderless: `buffer switch` is not a subsequence of
        // `switch-to-buffer` as one string, and has to be one as two.
        let mut p = prompt(&["switch-to-buffer", "save-buffer"]);
        p.text = "buffer switch".into();
        p.refilter();
        assert_eq!(p.matches.len(), 1);
        assert_eq!(p.current(), Some("switch-to-buffer"));
    }

    #[test]
    fn one_component_that_misses_rejects_the_candidate() {
        let mut p = prompt(&["switch-to-buffer"]);
        p.text = "switch zzz".into();
        p.refilter();
        assert!(p.matches.is_empty());
        // and Enter still acts on what was typed
        assert_eq!(p.value(), "switch zzz");
    }

    #[test]
    fn a_half_typed_second_word_does_not_empty_the_list() {
        // Every space is a keystroke someone is in the middle of. A list that
        // blanked on the space and came back on the next letter would read as a
        // flicker, and there is no candidate with a literal space to lose.
        let mut p = prompt(&["find-file"]);
        p.text = "find ".into();
        p.refilter();
        assert_eq!(p.matches.len(), 1);
    }

    #[test]
    fn a_consult_line_query_splits_because_it_is_only_a_filter() {
        let mut p = Prompt::new(
            PromptKind::Line,
            "Line: ",
            vec![
                "1  pub fn main() {".to_string(),
                "2  let x = 1;".to_string(),
            ],
        );
        p.text = "main fn".into();
        p.refilter();
        assert_eq!(p.matches.len(), 1);
        // and the item index is still the line number, which is what the jump
        // goes by — splitting the text changed nothing about that
        assert_eq!(p.matches[0], 0);
    }

    #[test]
    fn a_read_string_keeps_its_spaces_but_a_completing_read_splits_them() {
        // `read-string` hands the text back to Lisp verbatim, so a space in it
        // is part of the answer. Items can still arrive — `PromptItem` does not
        // ask whether the prompt completes — and they get the literal.
        let mut read = Prompt::new(
            PromptKind::Lisp {
                id: 1,
                completing: false,
            },
            "Name: ",
            Vec::new(),
        );
        read.text = "main fn".into();
        read.push_item("pub fn main() {".to_string());
        assert!(read.matches.is_empty());

        let mut pick = Prompt::new(
            PromptKind::Lisp {
                id: 2,
                completing: true,
            },
            "Pick: ",
            Vec::new(),
        );
        pick.text = "main fn".into();
        pick.push_item("pub fn main() {".to_string());
        assert_eq!(pick.matches.len(), 1);
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
