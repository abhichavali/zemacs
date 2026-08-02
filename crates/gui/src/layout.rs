//! Where every node ends up, and which one a click landed on.
//!
//! One walk down the tree to ask each node how big it wants to be, and one back
//! up placing it. That is the whole engine, and it is why [`Length`] has four
//! cases and not fourteen: `Auto` is answered by the walk down, `Fill` by the
//! walk back up, and the two never have to iterate to a fixed point.
//!
//! Nothing here is incremental. A scene relays out whole when it changes, which
//! nothing does per keystroke because nothing types into one — see the ceiling
//! written down in `docs/gui.org`.

use crate::{Align, Block, Dir, Length, Measure, Node, NodeId, Rect, Run, Scene, Tag};

/// How deep the tree may nest before layout gives up on it.
///
/// ponytail: a depth cap rather than a cycle check. Node ids are integers Lisp
/// computed, so a block can name a block that names it back — `push` cannot
/// prevent it, since the second node is pushed after the first already listed
/// it. A cap costs one comparison per node and turns "the editor hangs" into "a
/// pathological scene is truncated". Ceiling: a document genuinely nested 64
/// deep, which no prose is. The upgrade path is marking nodes as they are
/// entered and refusing a repeat, which costs a bitset the size of the arena.
const MAX_DEPTH: u32 = 64;

/// Every node's place on screen, flat and in paint order.
///
/// Flat because painting wants one loop and hit testing wants the same loop
/// backwards, and a tree would make one of them recursive for no gain. Parents
/// come before their children and siblings come in order, so the *last* frame
/// containing a point is the innermost thing under it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub frames: Vec<Frame>,
}

impl Layout {
    /// Where a node ended up, for the commands that mean "go there".
    ///
    /// Four commands in the curriculum modes are `goto-char` today — next
    /// problem, previous problem, jump to unit — and in a scene there is no
    /// point to move: going somewhere is scrolling that node into view, which
    /// needs its rect and nothing else.
    ///
    /// ponytail: a scan of the frame list rather than an index beside it. The
    /// list is a document's worth of boxes and this is called on a keystroke
    /// that jumps, not on the frame loop. Ceiling: a mode that asks per frame,
    /// which would want a `Vec<usize>` from node id to frame built once here.
    pub fn rect_of(&self, node: NodeId) -> Option<Rect> {
        self.frames.iter().find(|f| f.node == node).map(|f| f.rect)
    }

    /// How tall the whole scene is, which is the number a scroll offset has to
    /// be clamped against.
    ///
    /// The bottom of the lowest box *less the top of the root*, so the answer is
    /// independent of the scroll already taken out of every rect. That matters
    /// because the only moment anyone asks is while scrolling, and a height that
    /// shrank as you scrolled would let the document walk out from under itself.
    pub fn content_height(&self) -> i32 {
        let top = self.frames.first().map_or(0, |f| f.rect.y);
        self.frames
            .iter()
            .map(|f| f.rect.y + f.rect.h)
            .max()
            .map_or(0, |bottom| bottom - top)
    }
}

/// One node, placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub node: NodeId,
    /// Absolute, in the viewport's own coordinates, with the scene's scroll
    /// already taken out — so a click can be tested against it directly.
    pub rect: Rect,
    /// The nearest [`Tag`] enclosing this node, resolved on the way down.
    ///
    /// Carried here rather than looked up later because [`hit`] takes a layout
    /// and nothing else: a click arrives in the event loop, which has no
    /// business holding the scene to answer it.
    pub tag: Option<Tag>,
    /// The lines a [`Node::Text`] broke into. Empty for every other kind.
    pub lines: Vec<Line>,
}

/// One line of a wrapped paragraph.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    /// Top of the line, relative to its frame's rect.
    pub y: i32,
    /// The ascent plus the descent — how far the next line has to start down.
    pub height: i32,
    /// Where the glyphs sit, measured down from the line's own top. The largest
    /// ascent claimed by anything on the line, so runs of different sizes, and
    /// an inline equation, all sit on one baseline instead of each floating in
    /// its own band.
    pub baseline: i32,
    /// Left to right.
    pub pieces: Vec<Piece>,
}

impl Line {
    /// How far below the baseline the line reaches — a descender, or the depth
    /// of an inline bitmap. Not a field, because it is exactly this.
    pub fn descent(&self) -> i32 {
        self.height - self.baseline
    }
}

/// As much of one run as landed on one line.
///
/// A byte range rather than a copy of the text: the painter has the scene, and
/// a paragraph that stored its own words would go out of step with the document
/// the moment the scene was rebuilt around it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    /// Which run of the [`Node::Text`] this came from.
    pub run: usize,
    /// Byte offsets into that run's `text`, `[start, end)`, always on character
    /// boundaries — and both zero for a [`Run::Image`], which has no text to
    /// slice and is always drawn whole.
    pub start: usize,
    pub end: usize,
    /// Left edge, relative to the frame's rect.
    pub x: i32,
    pub width: i32,
    /// The run's own tag, copied here for the reason [`Frame::tag`] is.
    pub tag: Option<Tag>,
}

/// Place every node of `scene` inside `viewport`, measuring text with `m`.
///
/// The root is laid out as though the viewport were its parent's content box:
/// `Fill` and `Pct(100)` make it exactly as wide as the pane, and `Auto` makes
/// it as tall as its contents — which is what leaves something for
/// [`Scene::scroll`] to move. The scroll is applied once, here, by starting the
/// root above the viewport; everything downstream is in screen coordinates and
/// nothing else has to remember that a scene scrolls.
///
/// An empty scene, a viewport of no width and a paragraph of no text all lay
/// out to nothing. None of them is an error: they are what a document looks
/// like a moment before it is finished, and a layout engine that panicked on
/// them would take the editor with it.
pub fn layout(scene: &Scene, viewport: Rect, m: &dyn Measure) -> Layout {
    let mut out = Layout::default();
    let Some(root) = scene.root() else {
        return out;
    };
    let (w, h) = measure(scene, root, m, viewport.w, viewport.h, 0);
    let rect = Rect { x: viewport.x, y: viewport.y - scene.scroll, w, h };
    place(scene, root, m, rect, None, 0, &mut out);
    out
}

/// The node under `(x, y)`, and the nearest [`Tag`] enclosing it.
///
/// Innermost wins, because paint order puts a child after its parent and this
/// walks backwards. Inside a paragraph it goes further still and resolves to
/// the *run*: a tagged word outranks the block around it, which is the whole
/// reason a run may carry a tag, and a link that reported the paragraph's tag
/// would be no link at all.
///
/// The node id comes back even when there is no tag, so a mode can tell "you
/// clicked the figure, which means nothing" from "you clicked outside the
/// scene".
pub fn hit(layout: &Layout, x: i32, y: i32) -> Option<(NodeId, Option<Tag>)> {
    for f in layout.frames.iter().rev() {
        if !f.rect.contains(x, y) {
            continue;
        }
        for l in &f.lines {
            let top = f.rect.y + l.y;
            if y < top || y >= top + l.height {
                continue;
            }
            for p in &l.pieces {
                let left = f.rect.x + p.x;
                if p.tag.is_some() && x >= left && x < left + p.width {
                    return Some((f.node, p.tag));
                }
            }
        }
        return Some((f.node, f.tag));
    }
    None
}

/// A [`Length`] in pixels, or `None` for `Auto` — which only the caller can
/// answer, because "as big as the content" means something different to a rect
/// with no children than it does to a block with several.
fn fixed(len: Length, avail: i32) -> Option<i32> {
    let avail = avail.max(0);
    match len {
        Length::Px(p) => Some(p.max(0)),
        // Integer arithmetic throughout, so three thirds of a hundred leave a
        // pixel over. A percentage is for a measure of prose, not for a seam.
        Length::Pct(p) => Some((avail as i64 * p as i64 / 100) as i32),
        Length::Fill => Some(avail),
        Length::Auto => None,
    }
}

/// The length a node declares along one axis, if it declares one at all.
///
/// A paragraph and a figure are always exactly their own size, so both answer
/// `Auto`: there is no length on them to state it with, and none they could
/// honour if there were.
fn declared(node: &Node, axis: Dir) -> Length {
    match node {
        Node::Block(b) => match axis {
            Dir::Row => b.width,
            Dir::Column => b.height,
        },
        Node::Rect { w, h, .. } => match axis {
            Dir::Row => *w,
            Dir::Column => *h,
        },
        Node::Text { .. } | Node::Image { .. } => Length::Auto,
    }
}

/// The size `id` settles on when offered a box of `avail_w` by `avail_h`.
///
/// This is the walk down. It answers `Auto` — and only `Auto` needs answering,
/// since every other case is arithmetic on the box that was offered.
fn measure(
    scene: &Scene,
    id: NodeId,
    m: &dyn Measure,
    avail_w: i32,
    avail_h: i32,
    depth: u32,
) -> (i32, i32) {
    let (Some(node), true) = (scene.node(id), depth <= MAX_DEPTH) else {
        return (0, 0);
    };
    match node {
        // The *longest line*, not the width it was offered — that is what makes
        // an `Auto` parent shrink to its prose rather than to its pane. The
        // paragraph is then handed the parent's full measure to align inside;
        // see `children`.
        Node::Text { runs, .. } => {
            let lines = wrap(runs, avail_w, m);
            let w = lines.iter().map(line_width).max().unwrap_or(0);
            (w, lines.iter().map(|l| l.height).sum())
        }
        // A figure is its own size and there is nothing to negotiate; `depth`
        // is inert at block level, having no baseline to hang from. The clamp
        // is not paranoia about a real bitmap, it is that the width arrived
        // from Lisp and `as i32` on four billion would come out negative, which
        // would then be quietly subtracted from a parent.
        Node::Image { width, height, .. } => (clamp_u32(*width), clamp_u32(*height)),
        Node::Rect { w, h, .. } => {
            (fixed(*w, avail_w).unwrap_or(0), fixed(*h, avail_h).unwrap_or(0))
        }
        Node::Block(b) => {
            // The block's own width is settled *before* the children are
            // measured, so a paragraph inside a 200-pixel column wraps at 200
            // rather than at whatever the pane happened to be — which is the
            // difference between a height that is right and one that is a
            // guess nothing corrects later.
            let own_w = fixed(b.width, avail_w);
            let own_h = fixed(b.height, avail_h);
            let inner_w = (own_w.unwrap_or(avail_w) - 2 * b.pad).max(0);
            let inner_h = (own_h.unwrap_or(avail_h) - 2 * b.pad).max(0);
            let (cw, ch) = content(scene, b, m, inner_w, inner_h, depth);
            let w = own_w.unwrap_or(cw + 2 * b.pad);
            let h = own_h.unwrap_or(ch + 2 * b.pad);
            (w.max(0), h.max(0))
        }
    }
}

/// How much room a block's children want, inside a content box of `inner_w` by
/// `inner_h`: the sum along the stacking axis plus the gaps, and the widest of
/// them across it.
///
/// A child that is `Fill` along the stacking axis wants *nothing*: it lives on
/// what is left over, so counting it here would make an `Auto` parent grow to
/// make room for a child that only ever wanted the room the parent already had.
///
/// ponytail: `Pct` is not given the same treatment, so a percentage-sized child
/// inside an `Auto` parent is measured against two different boxes — once
/// against the box the parent was offered, which is what the parent sizes
/// itself from, and once against the parent's own final width, which is what it
/// actually gets. The same slack appears when a paragraph shares an `Auto`
/// parent with a wider sibling: the parent grows to the sibling, the paragraph
/// is re-wrapped in that wider box and needs fewer lines than were reserved for
/// it. Both leave a box roomier than its contents, never contents outside their
/// box, so the failure is a gap and not an overlap. Ceiling: `Auto` beside
/// `Pct` in the same axis, which a hand-written scene has little reason to do —
/// a percentage of "as big as my contents" is not a size anyone means. The
/// upgrade path is the min-content/max-content pair CSS resolves this with,
/// which is a second entry point into this function and not a change to it.
fn content(
    scene: &Scene,
    b: &Block,
    m: &dyn Measure,
    inner_w: i32,
    inner_h: i32,
    depth: u32,
) -> (i32, i32) {
    let (mut main, mut cross) = (0, 0);
    for &c in &b.children {
        let (w, h) = measure(scene, c, m, inner_w, inner_h, depth + 1);
        let (cm, cc) = along(b.dir, w, h);
        let fills = scene.node(c).map(|n| declared(n, b.dir)) == Some(Length::Fill);
        main += if fills { 0 } else { cm };
        cross = cross.max(cc);
    }
    // Between children, never around them. A child naming a node that does not
    // exist still takes its turn — it contributes no size but keeps its gap, so
    // that this walk and the one that places them agree about which child is
    // which. A dangling id is arithmetic Lisp got wrong, not a reason to
    // renumber everything after it.
    main += b.gap * (b.children.len() as i32 - 1).max(0);
    match b.dir {
        Dir::Column => (cross, main),
        Dir::Row => (main, cross),
    }
}

/// `(main, cross)` for a block stacking in `dir`, out of a `(width, height)`.
fn along(dir: Dir, w: i32, h: i32) -> (i32, i32) {
    match dir {
        Dir::Column => (h, w),
        Dir::Row => (w, h),
    }
}

fn clamp_u32(n: u32) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// Emit a frame for `id` at `rect`, and for its children.
///
/// This is the walk back up. Everything about the node's own box is decided by
/// the time it is called — the parent worked it out — so all this does is
/// record it, inherit the tag, and share out what is left over.
fn place(
    scene: &Scene,
    id: NodeId,
    m: &dyn Measure,
    rect: Rect,
    tag: Option<Tag>,
    depth: u32,
    out: &mut Layout,
) {
    let (Some(node), true) = (scene.node(id), depth <= MAX_DEPTH) else {
        return;
    };
    match node {
        // Wrapped a second time, at the width the parent settled on rather than
        // the one it offered, and only now aligned — alignment needs the final
        // measure, which is the one thing the walk down did not have.
        Node::Text { runs, align } => {
            let mut lines = wrap(runs, rect.w, m);
            align_lines(&mut lines, rect.w, *align);
            out.frames.push(Frame { node: id, rect, tag, lines });
        }
        Node::Image { .. } | Node::Rect { .. } => {
            out.frames.push(Frame { node: id, rect, tag, lines: Vec::new() })
        }
        Node::Block(b) => {
            // The block's own tag, if it has one, becomes what everything
            // inside inherits. `or` and not `and`: an untagged block passes its
            // parent's tag through, which is what makes "the nearest enclosing
            // tag" nearest rather than merely enclosing.
            let tag = b.tag.or(tag);
            out.frames.push(Frame { node: id, rect, tag, lines: Vec::new() });
            children(scene, b, m, rect, tag, depth, out);
        }
    }
}

/// What one child asked for, kept between the two loops below.
struct Want {
    main: i32,
    cross: i32,
    fill: bool,
}

fn children(
    scene: &Scene,
    b: &Block,
    m: &dyn Measure,
    rect: Rect,
    tag: Option<Tag>,
    depth: u32,
    out: &mut Layout,
) {
    let inner = Rect {
        x: rect.x + b.pad,
        y: rect.y + b.pad,
        w: (rect.w - 2 * b.pad).max(0),
        h: (rect.h - 2 * b.pad).max(0),
    };
    let (main_avail, cross_avail) = along(b.dir, inner.w, inner.h);

    let mut wants = Vec::with_capacity(b.children.len());
    let mut fixed_main = 0;
    let mut fills = 0;
    for &c in &b.children {
        let (w, h) = measure(scene, c, m, inner.w, inner.h, depth + 1);
        let (main, mut cross) = along(b.dir, w, h);
        let node = scene.node(c);
        let fill = node.map(|n| declared(n, b.dir)) == Some(Length::Fill);
        // A paragraph down a column is a block box: it takes the whole measure
        // it was given and is ragged only on the right. That is what an `Align`
        // on the paragraph aligns *within*, and it is deliberately not what the
        // walk down reported — which is the longest line, so that an `Auto`
        // parent still shrinks to the prose. Across a row a paragraph is a
        // column of prose instead, and takes its own height.
        if b.dir == Dir::Column && matches!(node, Some(Node::Text { .. })) {
            cross = cross_avail;
        }
        if fill {
            fills += 1;
        } else {
            fixed_main += main;
        }
        wants.push(Want { main: if fill { 0 } else { main }, cross, fill });
    }

    let gaps = b.gap * (b.children.len() as i32 - 1).max(0);
    let left = (main_avail - fixed_main - gaps).max(0);
    let share = if fills > 0 { left / fills } else { 0 };
    // The pixels that do not divide evenly go to the earliest `Fill` siblings,
    // one each. Dropping them instead would leave a seam at the end of the
    // block that is one pixel wide and perfectly visible.
    let mut extra = if fills > 0 { left % fills } else { 0 };

    let mut at = 0;
    for (i, &c) in b.children.iter().enumerate() {
        let Want { mut main, cross, fill } = wants[i];
        if fill {
            main = share + i32::from(extra > 0);
            extra -= i32::from(extra > 0);
        }
        let slack = (cross_avail - cross).max(0);
        let off = match b.align {
            Align::Start => 0,
            Align::Center => slack / 2,
            Align::End => slack,
        };
        let r = match b.dir {
            Dir::Column => Rect { x: inner.x + off, y: inner.y + at, w: cross, h: main },
            Dir::Row => Rect { x: inner.x + at, y: inner.y + off, w: main, h: cross },
        };
        place(scene, c, m, r, tag, depth + 1, out);
        at += main + b.gap;
    }
}

/// Where a line's last piece ends, which is the line's width.
fn line_width(l: &Line) -> i32 {
    l.pieces.last().map_or(0, |p| p.x + p.width)
}

/// Push each line across the measure it was wrapped in.
///
/// Done after wrapping rather than during, because a line's width is not known
/// until the line is finished and the whole point is to move a *finished* line.
fn align_lines(lines: &mut [Line], width: i32, align: Align) {
    if align == Align::Start {
        return;
    }
    for l in lines.iter_mut() {
        let slack = width - line_width(l);
        if slack <= 0 {
            continue;
        }
        let dx = if align == Align::Center { slack / 2 } else { slack };
        for p in &mut l.pieces {
            p.x += dx;
        }
    }
}

/// A stretch of one run that is all whitespace, all not, or a whole image.
struct Seg {
    run: usize,
    start: usize,
    end: usize,
    space: bool,
}

/// Break the runs of one paragraph into lines no wider than `max_w`.
///
/// The rule the whole crate exists to get right: **the runs are one flow**. A
/// bold word mid-sentence does not start a new box, a break may fall inside a
/// run, and a word split across a run boundary — "hel" upright and "lo" bold —
/// is still one word that moves to the next line whole. So the runs are
/// flattened into segments first and the words found afterwards, across them.
/// An image run is one such segment, which is what makes `$x_1$` glue to the
/// comma after it instead of stranding the comma on the next line.
///
/// Greedy: each word goes on the current line if it fits and starts a new one
/// if it does not. Knuth-Plass would break a paragraph better and would need
/// the whole paragraph's badness before it could place the first line, which is
/// not a trade a document that relays out whole should be making yet.
///
/// The whitespace a break lands on is dropped, so the next line starts at the
/// word rather than one space in. Whitespace anywhere else is kept exactly as
/// written, including a newline: a paragraph is prose, and if it wanted a hard
/// break it wanted two paragraphs.
fn wrap(runs: &[Run], max_w: i32, m: &dyn Measure) -> Vec<Line> {
    let max_w = max_w.max(0);
    let segs = segments(runs);
    let mut lines = Vec::new();
    let mut cur = Line::default();
    // The descent is not a field of `Line` — it is `height - baseline` there —
    // so it rides alongside until the line is finished and the two become one.
    let mut desc = 0;
    let mut y = 0;
    let mut x = 0;
    // Whitespace waiting to hear whether the next word is joining this line. If
    // it is not, this is the space the break ate.
    let mut pending: Vec<usize> = Vec::new();
    let mut pending_w = 0;

    let mut i = 0;
    while i < segs.len() {
        if segs[i].space {
            pending_w += metrics(&segs[i], runs, m).0;
            pending.push(i);
            i += 1;
            continue;
        }
        let start = i;
        while i < segs.len() && !segs[i].space {
            i += 1;
        }
        let word = &segs[start..i];
        let word_w: i32 = word.iter().map(|s| metrics(s, runs, m).0).sum();

        if !cur.pieces.is_empty() && x + pending_w + word_w > max_w {
            flush(&mut lines, &mut cur, &mut desc, &mut y);
            x = 0;
        } else {
            for &p in &pending {
                x += emit(&mut cur, &mut desc, &segs[p], runs, x, m);
            }
        }
        pending.clear();
        pending_w = 0;

        if word_w <= max_w - x {
            for s in word {
                x += emit(&mut cur, &mut desc, s, runs, x, m);
            }
        } else {
            // A word with nowhere to fit, even alone on a line. It breaks at
            // the character that overflows rather than running off the edge,
            // because a URL that vanished into the margin is worse than an ugly
            // break. At least one character goes on every line — the check is
            // against a line that already has something on it — so this
            // terminates even when the viewport is a single pixel wide.
            //
            // ponytail: the overflowing character is found by adding up
            // advances one character at a time, which assumes advances add.
            // True of a monospace face and of the fake metric the tests use,
            // and off by a kerning pair's worth on a proportional one — at the
            // break of a word that was already being broken badly. Ceiling: a
            // break one character early in a run of kerned text. The upgrade
            // path is measuring prefixes of the segment instead, which is the
            // same loop with a quadratic `advance`.
            for s in word {
                match &runs[s.run] {
                    // A bitmap cannot be broken into characters. If it does not
                    // fit a line of its own it overflows, because the
                    // alternative is not drawing it.
                    Run::Image { .. } => {
                        let w = metrics(s, runs, m).0;
                        if !cur.pieces.is_empty() && x + w > max_w {
                            flush(&mut lines, &mut cur, &mut desc, &mut y);
                            x = 0;
                        }
                        x += emit(&mut cur, &mut desc, s, runs, x, m);
                    }
                    Run::Text { text, style, .. } => {
                        for (o, ch) in text[s.start..s.end].char_indices() {
                            let mut buf = [0u8; 4];
                            let w = m.advance(ch.encode_utf8(&mut buf), *style);
                            if !cur.pieces.is_empty() && x + w > max_w {
                                flush(&mut lines, &mut cur, &mut desc, &mut y);
                                x = 0;
                            }
                            let one = Seg {
                                run: s.run,
                                start: s.start + o,
                                end: s.start + o + ch.len_utf8(),
                                space: false,
                            };
                            x += emit(&mut cur, &mut desc, &one, runs, x, m);
                        }
                    }
                }
            }
        }
    }
    // A paragraph that is *only* whitespace has parked every segment in
    // `pending` and never met a word to flush them in front of, so it would
    // finish with no lines and therefore no height at all. That is right for a
    // trailing space after a word — it is the space a break ate — and wrong for
    // the one case where the whitespace is the whole content: a blank line in a
    // listing, which `docs/gui.org` says is a `text` node of its own, would
    // silently close up and the code block would lose its paragraphing.
    if cur.pieces.is_empty() && lines.is_empty() && !pending.is_empty() {
        for &p in &pending {
            x += emit(&mut cur, &mut desc, &segs[p], runs, x, m);
        }
    }
    if !cur.pieces.is_empty() {
        flush(&mut lines, &mut cur, &mut desc, &mut y);
    }
    lines
}

/// Every run cut into alternating stretches of whitespace and not — and an
/// image run into exactly one stretch, since it is one indivisible thing.
fn segments(runs: &[Run]) -> Vec<Seg> {
    let mut segs = Vec::new();
    for (run, r) in runs.iter().enumerate() {
        let text = match r {
            Run::Image { .. } => {
                segs.push(Seg { run, start: 0, end: 0, space: false });
                continue;
            }
            Run::Text { text, .. } => text,
        };
        let mut start = 0;
        let mut kind: Option<bool> = None;
        for (at, ch) in text.char_indices() {
            let space = ch.is_whitespace();
            match kind {
                Some(k) if k == space => {}
                Some(k) => {
                    segs.push(Seg { run, start, end: at, space: k });
                    start = at;
                    kind = Some(space);
                }
                None => {
                    start = at;
                    kind = Some(space);
                }
            }
        }
        if let Some(k) = kind {
            segs.push(Seg { run, start, end: text.len(), space: k });
        }
    }
    segs
}

/// A segment's advance, and how far it rises above the baseline and hangs below
/// it — the three numbers a line is built out of.
fn metrics(s: &Seg, runs: &[Run], m: &dyn Measure) -> (i32, i32, i32) {
    match &runs[s.run] {
        Run::Text { text, style, .. } => {
            let (h, ascent) = m.line(*style);
            (m.advance(&text[s.start..s.end], *style), ascent, (h - ascent).max(0))
        }
        // The bitmap rises `height - depth` above the baseline and hangs
        // `depth` below it. That is the whole of why `depth` exists: without it
        // an inline `$x_1$` sits on the baseline by its bottom edge and the
        // subscript floats above the text it belongs to.
        Run::Image { width, height, depth, .. } => {
            let (h, d) = (clamp_u32(*height), clamp_u32(*depth));
            (clamp_u32(*width), (h - d).max(0), d)
        }
    }
}

/// Put a segment on the current line at `x`, and answer how wide it was.
///
/// Adjacent text from the same run is merged into one piece rather than left as
/// several, so the painter makes one call per run per line and a kerning pair is
/// not split down the middle by an accident of how the words were found. An
/// image run has one segment and so never merges with anything.
fn emit(cur: &mut Line, desc: &mut i32, s: &Seg, runs: &[Run], x: i32, m: &dyn Measure) -> i32 {
    let (w, ascent, descent) = metrics(s, runs, m);
    // The line clears the tallest thing on it and sits on the lowest baseline
    // claimed, which is what puts a 200% word, an inline equation and the body
    // text around them on one line instead of three overlapping bands.
    cur.baseline = cur.baseline.max(ascent);
    *desc = (*desc).max(descent);
    match cur.pieces.last_mut() {
        Some(p) if p.run == s.run && p.end == s.start => {
            p.end = s.end;
            p.width += w;
        }
        _ => cur.pieces.push(Piece {
            run: s.run,
            start: s.start,
            end: s.end,
            x,
            width: w,
            tag: runs[s.run].tag(),
        }),
    }
    w
}

fn flush(lines: &mut Vec<Line>, cur: &mut Line, desc: &mut i32, y: &mut i32) {
    cur.height = cur.baseline + *desc;
    cur.y = *y;
    *y += cur.height;
    lines.push(std::mem::take(cur));
    *desc = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Block, Family, Length::*, Node, Run, Scene, Style};

    /// A font that does not exist, and the reason this crate has no
    /// dependencies: eight pixels a character, sixteen tall, twelve of them
    /// above the baseline, all three scaled by the run's size percentage. Every
    /// expected number below is arithmetic on those four facts.
    ///
    /// The *proportional* family is the eight, not the monospace one, and that
    /// is deliberate rather than backwards: [`Style::default`] is prose, so
    /// every test here that says nothing about a family is measured in it, and
    /// pinning the default to the number the tests were written against is what
    /// keeps this fake honest about wrapping instead of about a family. A run
    /// that asks for [`Family::Mono`] measures twelve, which is the only reason
    /// a test can tell the two apart at all — a real proportional face differs
    /// from a real monospace one by exactly this, a different width for the same
    /// string.
    struct Fake;

    /// Pixels a character in `family`. Wider for mono, so a run that asked for
    /// the coding font wraps sooner and the difference shows up as a line break
    /// rather than as a number nobody looks at.
    fn per_char(family: Family) -> i32 {
        match family {
            Family::Prose => 8,
            Family::Mono => 12,
        }
    }

    impl Measure for Fake {
        fn advance(&self, text: &str, style: Style) -> i32 {
            text.chars().count() as i32 * per_char(style.family) * style.size as i32 / 100
        }
        fn line(&self, style: Style) -> (i32, i32) {
            (16 * style.size as i32 / 100, 12 * style.size as i32 / 100)
        }
    }

    fn run(text: &str) -> Run {
        Run::Text { text: text.into(), style: Style::default(), tag: None }
    }

    fn sized(text: &str, size: u16) -> Run {
        Run::Text { text: text.into(), style: Style { size, ..Style::default() }, tag: None }
    }

    fn in_family(text: &str, family: Family) -> Run {
        Run::Text { text: text.into(), style: Style { family, ..Style::default() }, tag: None }
    }

    fn tagged(text: &str, tag: Tag) -> Run {
        Run::Text { text: text.into(), style: Style::default(), tag: Some(tag) }
    }

    fn inline(width: u32, height: u32, depth: u32) -> Run {
        Run::Image { image: 1, width, height, depth, tag: None }
    }

    fn text(runs: Vec<Run>) -> Node {
        Node::Text { runs, align: Align::Start }
    }

    fn rect(w: Length, h: Length) -> Node {
        Node::Rect { w, h, fill: None }
    }

    /// A scene of one paragraph, laid out `w` wide.
    fn paragraph(runs: Vec<Run>, w: i32) -> Layout {
        let mut s = Scene::default();
        let id = s.push(text(runs));
        s.set_root(id);
        layout(&s, Rect { x: 0, y: 0, w, h: 1000 }, &Fake)
    }

    /// One block, `kids` inside it, laid out in `viewport`.
    fn stack(block: Block, kids: Vec<Node>, viewport: Rect) -> Layout {
        let mut s = Scene::default();
        let ids: Vec<NodeId> = kids.into_iter().map(|n| s.push(n)).collect();
        let root = s.push(Node::Block(Block { children: ids, ..block }));
        s.set_root(root);
        layout(&s, viewport, &Fake)
    }

    /// What each line reads as, which is the only readable way to assert that a
    /// break landed where it should. An image run reads as one solid block,
    /// because that is what it is.
    fn text_of(runs: &[Run], lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.pieces
                    .iter()
                    .map(|p| match &runs[p.run] {
                        Run::Text { text, .. } => &text[p.start..p.end],
                        Run::Image { .. } => "\u{25ae}",
                    })
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn a_paragraph_wraps_at_the_viewport_width_and_the_break_lands_between_words() {
        let runs = vec![run("the quick brown fox")];
        let out = paragraph(runs.clone(), 100);
        let lines = &out.frames[0].lines;
        // Twelve characters fit in a hundred pixels. "the quick" is nine, and
        // "the quick brown" would be fifteen — so the break is at the space,
        // and the space itself is gone rather than opening the second line.
        assert_eq!(text_of(&runs, lines), vec!["the quick", "brown fox"]);
        assert_eq!((lines[0].y, lines[1].y), (0, 16));
        assert_eq!((lines[0].baseline, lines[0].descent()), (12, 4));
        // A paragraph reports its longest line, not the width it was offered —
        // this is what an `Auto` parent shrinks to.
        assert_eq!(out.frames[0].rect, Rect { x: 0, y: 0, w: 72, h: 32 });
    }

    #[test]
    fn a_word_longer_than_the_line_breaks_mid_word() {
        let runs = vec![run("supercalifragilistic")];
        let out = paragraph(runs.clone(), 80);
        assert_eq!(text_of(&runs, &out.frames[0].lines), vec!["supercalif", "ragilistic"]);
    }

    #[test]
    fn a_word_that_only_misses_the_remainder_moves_whole_rather_than_breaking() {
        let runs = vec![run("ab cdefgh")];
        // Six characters fit. "ab" leaves room for three more, and "cdefgh"
        // needs six — but it fits on a line of its own, so it takes one whole
        // instead of leaving "cde" behind.
        let out = paragraph(runs.clone(), 48);
        assert_eq!(text_of(&runs, &out.frames[0].lines), vec!["ab", "cdefgh"]);
    }

    #[test]
    fn a_run_boundary_inside_a_line_does_not_force_a_break() {
        let bold = Run::Text {
            text: "bold".into(),
            style: Style { bold: true, ..Style::default() },
            tag: None,
        };
        let runs = vec![run("a "), bold, run(" word")];
        let out = paragraph(runs.clone(), 400);
        let lines = &out.frames[0].lines;
        assert_eq!(lines.len(), 1, "three runs, one line");
        assert_eq!(text_of(&runs, lines), vec!["a bold word"]);
        // One piece per run, laid end to end: the painter needs to know which
        // face to set each stretch in, and nothing more.
        let at: Vec<(usize, i32, i32)> =
            lines[0].pieces.iter().map(|p| (p.run, p.x, p.width)).collect();
        assert_eq!(at, vec![(0, 0, 16), (1, 16, 32), (2, 48, 40)]);
    }

    /// A run's family reaches the metric, and reaches it per *run* rather than
    /// per paragraph.
    ///
    /// This is the whole of what this crate does with a family — it never
    /// interprets one — so the failure it guards is the plumbing quietly
    /// dropping it somewhere between the run and the `Measure` call: a
    /// paragraph of prose with an inline identifier in it would then be laid
    /// out as if the identifier were prose, and the piece after it would start
    /// where the narrower face ended and be painted over.
    #[test]
    fn a_run_is_measured_in_its_own_family_and_its_neighbour_in_theirs() {
        let runs = vec![
            in_family("aaaa", Family::Prose),
            in_family("bbbb", Family::Mono),
            in_family("cccc", Family::Prose),
        ];
        let out = paragraph(runs.clone(), 400);
        let lines = &out.frames[0].lines;
        assert_eq!(lines.len(), 1, "three runs, one flow, one line");
        // Four characters at eight, twelve and eight pixels, laid end to end —
        // and each piece starting exactly where the one before it ended is the
        // property `sidesrather` was the symptom of losing.
        let at: Vec<(i32, i32)> = lines[0].pieces.iter().map(|p| (p.x, p.width)).collect();
        assert_eq!(at, vec![(0, 32), (32, 48), (80, 32)]);
    }

    /// The default family is prose, so a run that says nothing about one is set
    /// the way a reader expects a paragraph to be — which is the reason this
    /// field exists rather than a detail of it.
    #[test]
    fn a_run_that_names_no_family_is_measured_as_prose() {
        let plain = paragraph(vec![run("aaaaaaaa")], 400);
        let prose = paragraph(vec![in_family("aaaaaaaa", Family::Prose)], 400);
        let mono = paragraph(vec![in_family("aaaaaaaa", Family::Mono)], 400);
        let width = |l: &Layout| l.frames[0].lines[0].pieces[0].width;
        assert_eq!(width(&plain), width(&prose));
        assert!(width(&mono) > width(&plain), "the two families measured the same");
    }

    /// Wrapping asks the metric, so a family changes where the lines break and
    /// not only how wide a piece is. A listing set in the coding font runs out
    /// of measure sooner than the same words as prose, and that has to be what
    /// the wrapper sees.
    #[test]
    fn a_family_moves_where_a_paragraph_breaks() {
        let words = "aaa bbb ccc";
        // Eighty pixels take all three words at eight pixels a character (11
        // characters, 88 — so the last word goes over) and only two at twelve.
        let prose = paragraph(vec![in_family(words, Family::Prose)], 88);
        let mono = paragraph(vec![in_family(words, Family::Mono)], 88);
        assert_eq!(
            text_of(&[in_family(words, Family::Prose)], &prose.frames[0].lines),
            vec!["aaa bbb ccc"]
        );
        assert_eq!(
            text_of(&[in_family(words, Family::Mono)], &mono.frames[0].lines),
            vec!["aaa bbb", "ccc"]
        );
    }

    #[test]
    fn a_word_split_across_a_run_boundary_stays_one_word() {
        // "hel" and "lo" are two runs and one word, so the whole of "hello"
        // moves to the second line rather than the second run breaking off.
        let runs = vec![run("xx he"), run("llo")];
        let out = paragraph(runs.clone(), 56);
        assert_eq!(text_of(&runs, &out.frames[0].lines), vec!["xx", "hello"]);
    }

    #[test]
    fn a_lines_height_follows_its_tallest_run() {
        let runs = vec![run("a "), sized("B", 200), run(" and then some more words")];
        let out = paragraph(runs.clone(), 200);
        let lines = &out.frames[0].lines;
        // 200% is 32 tall with a 24 ascent, so the first line clears it and the
        // second — all body text — does not pay for it.
        assert_eq!((lines[0].height, lines[0].baseline), (32, 24));
        assert_eq!((lines[1].height, lines[1].baseline), (16, 12));
        assert_eq!(lines[1].y, 32, "the second line starts below the big word");
    }

    #[test]
    fn an_image_run_wraps_like_a_word() {
        let runs = vec![run("ab "), inline(40, 20, 4), run(" cd")];
        let out = paragraph(runs.clone(), 60);
        let lines = &out.frames[0].lines;
        // Sixty pixels take "ab" and no more: the bitmap is forty wide and the
        // space before it eight, so it takes a line, and "cd" is pushed off
        // that line in turn.
        assert_eq!(text_of(&runs, lines), vec!["ab", "\u{25ae}", "cd"]);
        assert_eq!(
            lines[1].pieces,
            vec![Piece { run: 1, start: 0, end: 0, x: 0, width: 40, tag: None }]
        );
        assert_eq!(lines[2].y, 36, "below a line the bitmap made twenty tall");
    }

    #[test]
    fn a_line_clears_an_image_runs_height_and_its_depth_below_the_baseline() {
        // Twenty tall hanging ten below the baseline: it rises only ten above,
        // which the body text already clears, but it hangs six lower than any
        // descender — so the line grows downwards and not up.
        let out = paragraph(vec![run("x"), inline(12, 20, 10)], 500);
        let l = &out.frames[0].lines[0];
        assert_eq!((l.height, l.baseline, l.descent()), (22, 12, 10));

        // Thirty tall hanging two below: now it is the ascent that grows, and
        // the text's own descender still sets the bottom of the line.
        let out = paragraph(vec![run("x"), inline(12, 30, 2)], 500);
        let l = &out.frames[0].lines[0];
        assert_eq!((l.height, l.baseline, l.descent()), (32, 28, 4));
    }

    #[test]
    fn fill_siblings_split_the_leftover_space() {
        let out = stack(
            Block { width: Fill, height: Fill, ..Block::default() },
            vec![rect(Fill, Px(10)), rect(Fill, Fill), rect(Fill, Fill)],
            Rect { x: 0, y: 0, w: 200, h: 101 },
        );
        let h: Vec<(i32, i32)> = out.frames[1..].iter().map(|f| (f.rect.y, f.rect.h)).collect();
        // 101 less the fixed 10 is 91, which does not halve: the odd pixel goes
        // to the first `Fill` rather than being dropped into a visible seam.
        assert_eq!(h, vec![(0, 10), (10, 46), (56, 45)]);
        // Across the stack they take the whole content box.
        assert!(out.frames[1..].iter().all(|f| f.rect.w == 200));
    }

    #[test]
    fn a_percentage_is_of_the_parent_content_box() {
        let out = stack(
            Block { width: Px(200), pad: 20, ..Block::default() },
            vec![rect(Pct(50), Px(10))],
            Rect { x: 0, y: 0, w: 500, h: 500 },
        );
        // The content box is 200 less twice the padding, so half of it is 80 —
        // not half of 200, and not half of the pane.
        assert_eq!(out.frames[1].rect, Rect { x: 20, y: 20, w: 80, h: 10 });
    }

    #[test]
    fn an_auto_block_shrinks_to_its_content() {
        let out = stack(
            Block { pad: 5, ..Block::default() },
            vec![text(vec![run("hi")])],
            Rect { x: 0, y: 0, w: 500, h: 500 },
        );
        // Two characters and five pixels of padding on each side, and the
        // height likewise — the block does not grow to the pane it was offered.
        assert_eq!(out.frames[0].rect, Rect { x: 0, y: 0, w: 26, h: 26 });
    }

    /// The workaround that stands in for a `Grid`: a table row is a
    /// row-direction block of `Auto`-width cells, and it only holds up as long
    /// as `Auto` across a row really is "as wide as this cell's own content".
    /// The columns of two such rows then line up — until a cell wraps, which is
    /// the ceiling written down in `docs/gui.org`.
    #[test]
    fn a_row_of_auto_cells_sizes_each_to_its_own_content() {
        let out = stack(
            Block { dir: Dir::Row, gap: 2, ..Block::default() },
            vec![text(vec![run("ab")]), text(vec![run("cdef")])],
            Rect { x: 0, y: 0, w: 500, h: 500 },
        );
        assert_eq!(out.frames[0].rect, Rect { x: 0, y: 0, w: 50, h: 16 });
        assert_eq!(out.frames[1].rect, Rect { x: 0, y: 0, w: 16, h: 16 });
        assert_eq!(out.frames[2].rect, Rect { x: 18, y: 0, w: 32, h: 16 });
    }

    #[test]
    fn a_block_centres_a_child_narrower_than_itself() {
        let out = stack(
            Block { width: Px(100), align: Align::Center, ..Block::default() },
            vec![rect(Px(20), Px(10))],
            Rect { x: 0, y: 0, w: 500, h: 500 },
        );
        assert_eq!(out.frames[1].rect.x, 40);
        let out = stack(
            Block { width: Px(100), align: Align::End, ..Block::default() },
            vec![rect(Px(20), Px(10))],
            Rect { x: 0, y: 0, w: 500, h: 500 },
        );
        assert_eq!(out.frames[1].rect.x, 80);
    }

    #[test]
    fn centring_a_short_line_moves_it_into_the_middle_of_the_measure() {
        let centred = |align| {
            let mut s = Scene::default();
            let t = s.push(Node::Text { runs: vec![run("ab")], align });
            let root =
                s.push(Node::Block(Block { width: Px(100), children: vec![t], ..Block::default() }));
            s.set_root(root);
            layout(&s, Rect { x: 0, y: 0, w: 500, h: 500 }, &Fake)
        };
        // The paragraph takes the whole measure the column gave it — that is
        // what there is to be centred *in*, and a ragged frame would leave
        // alignment with nothing to say.
        let out = centred(Align::Center);
        assert_eq!(out.frames[1].rect.w, 100);
        assert_eq!(out.frames[1].lines[0].pieces[0].x, 42);
        let out = centred(Align::End);
        assert_eq!(out.frames[1].lines[0].pieces[0].x, 84);
        let out = centred(Align::Start);
        assert_eq!(out.frames[1].lines[0].pieces[0].x, 0);
    }

    /// A block, a nested block and a rect inside that: three levels, so an
    /// offset composed with another can be told from one applied twice.
    fn nested() -> (Scene, [NodeId; 4]) {
        let mut s = Scene::default();
        let dot = s.push(rect(Px(3), Px(3)));
        let inner = s.push(Node::Block(Block { pad: 2, children: vec![dot], ..Block::default() }));
        let first = s.push(rect(Px(6), Px(6)));
        let root = s.push(Node::Block(Block {
            pad: 10,
            gap: 4,
            children: vec![first, inner],
            ..Block::default()
        }));
        s.set_root(root);
        (s, [root, first, inner, dot])
    }

    #[test]
    fn padding_and_gap_both_move_children_and_nesting_composes() {
        let (s, [root, ..]) = nested();
        let out = layout(&s, Rect { x: 0, y: 0, w: 500, h: 500 }, &Fake);
        let rects: Vec<Rect> = out.frames.iter().map(|f| f.rect).collect();
        assert_eq!(
            rects,
            vec![
                // The root: 10 of padding around a 6-wide column that is
                // 6 + 4 + 7 tall.
                Rect { x: 0, y: 0, w: 27, h: 37 },
                Rect { x: 10, y: 10, w: 6, h: 6 },
                // The gap pushes the nested block down past the first child,
                // and its own padding pushes its dot in again — one offset
                // composed with the other, not either applied twice.
                Rect { x: 10, y: 20, w: 7, h: 7 },
                Rect { x: 12, y: 22, w: 3, h: 3 },
            ]
        );
        assert_eq!(out.frames[0].node, root, "paint order is parents before children");
    }

    #[test]
    fn rect_of_answers_for_a_node_nested_deep_in_the_tree() {
        let (s, [root, _, inner, dot]) = nested();
        let out = layout(&s, Rect { x: 0, y: 0, w: 500, h: 500 }, &Fake);
        assert_eq!(out.rect_of(root), Some(Rect { x: 0, y: 0, w: 27, h: 37 }));
        assert_eq!(out.rect_of(inner), Some(Rect { x: 10, y: 20, w: 7, h: 7 }));
        assert_eq!(out.rect_of(dot), Some(Rect { x: 12, y: 22, w: 3, h: 3 }));
        // A node that is not in this scene has no rect, which is what a mode
        // asking to scroll to a stale id has to be told.
        assert_eq!(out.rect_of(99), None);
    }

    /// A paragraph with a tagged word, inside a tagged block, inside an
    /// untagged one — which is every case `hit` has to tell apart.
    fn tagged_scene() -> (Scene, [NodeId; 3]) {
        let mut s = Scene::default();
        let t = s.push(text(vec![run("click "), tagged("here", 9)]));
        let inner = s.push(Node::Block(Block {
            pad: 5,
            tag: Some(7),
            children: vec![t],
            ..Block::default()
        }));
        let root = s.push(Node::Block(Block {
            pad: 10,
            width: Fill,
            height: Fill,
            children: vec![inner],
            ..Block::default()
        }));
        s.set_root(root);
        (s, [root, inner, t])
    }

    #[test]
    fn hit_testing_finds_the_innermost_node() {
        let (s, [root, inner, t]) = tagged_scene();
        let out = layout(&s, Rect { x: 0, y: 0, w: 200, h: 200 }, &Fake);
        // The paragraph is at (15, 15) and ten characters wide.
        assert_eq!(hit(&out, 20, 20).map(|h| h.0), Some(t));
        // Inside the inner block but below its one line of prose.
        assert_eq!(hit(&out, 12, 34).map(|h| h.0), Some(inner));
        // In the root's own padding, outside everything it contains.
        assert_eq!(hit(&out, 2, 2).map(|h| h.0), Some(root));
        assert_eq!(hit(&out, 300, 2), None, "outside the scene is nothing at all");
    }

    #[test]
    fn hit_testing_reports_the_nearest_enclosing_tag() {
        let (s, [root, inner, t]) = tagged_scene();
        let out = layout(&s, Rect { x: 0, y: 0, w: 200, h: 200 }, &Fake);
        // The untagged root has no tag to give.
        assert_eq!(hit(&out, 2, 2), Some((root, None)));
        // A click on the tagged block's own padding is a click on the block —
        // a figure with a caption is a box, and its margin is part of it.
        assert_eq!(hit(&out, 12, 34), Some((inner, Some(7))));
        // Everything inside it inherits the tag, including a paragraph that has
        // none of its own.
        assert_eq!(hit(&out, 20, 20), Some((t, Some(7))));
        // ...and a tagged run outranks it, which is the whole point of a link
        // inside a sentence. "click " is six characters, so 15 + 48 is where
        // the tagged word starts.
        assert_eq!(hit(&out, 65, 20), Some((t, Some(9))));
        assert_eq!(hit(&out, 62, 20), Some((t, Some(7))), "one pixel earlier is still prose");
    }

    #[test]
    fn a_scene_is_moved_whole_by_its_scroll_offset() {
        let mut s = Scene::default();
        let id = s.push(rect(Px(10), Px(10)));
        s.set_root(id);
        s.scroll = 40;
        let out = layout(&s, Rect { x: 0, y: 100, w: 200, h: 200 }, &Fake);
        assert_eq!(out.frames[0].rect, Rect { x: 0, y: 60, w: 10, h: 10 });
        // Hit testing is in the same coordinates, so a scrolled scene needs no
        // second correction anywhere else.
        assert_eq!(hit(&out, 5, 65).map(|h| h.0), Some(id));
    }

    /// The number the scroll is clamped against, so it must not move when the
    /// scroll does — otherwise the end of a document walks away as you approach
    /// it.
    #[test]
    fn content_height_is_the_same_scrolled_or_not() {
        let mut s = Scene::default();
        let a = s.push(rect(Px(10), Px(30)));
        let b = s.push(rect(Px(10), Px(50)));
        let root = s.push(Node::Block(Block {
            children: vec![a, b],
            ..Block::default()
        }));
        s.set_root(root);
        let view = Rect { x: 0, y: 0, w: 200, h: 40 };
        let unscrolled = layout(&s, view, &Fake).content_height();
        assert_eq!(unscrolled, 80);
        s.scroll = 25;
        assert_eq!(layout(&s, view, &Fake).content_height(), unscrolled);
    }

    /// The blank line between two paragraphs of a listing. `docs/gui.org` says
    /// a code block is one `text` node per line precisely because there are no
    /// hard breaks — so if a whitespace-only line has no height, every blank
    /// line in every listing closes up.
    #[test]
    fn a_paragraph_of_nothing_but_spaces_still_occupies_a_line() {
        let out = paragraph(vec![run("   ")], 400);
        assert_eq!(out.frames[0].lines.len(), 1);
        assert!(out.frames[0].lines[0].height > 0);
        assert!(out.frames[0].rect.h > 0, "and the frame has that height");
    }

    /// ...but a space that merely *trails* a word is still the space a break
    /// ate, and must not buy a second line.
    #[test]
    fn a_space_after_the_last_word_does_not_add_a_line() {
        let out = paragraph(vec![run("hi ")], 400);
        assert_eq!(out.frames[0].lines.len(), 1);
    }

    #[test]
    fn an_empty_scene_lays_out_to_nothing() {
        let out = layout(&Scene::default(), Rect { x: 0, y: 0, w: 100, h: 100 }, &Fake);
        assert!(out.frames.is_empty());
        assert_eq!(hit(&out, 0, 0), None);
        assert_eq!(out.rect_of(0), None);
    }

    #[test]
    fn a_zero_width_viewport_breaks_every_character_rather_than_hanging() {
        let runs = vec![run("abc")];
        let out = paragraph(runs.clone(), 0);
        // One character a line, because a line that already has something on it
        // is the only line a break is allowed to take a character off.
        assert_eq!(text_of(&runs, &out.frames[0].lines), vec!["a", "b", "c"]);
        // ...and a block in the same viewport divides no leftover by no
        // siblings.
        let out = stack(
            Block { width: Fill, height: Fill, ..Block::default() },
            vec![rect(Fill, Fill)],
            Rect { x: 0, y: 0, w: 0, h: 0 },
        );
        assert_eq!(out.frames[1].rect, Rect { x: 0, y: 0, w: 0, h: 0 });
    }

    /// A paragraph with *nothing in it* has no lines — which is why a deliberate
    /// gap is a `Rect` and not an empty paragraph.
    ///
    /// Whitespace is deliberately not in this list, though it was once: three
    /// spaces are content, and a `text` node of three spaces is a line of three
    /// spaces. See `a_paragraph_of_nothing_but_spaces_still_occupies_a_line` —
    /// the blank line in a listing is written as exactly that node, and having
    /// it measure zero closed up every code block that had one.
    #[test]
    fn a_paragraph_of_no_text_is_a_paragraph_of_no_lines() {
        for runs in [vec![], vec![run("")]] {
            let out = paragraph(runs, 100);
            assert!(out.frames[0].lines.is_empty());
            assert_eq!(out.frames[0].rect.h, 0);
        }
    }

    #[test]
    fn a_cycle_in_the_arena_stops_instead_of_hanging() {
        let mut s = Scene::default();
        let a = s.push(Node::Block(Block { children: vec![1], ..Block::default() }));
        let b = s.push(Node::Block(Block { children: vec![a], ..Block::default() }));
        assert_eq!(b, 1, "the two blocks name each other");
        s.set_root(a);
        let out = layout(&s, Rect { x: 0, y: 0, w: 100, h: 100 }, &Fake);
        assert!(out.frames.len() <= MAX_DEPTH as usize + 1);
    }

    #[test]
    fn a_child_naming_no_node_is_silently_nothing() {
        let out = stack(
            Block { gap: 4, ..Block::default() },
            vec![rect(Px(6), Px(6))],
            Rect { x: 0, y: 0, w: 100, h: 100 },
        );
        let mut s = Scene::default();
        let real = s.push(rect(Px(6), Px(6)));
        let root =
            s.push(Node::Block(Block { gap: 4, children: vec![real, 99], ..Block::default() }));
        s.set_root(root);
        let dangling = layout(&s, Rect { x: 0, y: 0, w: 100, h: 100 }, &Fake);
        // The missing child draws nothing but still takes its gap, so the two
        // walks agree about which child is which — and the height differs from
        // the one-child case by exactly that gap.
        assert_eq!(dangling.frames.len(), 2);
        assert_eq!(dangling.frames[0].rect.h, out.frames[0].rect.h + 4);
    }
}
