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
    ///
    /// `previewing` opts the picker into consult's live preview, the thing the
    /// buffer switcher has always had: every time the highlight moves, the
    /// candidate under it goes back to the image as `%prompt-preview`. What that
    /// *means* is Lisp's — loading a theme, opening a font — which is the whole
    /// reason this is a flag here and a closure there. Core owns "the selection
    /// moved" and knows nothing else about it.
    Lisp {
        id: u64,
        completing: bool,
        previewing: bool,
    },
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
    /// How far back `M-p` has walked, counting from the newest entry of this
    /// kind's history. `0` means "not walking" — the text on screen is yours.
    pub history: usize,
    /// What was typed before the first `M-p`, so `M-n` all the way forward
    /// gives it back. Without it, glancing at the last command costs you the
    /// filter you had already narrowed to.
    pub stash: String,
    /// Answers already given to prompts of this kind, newest first — what
    /// [`Editor::prompt_history`](crate::Editor::prompt_history) holds, handed
    /// over when the prompt opens.
    ///
    /// It is a *ranking* input and not only what `M-p` walks: a candidate that
    /// has been chosen before gets [`RECENT`] added to its score, decayed by how
    /// far back it was, so `M-x` opens on the handful of commands you actually
    /// run and keeps them near the top as you narrow. That is the whole of what
    /// makes a command list of four hundred usable — the answer is nearly always
    /// something you have run this week, and nothing else in the score knows
    /// that.
    ///
    /// Matched against [`Prompt::name_of`], not the whole row, because that is
    /// what [`Prompt::submitted`] filed: an `M-x` row carries a docstring the
    /// image padded on, and it is not part of the answer.
    pub recent: Vec<String>,
    /// This prompt wants free text, whatever its kind would normally complete.
    ///
    /// dired's `+` and `C-c n` are a [`PromptKind::File`] asking for a *name*:
    /// the file lands in the directory on screen, so a path is not merely
    /// unnecessary but refused. Candidates there are actively wrong —
    /// [`Prompt::value`] answers with the highlighted one, so `+` typed `notes`
    /// and submitted whatever the filesystem listing had put under the cursor.
    ///
    /// A field rather than a kind of its own, because the *destination* is
    /// unchanged — this is still the prompt whose answer opens a file — and a
    /// new `PromptKind` is a match arm in three crates. See
    /// [`PromptKind::Lisp`]'s `completing`, which is the same distinction
    /// carried for the same reason.
    pub bare: bool,
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
        //
        // A Lisp picker joins them when it asked to. It restores nothing
        // through `origin` — what a theme or a font preview disturbed is not an
        // offset and not a buffer, so putting it back is the *callback's* job,
        // on the NIL it gets when the prompt is cancelled.
        matches!(
            self,
            PromptKind::Line
                | PromptKind::Search
                | PromptKind::Buffer
                | PromptKind::Lisp {
                    previewing: true,
                    ..
                }
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
            history: 0,
            stash: String::new(),
            recent: Vec::new(),
            bare: false,
        };
        p.refilter();
        p
    }

    /// True when this prompt should show a candidate list. [`Prompt::bare`]
    /// overrides the kind; everything else is the kind's own answer.
    pub fn completes(&self) -> bool {
        !self.bare && self.kind.completes()
    }

    /// The part of a candidate that *names* it, as opposed to the annotation
    /// drawn after it.
    ///
    /// An `M-x` row is `find-file    Open a file   C-x C-f`: the name is the
    /// first word and the rest is chrome the image padded on. The switcher's
    /// rows are a name, a run of padding, and the major mode.
    ///
    /// Matching cares because a hit in the name is worth far more than a hit in
    /// a docstring — see [`Prompt::refilter`] — and because this is what goes in
    /// the history, so it is also what recency is looked up by.
    pub fn name_of<'a>(&self, item: &'a str) -> &'a str {
        match self.kind {
            PromptKind::Command => item.split_whitespace().next().unwrap_or(""),
            // The padding `buffer_candidates` inserts is at least two spaces,
            // and a buffer name does not contain a run of them.
            PromptKind::Buffer => item.split("  ").next().unwrap_or(item),
            _ => item,
        }
    }

    /// Recompute `matches` for the current `text`.
    ///
    /// Ranking is three questions, in order:
    ///
    /// 1. **Did the *name* match, or only the annotation?** Every candidate
    ///    matched on its name outranks every candidate matched only on its
    ///    docstring. `M-x file` is asking about commands called file-something;
    ///    that some other command's help text mentions files is worth showing
    ///    and never worth showing first.
    /// 2. **How well did it match** — [`crate::fuzzy`]'s alignment score, plus
    ///    a decaying bonus for having been chosen before ([`Prompt::recent`]).
    /// 3. **How much of the candidate is left over.** `find-file` beats
    ///    `find-file-at-point` on an equal score, because the query covered more
    ///    of it. Not for [`PromptKind::Line`], where the index is the line
    ///    number and document order is the only sensible tie-break — sorting
    ///    the buffer's lines by length is nobody's idea of a search result.
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
        let mut ms = self.matchers();
        // Nothing typed means nothing to say: the list keeps the order it
        // arrived in — Lisp's own, or the switcher's most-recently-used — and
        // preferring the shorter of two unranked candidates would shuffle it
        // for no reason. `Line` never takes the length tie-break at all.
        let by_length = !self.text.trim().is_empty() && self.kind != PromptKind::Line;
        type Key = (std::cmp::Reverse<bool>, std::cmp::Reverse<i32>, usize, usize);
        let mut scored: Vec<Key> = Vec::with_capacity(self.items.len());
        for (i, item) in self.items.iter().enumerate() {
            let name = self.name_of(item);
            let Some((named, mut total)) = rate(&mut ms, name, item) else {
                continue;
            };
            // Decayed by how far back it was, but never below half: *having*
            // run something is the signal, and how recently only orders the
            // handful at the top. A bonus that faded to nothing would make the
            // history useless by the twentieth entry, which is a Tuesday.
            if let Some(r) = self.recent.iter().position(|e| e == name) {
                total += (RECENT - r as i32).max(RECENT / 2);
            }
            let len = if by_length { item.len() } else { 0 };
            scored.push((
                std::cmp::Reverse(named),
                std::cmp::Reverse(total),
                len,
                i,
            ));
        }
        // Unstable, and the trailing index is why it can be: every key ends in
        // the candidate's own position, so no two of them compare equal and the
        // order is total. That buys the sort its scratch-free path, which is
        // the difference between one allocation and none on a list the size of
        // a buffer's lines.
        scored.sort_unstable();
        self.matches = scored.into_iter().map(|(.., i)| i).collect();
        self.selected = 0;
    }

    /// One matcher per query component. Orderless splits on spaces so every
    /// component narrows on its own in whatever order they were typed; the
    /// other kinds are one query, spaces and all.
    fn matchers(&self) -> Vec<crate::fuzzy::Matcher> {
        match self.kind.orderless() {
            true => self
                .text
                .split_whitespace()
                .map(crate::fuzzy::Matcher::new)
                .collect(),
            false => vec![crate::fuzzy::Matcher::new(&self.text)],
        }
    }

    /// Where the current query matches inside one candidate row, as half-open
    /// character ranges — what a renderer underlines to show *why* a row is on
    /// screen.
    ///
    /// Recomputed per call, for the rows being drawn. Keeping positions for
    /// every candidate would mean a vector per line of the buffer on every
    /// keystroke of a `consult-line`, to show forty of them.
    pub fn match_spans(&self, item: &str) -> Vec<(usize, usize)> {
        if self.kind == PromptKind::Grep {
            return Vec::new();
        }
        let mut ms = self.matchers();
        let name = self.name_of(item);
        // Whichever half `refilter` scored it on, so the paint agrees with the
        // ranking rather than lighting up a docstring the score ignored.
        let hay = match ms.iter_mut().all(|m| m.score(name).is_some()) {
            true => name,
            false => item,
        };
        let mut spans: Vec<(usize, usize)> =
            ms.iter_mut().flat_map(|m| m.spans(hay)).collect();
        spans.sort_unstable();
        spans.dedup();
        spans
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

    /// What accepting this prompt *means*, as opposed to what it shows.
    ///
    /// The two differ wherever a row carries an annotation the image or the
    /// switcher padded onto it — a docstring and a key after an `M-x` command,
    /// the major mode after a buffer name. This is also what goes in the
    /// history, which is why it is one function and not a split at each call
    /// site: recalling `M-x` has to give back the command, not the row it was
    /// read off.
    ///
    /// `Ex` and `Search` answer with the text as typed. Both have no candidate
    /// list, and `value` would fall back to `text` anyway — saying so here means
    /// the reader does not have to go and check that.
    pub fn submitted(&self) -> String {
        match self.kind {
            PromptKind::Ex | PromptKind::Search => self.text.clone(),
            // [`Prompt::name_of`] is the one place that knows which part of a
            // row is the answer and which part is chrome, so recency looks the
            // history up by exactly what got filed in it.
            _ => {
                let v = self.value();
                self.name_of(&v).to_string()
            }
        }
    }

    /// Put `entry` in the input as if it had been typed, for `M-p`/`M-n`.
    pub fn recall(&mut self, entry: String) {
        self.text = entry;
        self.refilter();
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
        let mut ms = self.matchers();
        if rate(&mut ms, self.name_of(&item), &item).is_some() {
            self.matches.push(self.items.len());
        }
        self.items.push(item);
    }

    /// The same for a whole list, building the matchers once instead of once per
    /// candidate — which is what [`Prompt::push_item`] does, and is why a list of
    /// fifty thousand wants this door and not that one.
    ///
    /// Still an append and still in the order given, for [`Prompt::push_item`]'s
    /// reason: nothing has been typed yet, so there is nothing to rank by, and
    /// the caller's order is the meaningful one until there is.
    pub fn extend_items(&mut self, items: impl Iterator<Item = String>) {
        let mut ms = self.matchers();
        for item in items {
            if rate(&mut ms, self.name_of(&item), &item).is_some() {
                self.matches.push(self.items.len());
            }
            self.items.push(item);
        }
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

/// What having been chosen before is worth, before the decay by how long ago.
///
/// Tuned against the alignment scores it is added to: a matched character is
/// worth 16 and a word boundary 8, so this is about two extra characters of
/// evidence. Enough that among the commands that match `buf` the one you ran
/// this morning comes up first, and not enough that a command you once ran
/// beats a candidate the query actually spells out.
const RECENT: i32 = 36;

/// Score one candidate against every query component: `(matched the name, how
/// well)`, or `None` when any component missed both halves.
///
/// Components are matched against the whole of the candidate rather than
/// against what is left after the previous one. Consuming the match would put
/// them back in order, which is the thing orderless removes. The price is that
/// `fn fn` is satisfied by a line holding one `fn` — orderless pays it too.
///
/// Scores add rather than being taken best-of, so a candidate that matches each
/// component at a word boundary still outranks one that scrapes each of them
/// together. Every candidate is scored against the same number of components,
/// so the totals stay comparable.
///
/// `name` is the part of `item` that names it and is usually all of it — see
/// [`Prompt::name_of`]. Falling back to the whole row is what lets `M-x
/// clipboard` find a command whose *docstring* says clipboard, one tier below
/// everything whose name matched.
// ponytail: no way to escape a space, so a completing prompt cannot look for
// one. Orderless spells it `\ `; add that when a candidate set with spaces in
// it makes the ceiling hurt.
fn rate(ms: &mut [crate::fuzzy::Matcher], name: &str, item: &str) -> Option<(bool, i32)> {
    let mut total = 0;
    let mut named = true;
    for m in ms.iter_mut() {
        match m.score(name) {
            Some(s) => total += s,
            // Only worth a second look when there is more of the row to look
            // at; for most kinds the name *is* the row.
            None if name.len() < item.len() => {
                total += m.score(item)?;
                named = false;
            }
            None => return None,
        }
    }
    Some((named, total))
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
                previewing: false,
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
                previewing: false,
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

    /// An `M-x` row is a name and then prose. The prose is searchable — you do
    /// not always remember what a command is called — but it never comes first.
    #[test]
    fn a_name_that_matched_outranks_a_docstring_that_did() {
        let mut p = prompt(&[
            "kill-ring-save         Copy the region to the clipboard",
            "clipboard-yank         Paste",
        ]);
        p.text = "clip".into();
        p.refilter();
        assert_eq!(p.matches.len(), 2, "the docstring hit is still offered");
        assert_eq!(p.current(), Some("clipboard-yank         Paste"));
    }

    /// The point of a history: four hundred commands, and the answer is nearly
    /// always one of the six you use.
    #[test]
    fn what_you_ran_last_comes_back_first() {
        let mut p = prompt(&["buffer-menu", "kill-buffer", "buffer-list"]);
        // With nothing typed the list is as it arrived...
        assert_eq!(p.current(), Some("buffer-menu"));
        // ...and with a history it opens on the newest entry instead, even
        // though that one buries its match in the middle of a word.
        p.recent = vec!["kill-buffer".into()];
        p.refilter();
        assert_eq!(p.current(), Some("kill-buffer"));

        // Narrowing keeps it, because recency is part of the score rather than
        // a pre-sort that the first keystroke throws away.
        p.text = "buf".into();
        p.refilter();
        assert_eq!(p.current(), Some("kill-buffer"));

        // But it does not beat a candidate the query actually spells out.
        p.text = "buffer-m".into();
        p.refilter();
        assert_eq!(p.current(), Some("buffer-menu"));
    }

    /// Recency is looked up by the *answer*, which for `M-x` is the first word
    /// — the history never held the docstring, so nothing would ever match.
    #[test]
    fn recency_matches_the_answer_and_not_the_row() {
        let mut p = prompt(&["find-file    Open a file", "save-buffer  Write it"]);
        p.recent = vec!["save-buffer".into()];
        p.refilter();
        assert_eq!(p.current(), Some("save-buffer  Write it"));
        assert_eq!(p.submitted(), "save-buffer");
    }

    /// What the renderer paints. The spans have to be indices into the row as
    /// drawn, and they have to point at the match the ranking actually used.
    #[test]
    fn the_matched_characters_come_back_for_painting() {
        let mut p = prompt(&["switch-to-buffer"]);
        p.text = "buf".into();
        p.refilter();
        assert_eq!(p.match_spans("switch-to-buffer"), vec![(10, 13)]);

        // Orderless: one span per component, in row order rather than typed
        // order, because they are positions and not keystrokes.
        p.text = "buffer switch".into();
        p.refilter();
        assert_eq!(p.match_spans("switch-to-buffer"), vec![(0, 6), (10, 16)]);
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
