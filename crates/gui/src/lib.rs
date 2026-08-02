//! A document laid out in pixels, for the things a grid of cells cannot say.
//!
//! Everything else the editor draws is cells, and `core::overlay` says so out
//! loud in its note on `Overlay::scale`: *the widest scale claimed by any
//! overlay touching a line sets that line's cell, and the grid stays a grid*.
//! That is the right trade for a buffer you are typing into — the cursor and
//! the renderer have to agree about where column 12 is — and it is a ceiling
//! nothing on the overlay side can lift. A heading is 1.5× only by making its
//! whole line 1.5×, a figure claims whole rows, and two columns are two columns
//! of monospace.
//!
//! A *scene* is the other thing a window can show. It is measured in pixels
//! rather than counted in cells, its text is wrapped by the font rather than by
//! arithmetic, and it is a tree rather than a sequence of lines. It is not an
//! editing surface and does not pretend to be one: a buffer showing a scene is
//! read-only, which is what `org-frozen` already enforces in core.
//!
//! # This crate cannot see a font, a window, or an editor
//!
//! It has no dependencies at all. Text is measured through [`Measure`], which
//! the renderer implements over the `Face` cache it already owns and the tests
//! implement as "every character is eight pixels wide" — so the whole engine
//! runs, and fails loudly, with nothing open. Depending on `zemacs-core` would
//! additionally be a cycle, since core is what holds a [`Scene`] — on the
//! `Buffer` showing it, so that killing the buffer takes the scene with it.
//!
//! # The shape of the thing
//!
//! A [`Scene`] is an arena of [`Node`]s and a root. A node is one of four
//! things — four, not fourteen: everything a document needs is these composed,
//! and a widget that turns out to be worth a fifth is a fifth we will have a
//! document asking for. [`layout`] turns a scene into a flat list of
//! [`Frame`]s in paint order, and [`hit`] turns a click back into a node.

mod layout;

pub use layout::{hit, layout, Frame, Layout, Line, Piece};

// There is deliberately no `SceneId`. An earlier draft had one, plus a table on
// the `Editor` to resolve it, and both answered a question nobody asks: Lisp
// never names a scene, it names a *buffer*, and a buffer already has somewhere
// to keep one.

/// A node's place in the arena of the [`Scene`] that owns it.
///
/// **Not unique across the editor**, unlike every other id here, and the one
/// place this crate departs from `docs/gui.org`. The document asks for an
/// editor-wide integer on the model of `OverlayId`, but it also says a scene is
/// *built whole and swapped in* rather than mutated node by node — so Lisp
/// never names a node at all, except in the reply to a click or in the argument
/// to a scroll-into-view, and in both of those the scene is already known
/// because a buffer names it. An editor-wide counter would therefore buy
/// nothing and cost the arena its indexing.
pub type NodeId = usize;

/// A rendered bitmap, keyed exactly as `core::overlay::ImageId` keys one: this
/// is the same id, so a LaTeX preview already in the renderer's texture cache
/// can be dropped into a scene without being rasterised twice.
pub type ImageId = u64;

/// An integer Lisp chose. Rust does nothing with it except hand it back when a
/// click lands, which is the same contract an `OverlayId` has and for the same
/// reason: *an arbitrary Lisp value never has to survive a round trip through C
/// as printed source*. `i64` because that is the width an integer argument
/// arrives at `command_for` as.
pub type Tag = i64;

/// A colour, as a number naming an entry of `face-list` — the same table
/// `HlKind` names, in the same order.
///
/// A plain integer rather than `zemacs_core::HlKind` itself, which is the one
/// deliberate looseness in this file. Taking the enum would mean depending on
/// core, and core is the crate that will *hold* scenes; the cycle costs more
/// than the type safety is worth. Core maps this back to an `HlKind` at the one
/// point it builds a scene, and a number naming no face is that crate's problem
/// to fall back from. Colours are named rather than given as RGB for the reason
/// an overlay's are: a scene follows a theme change for free.
pub type FaceId = u8;

/// A box in pixels. Half-open on both axes, as every range in this editor is:
/// the pixel at `x + w` belongs to whatever is next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    /// Whether a point is inside, `[x, x + w)` by `[y, y + h)`. A zero-width or
    /// zero-height rect contains nothing, which is what keeps an empty
    /// paragraph from swallowing clicks meant for the block behind it.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// How far a box may stretch, in the vocabulary Lisp writes it in.
///
/// Four cases, and they are enough for a centred measure with margins
/// (`Fill | Px | Fill` across a row), a two-column figure-plus-caption, and a
/// table row — while staying small enough to resolve in one walk down and one
/// back up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Length {
    /// Exactly this many pixels. Negative is treated as zero.
    Px(i32),
    /// A percentage of the parent's *content* box — inside its padding.
    Pct(u16),
    /// Share what is left over with the other `Fill` siblings, equally.
    Fill,
    /// As big as the content. The default, because most boxes are.
    #[default]
    Auto,
}

/// Which way a [`Block`] stacks its children, and — because they are the same
/// question — which axis is its main one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Dir {
    /// Down. Also names the vertical axis when one is being asked about.
    #[default]
    Column,
    /// Across. Also names the horizontal axis.
    Row,
}

/// Where the slack goes when something is narrower than the room it was given.
///
/// On a [`Block`] this moves the *children*, across the direction the block
/// stacks in — so a centred figure above its caption is a column with
/// `Center`. On a [`Node::Text`] it moves the *lines*, within the paragraph's
/// own width. Two different things aligning, and both are wanted: a document
/// that had to fake a centre with padding would have to know its own measure,
/// which is the one number a document should never have to hold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}

/// How a run of text is set.
///
/// Everything a [`Measure`] implementation needs to pick a face and nothing
/// else: layout never interprets any of it, it only passes it through and asks
/// how wide the answer came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Style {
    /// Point size as a **percentage of the body size** — `200` is twice the
    /// prose around it, `100` is the prose itself.
    ///
    /// A percentage rather than a float for the reason `Overlay::scale` is one:
    /// the whole bridge to Lisp is integers and strings, and the renderer is
    /// going to snap it to one of a handful of rasterised sizes anyway.
    pub size: u16,
    pub bold: bool,
    pub italic: bool,
    /// `None` is the pane's own foreground.
    pub face: Option<FaceId>,
}

impl Default for Style {
    /// Body text: upright, unemphasised, the pane's colour, at the body size.
    /// `size: 0` would be a legal `u16` and an illegal type size, so this is not
    /// something `#[derive(Default)]` could have got right.
    fn default() -> Self {
        Self { size: 100, bold: false, italic: false, face: None }
    }
}

/// A piece of a paragraph: a stretch of text set one way, or a bitmap sitting
/// on the line.
///
/// A run is not a box. Every run of a [`Node::Text`] is wrapped as a single
/// flow, so a bold word mid-sentence does not start a new line and a line break
/// may fall inside a run — see [`layout`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Run {
    Text {
        text: String,
        style: Style,
        /// Handed back when a click lands on *this run*, ahead of the tag of
        /// any block around it.
        ///
        /// A tag here as well as on a [`Block`] because the clickable thing
        /// inside a paragraph is a word: a link that had to be its own node
        /// would be its own box, and the first rule of a text node is that it
        /// is one flow.
        tag: Option<Tag>,
    },
    /// A bitmap **in a sentence** — `$x_1$`, an inline fragment, a badge.
    ///
    /// Not symmetry for its own sake. An inline equation works on the cell grid
    /// today because core's `Image` carries a `depth`, and a scene that could
    /// only put one in a box of its own would be a regression on the document
    /// this is being built for. So an image run wraps like a word, sits on the
    /// line's baseline, and hangs `depth` pixels below it — which is what stops
    /// a subscript floating above the text it belongs to.
    Image {
        image: ImageId,
        width: u32,
        height: u32,
        /// How far below the baseline the bitmap hangs, out of its `height`.
        /// `height - depth` is therefore how far above it rises, and both take
        /// part in setting the line's height.
        depth: u32,
        tag: Option<Tag>,
    },
}

impl Run {
    /// The tag this run carries, whichever kind it is.
    pub fn tag(&self) -> Option<Tag> {
        match self {
            Run::Text { tag, .. } | Run::Image { tag, .. } => *tag,
        }
    }
}

/// Children stacked down or across. The only container.
///
/// One `pad` for all four edges and one `gap` between children, rather than the
/// eight numbers a full box model has. Nothing in a document has yet wanted a
/// different left and right, and the day one does it is a field here, not a new
/// node kind.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Block {
    pub dir: Dir,
    /// In paint order, and in stacking order — they are the same order.
    pub children: Vec<NodeId>,
    pub width: Length,
    pub height: Length,
    /// Inside every edge, taken out of the box before the children are placed.
    pub pad: i32,
    /// Between children, never around them: `n` children have `n - 1` gaps.
    pub gap: i32,
    /// Where a child narrower than the block sits, *across* [`Block::dir`].
    /// Along `dir` there is nothing to align: the children are packed from the
    /// start and `Fill` is how you push them apart.
    pub align: Align,
    pub background: Option<FaceId>,
    /// A hairline around the edge of the block's own rect.
    ///
    /// ponytail: one colour and no width, so a border is a rule you can see and
    /// not a box model. It does not take part in layout — the children are not
    /// pushed in by it, `pad` does that. Ceiling: a two-pixel frame whose
    /// contents do not overlap it. The upgrade path is a width beside the
    /// colour, added to `pad` when the content box is computed.
    pub border: Option<FaceId>,
    /// Handed back when a click lands on this block or on anything inside it
    /// that does not carry a nearer one.
    ///
    /// On the block as well as on a [`Run`] because a figure with its caption,
    /// a problem card and a pill are all *boxes*: a tag only on runs would
    /// leave every one of them cold everywhere except on its words.
    pub tag: Option<Tag>,
}

/// One of the four things a scene is made of.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Node {
    /// Children stacked down or across, with padding, a gap, and optionally a
    /// background and a border. The only container.
    Block(Block),
    /// A paragraph: one or more [`Run`]s wrapped as a *single* flow.
    Text { runs: Vec<Run>, align: Align },
    /// A bitmap by [`ImageId`], at block level — a figure. An image *in a
    /// sentence* is not this, it is a [`Run::Image`].
    ///
    /// `crates/render` has the texture; this has the size, because layout has
    /// to know how much room to leave and cannot ask a texture it has never
    /// seen.
    Image {
        image: ImageId,
        width: u32,
        height: u32,
        /// Carried so that one `Image` from core drops into either position
        /// without being re-measured, and **inert here**: depth is a distance
        /// from a baseline, and a figure standing in a column of its own has no
        /// baseline to hang from. A block image's box is `width` by `height`.
        depth: u32,
    },
    /// A fixed gap or a rule: a box with a size and maybe a colour, no
    /// children. This is how a scene says "space here" without an empty
    /// paragraph, which would be a paragraph of no lines and therefore of no
    /// height at all.
    Rect { w: Length, h: Length, fill: Option<FaceId> },
}

/// A whole document: an arena of nodes, the one they hang from, and how far
/// down it has been scrolled.
///
/// The arena rather than `Box`ed children because a [`Frame`] has to name the
/// node it came from and a click has to name it back, and an index is the only
/// name a tree of owned boxes can give out that survives being handed to
/// another crate.
///
/// A scene is built once and swapped in whole. There is deliberately no way to
/// mutate a node after it is pushed: the alternative — change this node, now
/// this one — is the API that makes every caller hold ids it has to keep in
/// step with the document.
///
/// # Why it is `Clone`, `PartialEq` and `Hash`
///
/// None of the three is for this crate's own use, and all three are load
/// bearing one crate out.
///
/// `Clone` and `PartialEq` because a scene travels to core inside an
/// `EditorCommand`, and every command is cloned on its way from a key to
/// `apply` while half of core's test suite compares them. Core cannot write
/// these itself — the orphan rule puts the traits and the type both somewhere
/// else — so without them core carries a newtype and hand-writes both, which is
/// two walks of the arena spelled out where a `Vec` copy would do.
///
/// `Hash` because the renderer keys its layout cache on the *content* of the
/// scene rather than on a generation somebody has to remember to bump. That is
/// the same argument the frame digest makes: a fingerprint of what is actually
/// there cannot go out of step with a field added later, and a stale layout is
/// a document painted at the wrong size with nothing anywhere saying so.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Scene {
    nodes: Vec<Node>,
    root: Option<NodeId>,
    /// How far the scene has scrolled, in **pixels**, positive being down.
    ///
    /// ponytail: one offset for the whole scene, so a scene containing two
    /// independently scrolling panes is not expressible. Ceiling reached the
    /// day a document wants a fixed sidebar; the upgrade path is moving the
    /// offset onto [`Block`], where it would be a sixth field and not a fifth
    /// node kind.
    pub scroll: i32,
}

impl Scene {
    /// Add a node and answer its id.
    ///
    /// Children are pushed before the parent that names them, which is the only
    /// discipline the arena asks for and the only one a builder walking a Lisp
    /// form bottom-up can accidentally keep.
    pub fn push(&mut self, node: Node) -> NodeId {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    /// Name the node everything hangs from. A scene without one lays out to
    /// nothing rather than to a panic — it is what a scene under construction
    /// looks like.
    pub fn set_root(&mut self, root: NodeId) {
        self.root = Some(root);
    }

    pub fn root(&self) -> Option<NodeId> {
        self.root
    }

    /// An id naming no node is `None` rather than a panic, everywhere: ids
    /// reach this crate from arithmetic in Lisp, and the same reasoning that
    /// makes `Overlays::edit` silent about an unknown handle applies here.
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// The two questions layout asks a font, and the only two.
///
/// `crates/render` implements this over the faces it already opens, so a scene
/// adds no second font stack. The test suite implements it as eight pixels a
/// character, which is what makes every wrapping test in this crate readable as
/// arithmetic rather than as a screenshot.
///
/// An image run is not asked about: it carries its own width, height and depth,
/// which is why this trait did not grow a third method when inline images
/// arrived.
pub trait Measure {
    /// Advance width of `text` set in `style`, in pixels.
    fn advance(&self, text: &str, style: Style) -> i32;
    /// Line height and ascent for `style`, in pixels — in that order.
    fn line(&self, style: Style) -> (i32, i32);
}
