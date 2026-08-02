//! Ranges that survive editing, carrying something to draw.
//!
//! A [`Span`](crate::Span) is a fact about one revision of the text: the syntax
//! thread hands back a fresh set every time the buffer changes, and anything
//! that wanted to keep one would have to recompute it. An *overlay* is the other
//! kind of annotation — put there deliberately, by a command rather than by a
//! parser, and expected to still be over the same words after you type in front
//! of it. That is a marker pair, so this module is [`marker::adjust_pos`] twice
//! plus a payload, and nothing else.
//!
//! # There are no text properties
//!
//! Emacs has both overlays and text properties, and the difference is history:
//! properties live *in* the buffer's string data, which is why they survive a
//! copy into the kill ring and why `insert` inherits them. Neither of those is
//! worth a second mechanism here — a rope of `char`s is what makes every offset
//! in the API a character index, and threading a property list through
//! `Rope::insert` would give that up to buy an inheritance rule most configs
//! turn off. One mechanism, and it is this one.
//!
//! # What an overlay carries
//!
//! Only what the *renderer* has to know: a face, a background, a substituted
//! string, an image, and — since typesetting arrived — how big and how heavy the
//! type is. Everything else a config wants to hang off an overlay — an avy
//! hint's target, a diagnostic's message, a closure — stays in the Lisp image,
//! in a hash table keyed by the same handle. That is the boundary rule working
//! as intended: Rust owns the storage the frame reads, Lisp owns the property
//! list, and an arbitrary Lisp value never has to survive a round trip through C
//! as printed source.
//!
//! # Cells and lines
//!
//! The payloads split in two, and the split is the one thing to understand here.
//! Most of them are about the *cells* a range covers: a face recolours them, a
//! `display` string replaces them, `bold` and `italic` change which face
//! rasterises them. Three are about the *lines* the range touches, whole:
//! [`Overlay::fold`] makes them stop occupying rows, [`Overlay::line_background`]
//! paints a band the width of the pane behind them, and
//! [`Overlay::line_prefix`] pushes their text in and draws something in the gap.
//! [`Overlay::scale`] is written as a cell payload and behaves as a line one —
//! see its own note, which is the only surprising thing in this file.
//!
//! Nothing here decides *what* gets which. A level-1 heading being 1.5× and bold
//! is policy, and policy is Lisp's; this is the vocabulary it says it in.
//!
//! ponytail: colours are named from `face-list` rather than given as RGB, so an
//! overlay follows a theme change and no new colour vocabulary exists. An
//! overlay wanting a colour no face has must `set-syntax-color` a spare face
//! first; the upgrade path is a primitive taking three floats, the shape
//! `set-background` already has.

use crate::marker::{adjust_pos, Insertion};
use crate::HlKind;

/// An overlay as Lisp holds it — a plain integer, like a marker handle, and
/// unique across every buffer so one can never be mistaken for another's.
pub type OverlayId = u64;

/// A rendered bitmap, keyed by what produced it. See [`Image`].
pub type ImageId = u64;

/// A bitmap an overlay can put in the text.
///
/// Pixels rather than a path: the only producer is `zemacs-latex`, which hands
/// back RGBA it has already decoded, and writing it out only for the renderer to
/// read it back would be two syscalls to avoid holding a few hundred KB that the
/// renderer is about to upload anyway. The disk cache that *does* exist lives one
/// layer down, in `zemacs-latex`, where it belongs.
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Pixels the image descends *below* the text baseline, so an inline
    /// fragment sits on the line rather than floating above it.
    pub depth: u32,
    /// `width * height * 4`, `R G B A`, straight (not premultiplied) alpha.
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Image {
    /// Without this, one `{:?}` of a failed assertion prints a megabyte of
    /// pixel values.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("depth", &self.depth)
            .finish_non_exhaustive()
    }
}

/// One overlay: a char range plus what to do with it.
#[derive(Clone, Debug, PartialEq)]
pub struct Overlay {
    pub id: OverlayId,
    /// Char offsets, `start < end` always — an overlay whose text is deleted is
    /// deleted with it rather than lingering as a zero-width one.
    pub start: usize,
    pub end: usize,
    /// Foreground, named from `face-list`. `None` leaves the syntax colour.
    pub face: Option<HlKind>,
    /// Background, from the same table. `None` leaves the pane's.
    pub background: Option<HlKind>,
    /// Drawn *instead of* the covered text — org-modern bullets, org-appear,
    /// ghost text. The cells are replaced, so wrapping and the cursor follow.
    pub display: Option<String>,
    /// Drawn instead of the covered text, as a bitmap.
    pub image: Option<ImageId>,
    /// Type size for the covered run, as a **percentage of the body size**:
    /// `Some(150)` is one and a half times the text around it, `None` and
    /// `Some(100)` are the body size itself.
    ///
    /// A percentage rather than a float because the whole overlay bridge is
    /// integers and strings — see `command_for` in `zemacs-lisp` — and because
    /// the renderer is going to snap it to one of a handful of steps anyway, for
    /// which an exact float was never the input.
    ///
    /// **This is written per run and behaves per line.** A scaled cell is wider
    /// as well as taller, and every piece of layout downstream — where a line
    /// wraps, which column the cursor is in, which character a click lands on —
    /// counts in whole cells of one width. Letting a run carry its own advance
    /// would mean a per-line table of pixel offsets and rewriting all of that in
    /// pixels; instead the *widest scale claimed by any overlay touching a line*
    /// sets that line's cell, and the grid stays a grid. That is what a heading
    /// wants anyway, and it is the only reading under which the line the cursor
    /// is on and the line the renderer drew agree about where column 12 is.
    ///
    /// ponytail: so a scaled run in the middle of a body line enlarges the whole
    /// line rather than a word. Ceiling: `*this* word bigger`, which nothing has
    /// asked for. Upgrade path is the per-line advance table `draw_image`
    /// already names for its own horizontal reflow — one table, both problems.
    pub scale: Option<u16>,
    /// Real weight, not a colour. `None` leaves whatever an earlier overlay or
    /// the syntax highlight decided; `Some(false)` says upright *deliberately*,
    /// which is how a later overlay takes an earlier one's bold off.
    ///
    /// The reason [`HlKind`]'s markup faces carry emphasis as colour is that the
    /// renderer used to open exactly one font face. It opens more than one now,
    /// and this is the attribute that lifted it.
    pub bold: Option<bool>,
    /// Real slant. Same tri-state as [`Overlay::bold`], same reason.
    pub italic: Option<bool>,
    /// A band across the **whole pane**, behind every line this overlay touches,
    /// named from `face-list` like every other colour here.
    ///
    /// Not the same as [`Overlay::background`], and the difference is the point:
    /// a background paints the cells the range covers, so a source block drawn
    /// with one is a stripe as ragged as its text. This paints the line, edge to
    /// edge, so the block reads as a block. Emacs needs a stretch display
    /// property and a `:extend`ed face to say the same thing.
    pub line_background: Option<HlKind>,
    /// Drawn in the left margin of every row of every line this overlay touches,
    /// with the line's own text pushed right by exactly its width.
    ///
    /// Emacs' `line-prefix` and `wrap-prefix` collapsed into one, because the
    /// case that wants either — a quote bar, an indented source block — wants
    /// the continuation rows to line up too, and two properties that are always
    /// set together are one property.
    pub line_prefix: Option<String>,
    /// **Hide the lines after this overlay's first one.** The one payload here
    /// that is about *lines* rather than about cells, and the whole of code
    /// folding: everything else an overlay carries replaces some characters with
    /// something else the same shape, and a fold makes rows stop existing.
    ///
    /// The line `start` falls on stays drawn — that is the headline, the `fn`
    /// signature, the `{` — and carries an indicator; every line after it up to
    /// `end` is skipped by the renderer *and* stepped over by `j`. See
    /// [`fold_hiding`], which is the one predicate both sides ask.
    pub fold: bool,
}

impl Overlay {
    fn new(id: OverlayId, start: usize, end: usize) -> Self {
        Self {
            id,
            start,
            end,
            face: None,
            background: None,
            display: None,
            image: None,
            scale: None,
            bold: None,
            italic: None,
            line_background: None,
            line_prefix: None,
            fold: false,
        }
    }
}

/// The fold hiding the line that starts at char offset `line_start`, as that
/// fold's own start offset — `None` when the line is drawn.
///
/// The single question folding asks, and both the renderer and `j` ask it, for
/// the same reason `expand_line` is shared: a line the frame does not draw and
/// a line the cursor may land on must be the same set, or point disappears.
///
/// Strictly *after* the fold's first line: `o.start < line_start` is false for
/// the line the fold begins on, which is what keeps a headline visible with its
/// body hidden underneath. And `< o.end` because the range is half-open, so the
/// line beginning exactly where a fold ends is outside it.
pub fn fold_hiding(overlays: &[Overlay], line_start: usize) -> Option<usize> {
    overlays
        .iter()
        .find(|o| o.fold && o.start < line_start && line_start < o.end)
        .map(|o| o.start)
}

/// True when a fold *begins* inside `[start, end]` — the line that stays drawn
/// and gets the indicator. Inclusive at the end, so a fold anchored at the
/// newline still belongs to the line before it rather than to no line at all.
pub fn fold_starts_in(overlays: &[Overlay], start: usize, end: usize) -> bool {
    overlays
        .iter()
        .any(|o| o.fold && (start..=end).contains(&o.start))
}

/// A change to an overlay, as it arrives from Lisp.
///
/// One [`EditorCommand`](crate::EditorCommand) variant carries all of these
/// rather than a dozen flat ones: they share a handle and a destination, and
/// none of them touches the document — an overlay is drawing, not text, which is
/// why `mutates_document` says no and a face can therefore be put on a
/// *generated* buffer like dired or magit.
///
/// One variant per attribute rather than one carrying a struct of options, for
/// the reason `overlay-put` has the shape it does: a config sets one thing at a
/// time and must not have to restate the other ten to do it. `None` clears.
#[derive(Clone, Debug, PartialEq)]
pub enum OverlayEdit {
    /// `None` clears the face.
    Face(OverlayId, Option<HlKind>),
    Background(OverlayId, Option<HlKind>),
    Display(OverlayId, Option<String>),
    Image(OverlayId, Option<ImageId>),
    /// Percent of the body size. `None`, and `Some(100)`, are body size.
    Scale(OverlayId, Option<u16>),
    /// `Some(false)` is upright *on purpose*, and outranks an earlier overlay's
    /// bold; `None` is no opinion. Same for [`OverlayEdit::Italic`].
    Bold(OverlayId, Option<bool>),
    Italic(OverlayId, Option<bool>),
    LineBackground(OverlayId, Option<HlKind>),
    LinePrefix(OverlayId, Option<String>),
    /// Fold, or unfold, the lines after this overlay's first one.
    Fold(OverlayId, bool),
    Delete(OverlayId),
    /// Every overlay overlapping `[start, end)`, as Emacs' `remove-overlays`.
    RemoveIn(usize, usize),
}

/// The overlays of one buffer, in char offsets, in the order they were made.
///
/// ponytail: a `Vec` scanned linearly, for the same reason [`crate::marker`] is
/// one — a handful per buffer, placed by a command. That stops being true the
/// day something puts one on every hit of a search (avy will), and the answer
/// then is the same: an interval tree, or at least keeping the list sorted by
/// `start` so the renderer can binary-search a line.
#[derive(Default)]
pub struct Overlays {
    live: Vec<Overlay>,
}

impl Overlays {
    pub fn add(&mut self, id: OverlayId, start: usize, end: usize) {
        self.live.push(Overlay::new(id, start, end));
    }

    /// In creation order, which is the order the renderer resolves them in:
    /// **the most recently made overlay wins**, per attribute.
    pub fn all(&self) -> &[Overlay] {
        &self.live
    }

    pub fn span(&self, id: OverlayId) -> Option<(usize, usize)> {
        self.live.iter().find(|o| o.id == id).map(|o| (o.start, o.end))
    }

    /// Everything overlapping `[start, end)`, in creation order.
    pub fn in_range(&self, start: usize, end: usize) -> impl Iterator<Item = &Overlay> {
        self.live
            .iter()
            .filter(move |o| o.end > start && o.start < end)
    }

    /// Editing one this buffer does not have is silently nothing — the handle
    /// may name a deleted overlay, or one another buffer owns.
    pub fn edit(&mut self, edit: OverlayEdit) {
        let id = match edit {
            OverlayEdit::Delete(id) => {
                self.live.retain(|o| o.id != id);
                return;
            }
            OverlayEdit::RemoveIn(start, end) => {
                self.live.retain(|o| !(o.end > start && o.start < end));
                return;
            }
            OverlayEdit::Face(id, _)
            | OverlayEdit::Background(id, _)
            | OverlayEdit::Display(id, _)
            | OverlayEdit::Image(id, _)
            | OverlayEdit::Scale(id, _)
            | OverlayEdit::Bold(id, _)
            | OverlayEdit::Italic(id, _)
            | OverlayEdit::LineBackground(id, _)
            | OverlayEdit::LinePrefix(id, _)
            | OverlayEdit::Fold(id, _) => id,
        };
        let Some(o) = self.live.iter_mut().find(|o| o.id == id) else {
            return;
        };
        match edit {
            OverlayEdit::Face(_, k) => o.face = k,
            OverlayEdit::Background(_, k) => o.background = k,
            OverlayEdit::Display(_, s) => o.display = s,
            OverlayEdit::Image(_, i) => o.image = i,
            // 100% is the body size, which is what "no scale" already means —
            // folded here so the renderer never has to ask the question twice.
            OverlayEdit::Scale(_, s) => o.scale = s.filter(|&s| s != 100),
            OverlayEdit::Bold(_, b) => o.bold = b,
            OverlayEdit::Italic(_, i) => o.italic = i,
            OverlayEdit::LineBackground(_, k) => o.line_background = k,
            OverlayEdit::LinePrefix(_, s) => o.line_prefix = s,
            OverlayEdit::Fold(_, f) => o.fold = f,
            OverlayEdit::Delete(_) | OverlayEdit::RemoveIn(..) => unreachable!("returned above"),
        }
    }

    pub fn clear(&mut self) {
        self.live.clear();
    }

    /// Every [`ImageId`] still referenced. The buffer prunes its bitmaps against
    /// this, so deleting the last overlay on an image frees it.
    pub fn images(&self) -> impl Iterator<Item = ImageId> + '_ {
        self.live.iter().filter_map(|o| o.image)
    }

    /// `[start, start + removed)` has just become `inserted` characters.
    ///
    /// Both ends move by the marker rule, with Emacs' default insertion types:
    /// the start `Stay`s, so text typed at the front lands *inside*, and the end
    /// `Stay`s too, so text typed at the back lands *outside*. ponytail: those
    /// are fixed rather than the `front-advance` / `rear-advance` pair
    /// `make-overlay` takes in Emacs. Nothing has wanted the other three
    /// combinations; adding them is two booleans on [`Overlay`] and two
    /// arguments on the primitive.
    pub fn adjust(&mut self, start: usize, removed: usize, inserted: usize) {
        for o in &mut self.live {
            o.start = adjust_pos(o.start, Insertion::Stay, start, removed, inserted);
            o.end = adjust_pos(o.end, Insertion::Stay, start, removed, inserted);
        }
        // An edit that swallowed the whole range collapses both ends onto its
        // own start. Emacs would keep the empty overlay; we drop it, because
        // every payload here is *about* the text underneath — a `display` string
        // over no text would still be drawn, which is a ghost nobody asked for.
        self.live.retain(|o| o.start < o.end);
    }

    /// Pull every overlay back inside a document that was replaced under them —
    /// undo restores a whole snapshot, so positions are all that survive.
    pub fn clamp(&mut self, len: usize) {
        for o in &mut self.live {
            o.start = o.start.min(len);
            o.end = o.end.min(len);
        }
        self.live.retain(|o| o.start < o.end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One overlay over `[start, end)`, so an invariant reads as a before/after.
    fn overlay(start: usize, end: usize) -> Overlays {
        let mut o = Overlays::default();
        o.add(1, start, end);
        o
    }

    /// Where `[5, 10)` ends up after the edit `(start, removed, inserted)`.
    fn after(edit: (usize, usize, usize)) -> Option<(usize, usize)> {
        let mut o = overlay(5, 10);
        o.adjust(edit.0, edit.1, edit.2);
        o.span(1)
    }

    #[test]
    fn an_edit_entirely_before_an_overlay_slides_it() {
        assert_eq!(after((0, 0, 3)), Some((8, 13)));
        assert_eq!(after((0, 3, 0)), Some((2, 7)));
        // ...including a deletion ending exactly where it starts.
        assert_eq!(after((2, 3, 0)), Some((2, 7)));
    }

    #[test]
    fn an_edit_entirely_after_an_overlay_leaves_it_alone() {
        assert_eq!(after((10, 0, 4)), Some((5, 10)));
        assert_eq!(after((12, 5, 0)), Some((5, 10)));
    }

    /// Emacs' default insertion types, and the whole reason an overlay is a
    /// marker *pair*: the two ends answer an insertion at the boundary
    /// differently.
    #[test]
    fn text_typed_at_the_front_lands_inside_and_at_the_back_outside() {
        assert_eq!(after((5, 0, 2)), Some((5, 12)));
        assert_eq!(after((10, 0, 2)), Some((5, 10)));
    }

    #[test]
    fn an_edit_inside_an_overlay_resizes_it() {
        assert_eq!(after((7, 0, 3)), Some((5, 13)));
        assert_eq!(after((7, 2, 0)), Some((5, 8)));
        assert_eq!(after((7, 2, 5)), Some((5, 13)));
    }

    /// The cases the marker machinery exists for: an edit that straddles one
    /// end, or contains the whole thing.
    #[test]
    fn a_deletion_straddling_an_end_clips_the_overlay_to_what_survived() {
        // over the tail
        assert_eq!(after((8, 5, 0)), Some((5, 8)));
        // over the head
        assert_eq!(after((3, 4, 0)), Some((3, 6)));
        // over the head, replaced by more text than it removed
        assert_eq!(after((3, 4, 6)), Some((3, 12)));
    }

    #[test]
    fn an_overlay_whose_text_is_deleted_is_deleted_with_it() {
        assert_eq!(after((5, 5, 0)), None);
        assert_eq!(after((0, 20, 0)), None);
        // ...but a replacement of exactly the range keeps it, over the new text:
        // there is still something for the payload to be about.
        assert_eq!(after((5, 5, 3)), Some((5, 8)));
    }

    #[test]
    fn clamping_pulls_an_overlay_into_a_document_that_shrank() {
        let mut o = overlay(5, 10);
        o.clamp(7);
        assert_eq!(o.span(1), Some((5, 7)));
        o.clamp(3);
        assert_eq!(o.span(1), None, "nothing left for it to be about");
    }

    #[test]
    fn in_range_is_overlap_rather_than_containment() {
        let mut o = Overlays::default();
        o.add(1, 0, 5);
        o.add(2, 5, 10);
        o.add(3, 10, 15);
        let ids = |a, b| o.in_range(a, b).map(|o| o.id).collect::<Vec<_>>();
        assert_eq!(ids(4, 11), vec![1, 2, 3]);
        // Touching at a boundary is not overlapping, at either end.
        assert_eq!(ids(5, 10), vec![2]);
        assert_eq!(ids(0, 0), Vec::<OverlayId>::new());
    }

    #[test]
    fn properties_are_set_and_cleared_by_handle() {
        let mut o = overlay(5, 10);
        o.edit(OverlayEdit::Face(1, Some(HlKind::Keyword)));
        o.edit(OverlayEdit::Display(1, Some("◉".into())));
        assert_eq!(o.all()[0].face, Some(HlKind::Keyword));
        assert_eq!(o.all()[0].display.as_deref(), Some("◉"));
        o.edit(OverlayEdit::Display(1, None));
        assert_eq!(o.all()[0].display, None);
        // A handle no overlay has is silently nothing, never a panic: it came
        // from arithmetic in Lisp and can be anything.
        o.edit(OverlayEdit::Face(99, Some(HlKind::String)));
        o.edit(OverlayEdit::Delete(99));
        assert_eq!(o.all().len(), 1);
        o.edit(OverlayEdit::Delete(1));
        assert!(o.all().is_empty());
    }

    /// The attributes that arrived with typesetting. Tri-state throughout, and
    /// that is the part worth a test: `Some(false)` is a claim ("upright") and
    /// `None` is the absence of one, which is what lets a later overlay take an
    /// earlier one's bold off without also clearing its size.
    #[test]
    fn rich_display_attributes_are_set_and_cleared_by_handle() {
        let mut o = overlay(5, 10);
        o.edit(OverlayEdit::Scale(1, Some(150)));
        o.edit(OverlayEdit::Bold(1, Some(true)));
        o.edit(OverlayEdit::Italic(1, Some(false)));
        o.edit(OverlayEdit::LineBackground(1, Some(HlKind::Code)));
        o.edit(OverlayEdit::LinePrefix(1, Some("▎ ".into())));
        let at = |o: &Overlays| {
            let o = &o.all()[0];
            (o.scale, o.bold, o.italic, o.line_background, o.line_prefix.clone())
        };
        assert_eq!(
            at(&o),
            (Some(150), Some(true), Some(false), Some(HlKind::Code), Some("▎ ".into()))
        );
        // Body size is not a scale, however it is spelled: the renderer asks
        // "is this line bigger than the text", and 100% is not.
        o.edit(OverlayEdit::Scale(1, Some(100)));
        assert_eq!(at(&o).0, None);
        // Clearing one leaves the rest, which is the whole of "per attribute".
        o.edit(OverlayEdit::Bold(1, None));
        assert_eq!(at(&o).1, None);
        assert_eq!(at(&o).3, Some(HlKind::Code));
        o.edit(OverlayEdit::LinePrefix(1, None));
        assert_eq!(at(&o).4, None);
        // A handle no overlay has stays silent here too — these arrive from the
        // same arithmetic in Lisp as every other edit.
        o.edit(OverlayEdit::Scale(99, Some(200)));
        assert_eq!(o.all().len(), 1);
    }

    /// Folding, as the one predicate the renderer and `j` both ask. "abc\ndef\n
    /// ghi\njkl" has line starts 0, 4, 8, 12; a fold over `[2, 11)` begins on
    /// line 0 and reaches into line 2.
    #[test]
    fn a_fold_hides_the_lines_after_its_own() {
        let mut o = overlay(2, 11);
        o.edit(OverlayEdit::Fold(1, true));
        let all = o.all();
        // The line the fold *starts* on is still drawn — it is the headline,
        // and hiding it would hide the thing you fold from.
        assert_eq!(fold_hiding(all, 0), None);
        assert_eq!(fold_hiding(all, 4), Some(2));
        assert_eq!(fold_hiding(all, 8), Some(2));
        // Half-open: the line beginning exactly where the fold ends is outside.
        assert_eq!(fold_hiding(all, 11), None);
        assert_eq!(fold_hiding(all, 12), None);
        // ...and the indicator goes on the line the fold begins on, found by
        // asking with that line's own range.
        assert!(fold_starts_in(all, 0, 3));
        assert!(!fold_starts_in(all, 4, 7));

        // An ordinary overlay is not a fold, however much of the buffer it
        // covers: `fold` is a claim, not a consequence of having a range.
        let mut plain = overlay(2, 11);
        plain.edit(OverlayEdit::Display(1, Some("x".into())));
        assert_eq!(fold_hiding(plain.all(), 4), None);
        // ...and unfolding is the same edit the other way, so one overlay can
        // be cycled without being remade.
        o.edit(OverlayEdit::Fold(1, false));
        assert_eq!(fold_hiding(o.all(), 4), None);
    }

    /// A fold is a marker pair like every other overlay, so it *moves* — and a
    /// fold that stopped covering the text it was folding would hide the wrong
    /// lines, which is worse than not folding at all.
    #[test]
    fn a_fold_follows_the_text_it_hides() {
        let mut o = overlay(2, 11);
        o.edit(OverlayEdit::Fold(1, true));
        o.adjust(0, 0, 4); // four characters typed above it
        assert_eq!(o.span(1), Some((6, 15)));
        assert_eq!(fold_hiding(o.all(), 8), Some(6));
        // Delete everything it covered and the fold goes with it, so nothing is
        // left hiding lines that are no longer there.
        o.adjust(6, 9, 0);
        assert_eq!(fold_hiding(o.all(), 8), None);
    }

    #[test]
    fn remove_in_takes_everything_that_overlaps() {
        let mut o = Overlays::default();
        o.add(1, 0, 5);
        o.add(2, 5, 10);
        o.add(3, 10, 15);
        o.edit(OverlayEdit::RemoveIn(4, 6));
        assert_eq!(o.all().iter().map(|o| o.id).collect::<Vec<_>>(), vec![3]);
    }
}
