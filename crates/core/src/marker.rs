//! Positions that survive editing.
//!
//! A char offset is a fact about one version of the text: insert a character in
//! front of it and it names its neighbour instead. Anything holding a position
//! across an edit — a Lisp command that reads point, thinks, and comes back, or
//! a "return to where I was" binding — needs the offset moved for it, and that
//! is all a marker is.
//!
//! [`Markers::adjust`] has exactly one caller, `Buffer::splice`, because the bug
//! this exists to prevent is an edit path that forgets to adjust.

/// A marker as Lisp holds it — a plain integer, so it survives the reader on
/// the way back out. Allocated by `Editor::make_marker`, and unique across
/// every buffer so a handle can never be mistaken for another buffer's.
pub type MarkerId = u64;

/// What an insertion *exactly at* a marker does to it, Emacs' insertion type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Insertion {
    /// Text typed at the marker lands after it. Emacs' default, and the one
    /// that makes a marker feel like it is holding a place rather than a cursor.
    #[default]
    Stay,
    /// The marker rides along to the end of the inserted text.
    Advance,
}

#[derive(Clone, Copy, Debug)]
struct Marker {
    id: MarkerId,
    pos: usize,
    insertion: Insertion,
}

/// The markers of one buffer, in char offsets.
///
/// ponytail: a `Vec` scanned linearly. Markers are placed by hand — a few per
/// buffer, not one per line — so a map keyed by id would cost more to keep than
/// the scan it saves. Reach for one when something starts putting a marker on
/// every hit of a search.
#[derive(Default)]
pub struct Markers {
    live: Vec<Marker>,
}

impl Markers {
    pub fn add(&mut self, id: MarkerId, pos: usize, insertion: Insertion) {
        self.live.push(Marker { id, pos, insertion });
    }

    pub fn position(&self, id: MarkerId) -> Option<usize> {
        self.live.iter().find(|m| m.id == id).map(|m| m.pos)
    }

    /// Moving a marker this buffer does not have is silently nothing: the handle
    /// may name one that was deleted, or one another buffer owns.
    pub fn set(&mut self, id: MarkerId, pos: usize) {
        if let Some(m) = self.live.iter_mut().find(|m| m.id == id) {
            m.pos = pos;
        }
    }

    pub fn remove(&mut self, id: MarkerId) {
        self.live.retain(|m| m.id != id);
    }

    pub fn clear(&mut self) {
        self.live.clear();
    }

    /// `[start, start + removed)` has just become `inserted` characters.
    pub fn adjust(&mut self, start: usize, removed: usize, inserted: usize) {
        for m in &mut self.live {
            m.pos = adjust_pos(m.pos, m.insertion, start, removed, inserted);
        }
    }

    /// Pull every marker back inside a document that was replaced under them.
    pub fn clamp(&mut self, len: usize) {
        for m in &mut self.live {
            m.pos = m.pos.min(len);
        }
    }
}

/// Where `pos` lands once `[start, start + removed)` has become `inserted`
/// characters.
///
/// A free function rather than a method because an overlay is a *pair* of these
/// and lives in [`crate::overlay`] — one formula, two callers, so the two can
/// never drift into disagreeing about what an insertion at a boundary means.
pub fn adjust_pos(
    pos: usize,
    insertion: Insertion,
    start: usize,
    removed: usize,
    inserted: usize,
) -> usize {
    let end = start + removed;
    match pos {
        p if p < start => p,
        // At the edit itself, where the insertion type is the whole question. A
        // deletion starting here answers it the same way: there is nothing left
        // between the marker and the new text.
        p if p == start => match insertion {
            Insertion::Stay => start,
            Insertion::Advance => start + inserted,
        },
        // Swallowed by the deletion — collapse onto its start, the only
        // position of the old range that still exists.
        p if p < end => start,
        p => p - removed + inserted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One marker, so an invariant reads as a single before/after pair.
    fn marker(pos: usize, insertion: Insertion) -> Markers {
        let mut markers = Markers::default();
        markers.add(1, pos, insertion);
        markers
    }

    /// Where a marker at `pos` ends up after the edit `(start, removed, inserted)`.
    fn after(pos: usize, insertion: Insertion, edit: (usize, usize, usize)) -> usize {
        let mut markers = marker(pos, insertion);
        markers.adjust(edit.0, edit.1, edit.2);
        markers.position(1).expect("the marker is still live")
    }

    #[test]
    fn an_insert_before_a_marker_pushes_it_along() {
        assert_eq!(after(10, Insertion::Stay, (3, 0, 2)), 12);
    }

    #[test]
    fn an_insert_after_a_marker_leaves_it_alone() {
        assert_eq!(after(10, Insertion::Stay, (11, 0, 5)), 10);
    }

    #[test]
    fn an_insert_at_a_marker_obeys_its_insertion_type() {
        assert_eq!(after(10, Insertion::Stay, (10, 0, 4)), 10);
        assert_eq!(after(10, Insertion::Advance, (10, 0, 4)), 14);
    }

    #[test]
    fn a_delete_before_a_marker_pulls_it_earlier() {
        assert_eq!(after(10, Insertion::Stay, (2, 3, 0)), 7);
        // ...including one ending exactly where the marker is, which is still
        // entirely before it.
        assert_eq!(after(10, Insertion::Stay, (7, 3, 0)), 7);
    }

    #[test]
    fn a_delete_after_a_marker_leaves_it_alone() {
        assert_eq!(after(10, Insertion::Stay, (10, 5, 0)), 10);
        assert_eq!(after(10, Insertion::Stay, (20, 5, 0)), 10);
    }

    #[test]
    fn a_delete_spanning_a_marker_leaves_it_at_the_start() {
        assert_eq!(after(10, Insertion::Stay, (8, 5, 0)), 8);
        // Both insertion types: the text the marker was between is gone, so
        // there is no side left to be on.
        assert_eq!(after(10, Insertion::Advance, (8, 5, 0)), 8);
    }

    #[test]
    fn a_replacement_moves_a_later_marker_by_the_difference() {
        assert_eq!(after(10, Insertion::Stay, (2, 3, 6)), 13);
    }

    #[test]
    fn a_deleted_marker_has_no_position() {
        let mut markers = marker(10, Insertion::Stay);
        markers.remove(1);
        assert_eq!(markers.position(1), None);
        assert_eq!(markers.position(99), None);
    }

    #[test]
    fn clamping_pulls_a_marker_into_a_document_that_shrank() {
        let mut markers = marker(10, Insertion::Stay);
        markers.clamp(4);
        assert_eq!(markers.position(1), Some(4));
    }
}
