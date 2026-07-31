//! Frames, windows, and the layout that arranges them.
//!
//! The Emacs vocabulary, because it is the one that distinguishes the three
//! things that actually differ:
//!
//! * a **buffer** is text — it lives in the editor, not in any window;
//! * a **window** is a viewport onto a buffer, with its own cursor and scroll,
//!   so the same buffer shown twice scrolls independently;
//! * a **frame** is an OS window, holding a tree of windows.
//!
//! Layout is a binary tree and the arithmetic here is **pure**: [`Frame::panes`]
//! turns a tree plus a rectangle into per-window rectangles, and nothing in this
//! file knows about SDL. That is deliberate — the renderer and the mouse
//! hit-testing in the app must agree exactly on where a divider is, and the only
//! way to guarantee that is for both to call the same function.
//!
//! The split directions are named after the *result* rather than the divider,
//! because "horizontal split" means opposite things in vim and Emacs:
//! [`Split::Columns`] puts windows side by side, [`Split::Rows`] stacks them.

/// Stable handle for a buffer. Indices would not survive buffers being closed.
pub type BufferId = usize;

/// Stable handle for a window within its frame.
pub type WindowId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

/// How a split arranges its two children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Split {
    /// Side by side, divider running top-to-bottom. `C-<ret>`.
    Columns,
    /// Stacked, divider running left-to-right. `C-M-<ret>`.
    Rows,
}

/// Which child of a split — the path type for addressing a divider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    First,
    Second,
}

/// The window tree. A leaf is one window; a split is two subtrees and the
/// fraction of the space the first one gets.
#[derive(Clone, Debug, PartialEq)]
pub enum Layout {
    Leaf(WindowId),
    Split {
        dir: Split,
        /// Share of the parent given to `first`, clamped to [`MIN_RATIO`].
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

/// Smallest share a pane can be dragged to — enough to stay grabbable.
pub const MIN_RATIO: f32 = 0.08;
/// Divider thickness in pixels. Also the mouse grab tolerance.
pub const DIVIDER: i32 = 4;

/// A window and the rectangle it occupies.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pane {
    pub window: WindowId,
    pub rect: Rect,
}

/// A draggable divider: where it is, which way it moves, and how to address the
/// split it belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    pub rect: Rect,
    pub dir: Split,
    /// Route from the root to the split, so [`Frame::set_ratio`] can find it.
    pub path: Vec<Side>,
    /// The area the split was given — the drag converts a pixel to a ratio
    /// against this.
    pub area: Rect,
}

/// A viewport onto a buffer.
#[derive(Clone, Debug)]
pub struct Window {
    pub id: WindowId,
    pub buffer: BufferId,
    /// Cursor and scroll live here, not in the buffer: the same buffer shown in
    /// two windows has two cursors.
    pub cursor: usize,
    pub scroll: usize,
    /// Lines that fit, written by the renderer each frame.
    pub viewport_lines: usize,
    /// Cells of text this window has across, gutter already taken out — written
    /// by the renderer each frame, exactly as `viewport_lines` is and for the
    /// same reason: only the renderer knows how wide a cell is in pixels or how
    /// many digits the line numbers claimed. It is what makes a *visual* line a
    /// thing core can reason about, and `0` means "nothing has drawn this window
    /// yet", which every reader treats as "no wrapping".
    pub wrap_cols: usize,
}

/// An OS window holding a tree of editor windows.
pub struct Frame {
    pub windows: Vec<Window>,
    pub layout: Layout,
    pub current: WindowId,
    next_id: WindowId,
}

impl Frame {
    pub fn new(buffer: BufferId) -> Self {
        Self {
            windows: vec![Window {
                id: 0,
                buffer,
                cursor: 0,
                scroll: 0,
                viewport_lines: 24,
                wrap_cols: 0,
            }],
            layout: Layout::Leaf(0),
            current: 0,
            next_id: 1,
        }
    }

    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    pub fn current_window(&self) -> &Window {
        self.window(self.current).unwrap_or(&self.windows[0])
    }

    pub fn current_window_mut(&mut self) -> &mut Window {
        let id = self.current;
        match self.windows.iter().position(|w| w.id == id) {
            Some(i) => &mut self.windows[i],
            None => &mut self.windows[0],
        }
    }

    /// Split the current window, the new one showing the same buffer at the
    /// same position, and focus it — the vim behaviour.
    pub fn split(&mut self, dir: Split) -> WindowId {
        let current = self.current_window().clone();
        let id = self.next_id;
        self.next_id += 1;
        self.windows.push(Window {
            id,
            buffer: current.buffer,
            cursor: current.cursor,
            scroll: current.scroll,
            viewport_lines: current.viewport_lines,
            wrap_cols: current.wrap_cols,
        });
        replace_leaf(&mut self.layout, current.id, |leaf| Layout::Split {
            dir,
            ratio: 0.5,
            first: Box::new(leaf),
            second: Box::new(Layout::Leaf(id)),
        });
        self.current = id;
        id
    }

    /// Close the current window; its sibling subtree takes the space. The last
    /// window in a frame cannot be closed — a frame always shows something.
    pub fn close_current(&mut self) -> bool {
        if self.windows.len() < 2 {
            return false;
        }
        let gone = self.current;
        if !remove_leaf(&mut self.layout, gone) {
            return false;
        }
        self.windows.retain(|w| w.id != gone);
        self.current = first_leaf(&self.layout);
        true
    }

    /// Focus the next window in tree order, wrapping.
    pub fn focus_next(&mut self) {
        let order = leaves(&self.layout);
        let at = order.iter().position(|&id| id == self.current).unwrap_or(0);
        self.current = order[(at + 1) % order.len()];
    }

    pub fn focus(&mut self, id: WindowId) {
        if self.window(id).is_some() {
            self.current = id;
        }
    }

    /// A label per window, in tree order, for ace-window style jumping.
    ///
    /// Home-row keys rather than digits: they are where your fingers already
    /// are, and digits are counts everywhere else in this editor.
    pub fn ace_labels(&self) -> Vec<(char, WindowId)> {
        const KEYS: &[char] = &['a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l'];
        leaves(&self.layout)
            .into_iter()
            .zip(KEYS.iter().copied())
            .map(|(id, key)| (key, id))
            .collect()
    }

    /// Every window's rectangle within `area`.
    pub fn panes(&self, area: Rect) -> Vec<Pane> {
        let mut out = Vec::new();
        collect_panes(&self.layout, area, &mut out);
        out
    }

    /// Every divider within `area`, for drawing and for mouse hit-testing.
    pub fn dividers(&self, area: Rect) -> Vec<Divider> {
        let mut out = Vec::new();
        collect_dividers(&self.layout, area, &mut Vec::new(), &mut out);
        out
    }

    /// The window under a point, if any.
    pub fn window_at(&self, area: Rect, x: i32, y: i32) -> Option<WindowId> {
        self.panes(area)
            .into_iter()
            .find(|p| p.rect.contains(x, y))
            .map(|p| p.window)
    }

    /// The divider under a point, within [`DIVIDER`] pixels.
    pub fn divider_at(&self, area: Rect, x: i32, y: i32) -> Option<Divider> {
        self.dividers(area).into_iter().find(|d| {
            let grab = match d.dir {
                Split::Columns => Rect::new(d.rect.x - DIVIDER, d.rect.y, d.rect.w + DIVIDER * 2, d.rect.h),
                Split::Rows => Rect::new(d.rect.x, d.rect.y - DIVIDER, d.rect.w, d.rect.h + DIVIDER * 2),
            };
            grab.contains(x, y)
        })
    }

    /// Move the split addressed by `path` so its divider lands on a pixel.
    pub fn drag_divider(&mut self, divider: &Divider, x: i32, y: i32) {
        let area = divider.area;
        let ratio = match divider.dir {
            Split::Columns if area.w > 0 => (x - area.x) as f32 / area.w as f32,
            Split::Rows if area.h > 0 => (y - area.y) as f32 / area.h as f32,
            _ => return,
        };
        self.set_ratio(&divider.path, ratio);
    }

    pub fn set_ratio(&mut self, path: &[Side], ratio: f32) {
        let mut node = &mut self.layout;
        for step in path {
            let Layout::Split { first, second, .. } = node else {
                return;
            };
            node = match step {
                Side::First => first,
                Side::Second => second,
            };
        }
        if let Layout::Split { ratio: r, .. } = node {
            *r = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
        }
    }
}

// --- tree helpers ---------------------------------------------------------

fn leaves(node: &Layout) -> Vec<WindowId> {
    match node {
        Layout::Leaf(id) => vec![*id],
        Layout::Split { first, second, .. } => {
            let mut v = leaves(first);
            v.extend(leaves(second));
            v
        }
    }
}

fn first_leaf(node: &Layout) -> WindowId {
    match node {
        Layout::Leaf(id) => *id,
        Layout::Split { first, .. } => first_leaf(first),
    }
}

/// Swap the leaf holding `id` for whatever `make` builds from it.
fn replace_leaf(node: &mut Layout, id: WindowId, make: impl FnOnce(Layout) -> Layout) {
    match node {
        Layout::Leaf(found) if *found == id => {
            let leaf = Layout::Leaf(id);
            *node = make(leaf);
        }
        Layout::Leaf(_) => {}
        Layout::Split { first, second, .. } => {
            // The closure is FnOnce, so try one side then the other by value.
            if leaves(first).contains(&id) {
                replace_leaf(first, id, make);
            } else {
                replace_leaf(second, id, make);
            }
        }
    }
}

/// Drop the leaf holding `id`, collapsing its parent into the sibling.
fn remove_leaf(node: &mut Layout, id: WindowId) -> bool {
    let Layout::Split { first, second, .. } = node else {
        return false; // a lone leaf: nothing to collapse into
    };
    let keep = match (first.as_ref(), second.as_ref()) {
        (Layout::Leaf(a), _) if *a == id => Some(second.as_ref().clone()),
        (_, Layout::Leaf(b)) if *b == id => Some(first.as_ref().clone()),
        _ => None,
    };
    match keep {
        Some(sibling) => {
            *node = sibling;
            true
        }
        None => remove_leaf(first, id) || remove_leaf(second, id),
    }
}

/// Split `area` into the two child rectangles, leaving [`DIVIDER`] between.
fn halves(area: Rect, dir: Split, ratio: f32) -> (Rect, Rect, Rect) {
    let ratio = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
    match dir {
        Split::Columns => {
            let usable = (area.w - DIVIDER).max(0);
            let a = (usable as f32 * ratio).round() as i32;
            (
                Rect::new(area.x, area.y, a, area.h),
                Rect::new(area.x + a + DIVIDER, area.y, usable - a, area.h),
                Rect::new(area.x + a, area.y, DIVIDER, area.h),
            )
        }
        Split::Rows => {
            let usable = (area.h - DIVIDER).max(0);
            let a = (usable as f32 * ratio).round() as i32;
            (
                Rect::new(area.x, area.y, area.w, a),
                Rect::new(area.x, area.y + a + DIVIDER, area.w, usable - a),
                Rect::new(area.x, area.y + a, area.w, DIVIDER),
            )
        }
    }
}

fn collect_panes(node: &Layout, area: Rect, out: &mut Vec<Pane>) {
    match node {
        Layout::Leaf(id) => out.push(Pane {
            window: *id,
            rect: area,
        }),
        Layout::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let (a, b, _) = halves(area, *dir, *ratio);
            collect_panes(first, a, out);
            collect_panes(second, b, out);
        }
    }
}

fn collect_dividers(node: &Layout, area: Rect, path: &mut Vec<Side>, out: &mut Vec<Divider>) {
    let Layout::Split {
        dir,
        ratio,
        first,
        second,
    } = node
    else {
        return;
    };
    let (a, b, bar) = halves(area, *dir, *ratio);
    out.push(Divider {
        rect: bar,
        dir: *dir,
        path: path.clone(),
        area,
    });
    path.push(Side::First);
    collect_dividers(first, a, path, out);
    path.pop();
    path.push(Side::Second);
    collect_dividers(second, b, path, out);
    path.pop();
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        w: 1000,
        h: 600,
    };

    #[test]
    fn one_window_fills_the_frame() {
        let f = Frame::new(0);
        let panes = f.panes(AREA);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].rect, AREA);
        assert!(f.dividers(AREA).is_empty());
    }

    #[test]
    fn columns_put_windows_side_by_side() {
        let mut f = Frame::new(0);
        f.split(Split::Columns);
        let panes = f.panes(AREA);
        assert_eq!(panes.len(), 2);
        // same top and height, different x — side by side
        assert_eq!(panes[0].rect.y, panes[1].rect.y);
        assert_eq!(panes[0].rect.h, panes[1].rect.h);
        assert!(panes[0].rect.x < panes[1].rect.x);
        // and they tile the area exactly, divider included
        assert_eq!(
            panes[0].rect.w + panes[1].rect.w + DIVIDER,
            AREA.w
        );
    }

    #[test]
    fn rows_stack_windows() {
        let mut f = Frame::new(0);
        f.split(Split::Rows);
        let panes = f.panes(AREA);
        assert_eq!(panes[0].rect.x, panes[1].rect.x);
        assert_eq!(panes[0].rect.w, panes[1].rect.w);
        assert!(panes[0].rect.y < panes[1].rect.y);
        assert_eq!(panes[0].rect.h + panes[1].rect.h + DIVIDER, AREA.h);
    }

    #[test]
    fn a_split_shares_the_buffer_and_position() {
        let mut f = Frame::new(7);
        f.current_window_mut().cursor = 42;
        f.current_window_mut().scroll = 5;
        let new = f.split(Split::Columns);
        assert_eq!(f.window(new).unwrap().buffer, 7);
        assert_eq!(f.window(new).unwrap().cursor, 42);
        assert_eq!(f.window(new).unwrap().scroll, 5);
        // focus follows the new window
        assert_eq!(f.current, new);
    }

    #[test]
    fn nested_splits_tile_without_gaps_or_overlap() {
        let mut f = Frame::new(0);
        f.split(Split::Columns);
        f.split(Split::Rows);
        f.split(Split::Columns);
        let panes = f.panes(AREA);
        assert_eq!(panes.len(), 4);
        for (i, a) in panes.iter().enumerate() {
            for b in panes.iter().skip(i + 1) {
                let overlaps = a.rect.x < b.rect.x + b.rect.w
                    && b.rect.x < a.rect.x + a.rect.w
                    && a.rect.y < b.rect.y + b.rect.h
                    && b.rect.y < a.rect.y + a.rect.h;
                assert!(!overlaps, "{:?} overlaps {:?}", a.rect, b.rect);
            }
            assert!(a.rect.w >= 0 && a.rect.h >= 0);
        }
    }

    #[test]
    fn closing_a_window_gives_its_space_to_the_sibling() {
        let mut f = Frame::new(0);
        f.split(Split::Columns);
        assert!(f.close_current());
        let panes = f.panes(AREA);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].rect, AREA);
        // the last window cannot be closed
        assert!(!f.close_current());
        assert_eq!(f.windows.len(), 1);
    }

    #[test]
    fn focus_cycles_through_every_window() {
        let mut f = Frame::new(0);
        f.split(Split::Columns);
        f.split(Split::Rows);
        let start = f.current;
        let mut seen = vec![start];
        for _ in 0..2 {
            f.focus_next();
            seen.push(f.current);
        }
        let mut distinct = seen.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), 3, "every window is reachable: {seen:?}");
        // one more step wraps back to where the cycle started
        f.focus_next();
        assert_eq!(f.current, start);
    }

    #[test]
    fn dividers_sit_between_panes_and_are_grabbable() {
        let mut f = Frame::new(0);
        f.split(Split::Columns);
        let d = &f.dividers(AREA)[0];
        assert_eq!(d.dir, Split::Columns);
        let panes = f.panes(AREA);
        assert_eq!(d.rect.x, panes[0].rect.x + panes[0].rect.w);
        // grabbable slightly off-center, which is what makes it usable
        assert!(f.divider_at(AREA, d.rect.x + 1, 300).is_some());
        assert!(f.divider_at(AREA, d.rect.x - 2, 300).is_some());
        assert!(f.divider_at(AREA, 10, 300).is_none());
    }

    #[test]
    fn dragging_a_divider_moves_the_boundary() {
        let mut f = Frame::new(0);
        f.split(Split::Columns);
        let d = f.dividers(AREA)[0].clone();
        f.drag_divider(&d, 250, 300);
        let panes = f.panes(AREA);
        assert!((panes[0].rect.w - 250).abs() <= DIVIDER);

        // and it cannot be dragged off the edge
        f.drag_divider(&d, -500, 300);
        assert!(f.panes(AREA)[0].rect.w > 0);
        f.drag_divider(&d, 99_999, 300);
        assert!(f.panes(AREA)[1].rect.w > 0);
    }

    #[test]
    fn dragging_an_inner_divider_leaves_the_outer_one_alone() {
        let mut f = Frame::new(0);
        f.split(Split::Columns); // outer
        f.split(Split::Rows); // inner, inside the right column
        let dividers = f.dividers(AREA);
        assert_eq!(dividers.len(), 2);
        let outer_before = f.panes(AREA)[0].rect.w;

        let inner = dividers.iter().find(|d| d.dir == Split::Rows).unwrap().clone();
        f.drag_divider(&inner, inner.rect.x, 150);
        assert_eq!(f.panes(AREA)[0].rect.w, outer_before);
    }

    #[test]
    fn window_at_finds_the_pane_under_the_mouse() {
        let mut f = Frame::new(0);
        let left = f.current;
        let right = f.split(Split::Columns);
        assert_eq!(f.window_at(AREA, 10, 10), Some(left));
        assert_eq!(f.window_at(AREA, 990, 10), Some(right));
    }

    #[test]
    fn a_tiny_frame_never_produces_negative_rectangles() {
        let mut f = Frame::new(0);
        f.split(Split::Columns);
        f.split(Split::Rows);
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, DIVIDER, DIVIDER),
            Rect::new(0, 0, 20, 8),
        ] {
            for p in f.panes(area) {
                assert!(p.rect.w >= 0 && p.rect.h >= 0, "{:?} from {area:?}", p.rect);
            }
        }
    }
}
