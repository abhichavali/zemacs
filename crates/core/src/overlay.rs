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
//! string, an image. Everything else a config wants to hang off an overlay — an
//! avy hint's target, a diagnostic's message, a closure — stays in the Lisp
//! image, in a hash table keyed by the same handle. That is the boundary rule
//! working as intended: Rust owns the storage the frame reads, Lisp owns the
//! property list, and an arbitrary Lisp value never has to survive a round trip
//! through C as printed source.
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
        }
    }
}

/// A change to an overlay, as it arrives from Lisp.
///
/// One [`EditorCommand`](crate::EditorCommand) variant carries all of these
/// rather than six flat ones: they share a handle and a destination, and none of
/// them touches the document — an overlay is drawing, not text, which is why
/// `mutates_document` says no and a face can therefore be put on a *generated*
/// buffer like dired or magit.
#[derive(Clone, Debug, PartialEq)]
pub enum OverlayEdit {
    /// `None` clears the face.
    Face(OverlayId, Option<HlKind>),
    Background(OverlayId, Option<HlKind>),
    Display(OverlayId, Option<String>),
    Image(OverlayId, Option<ImageId>),
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
            | OverlayEdit::Image(id, _) => id,
        };
        let Some(o) = self.live.iter_mut().find(|o| o.id == id) else {
            return;
        };
        match edit {
            OverlayEdit::Face(_, k) => o.face = k,
            OverlayEdit::Background(_, k) => o.background = k,
            OverlayEdit::Display(_, s) => o.display = s,
            OverlayEdit::Image(_, i) => o.image = i,
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
