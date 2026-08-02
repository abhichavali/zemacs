//! What core knows about a scene, which is deliberately almost nothing.
//!
//! A [`Scene`] is the other thing a window can show: a tree laid out in pixels
//! rather than a sequence of lines counted in cells. `crates/gui` owns the
//! model and the layout engine, `crates/render` owns the paint, and this crate
//! owns exactly one fact — *which buffer is showing one* — because that is the
//! only part of it the editor has to remember between frames.
//!
//! # There is no scene table
//!
//! An earlier draft of `docs/gui.org` gave scenes an id and an `Editor`-side
//! table to resolve it, and both were answers to a question nobody asks: Lisp
//! never names a scene, it names a *buffer* — `(scene-set (current-buffer) ...)`
//! — and a buffer already has somewhere to put one. So the scene lives on
//! [`Buffer::scene`](crate::Buffer::scene), an id space stops existing, and
//! killing a buffer takes its scene with it for free rather than by a sweep
//! somebody has to remember to write.
//!
//! # What is here
//!
//! [`HlKind::face_id`] and [`HlKind::from_face_id`], because `crates/gui`
//! cannot depend on core and so names colours by number; [`images`], the one
//! walk that says where in a scene an [`ImageId`] can sit; and
//! [`Editor::with_image_sizes`], which is the one place a scene is *read* here
//! rather than stored — a figure whose size only the image table knows.

use zemacs_gui::{FaceId, Node, Run, Scene};

use crate::{Editor, HlKind, ImageId};

/// Every image `scene` names, as `(id, width, height)` and in arena order:
/// a figure at block level, a fragment inside a sentence, and nowhere else.
///
/// Written once and read for two opposite reasons, which is the whole argument
/// for it being a function. [`Editor::with_image_sizes`] asks in order to *fill
/// a missing size in*; [`Editor::prune_images`](crate::Editor) asks in order to
/// *keep the bitmap alive*. Two hand-written walks would be two chances for a
/// node kind added later to be visible to one and invisible to the other, and
/// the half that goes wrong there is the half that drops a texture a page is
/// still drawing — a scene naming a LaTeX preview no overlay names, pruned the
/// next time any overlay was deleted, painting an id that resolves to nothing.
///
/// Repeats are not removed: both callers want them. The size comes along
/// because the only question either caller asks about a figure after its id is
/// whether it has one.
pub(crate) fn images(scene: &Scene) -> impl Iterator<Item = (ImageId, u32, u32)> + '_ {
    (0..scene.len())
        .filter_map(move |id| scene.node(id))
        .flat_map(node_images)
}

/// [`images`] for one node — the two shapes an [`ImageId`] comes in.
fn node_images(node: &Node) -> impl Iterator<Item = (ImageId, u32, u32)> + '_ {
    let figure = match node {
        Node::Image {
            image,
            width,
            height,
            ..
        } => Some((*image, *width, *height)),
        _ => None,
    };
    let runs: &[Run] = match node {
        Node::Text { runs, .. } => runs,
        _ => &[],
    };
    figure
        .into_iter()
        .chain(runs.iter().filter_map(|r| match r {
            Run::Image {
                image,
                width,
                height,
                ..
            } => Some((*image, *width, *height)),
            _ => None,
        }))
}

impl Editor {
    /// The same scene with every image that named no size taking the size of
    /// the bitmap this editor already holds.
    ///
    /// `(image ID)` is the whole form a document wants to write: the id came
    /// back from `latex-preview`, and how big the bitmap is is a fact core
    /// has had in its image table since it rasterised it. Without this, a node
    /// that gave no size lays out to a 0×0 box and the equation is invisible —
    /// `crates/gui` cannot ask how big a texture is, by design, and the person
    /// writing the document should not have to know either.
    ///
    /// Only when *both* dimensions are zero, so a figure a document
    /// deliberately scaled keeps the number it chose. `depth` comes across with
    /// them rather than separately, because a size taken from the bitmap and a
    /// baseline taken from nowhere would drop an inline fragment onto the line
    /// instead of through it.
    ///
    /// The scan comes first and the rebuild only follows it, because [`Scene`]
    /// has no `node_mut` — a scene is built once and swapped in whole — so
    /// filling one size in means pushing every node into a fresh arena. Nearly
    /// every scene arrives with its sizes already stated, and those get their
    /// own arena back untouched.
    pub(crate) fn with_image_sizes(&self, scene: Scene) -> Scene {
        if !images(&scene).any(|(_, w, h)| w == 0 && h == 0) {
            return scene;
        }
        let mut out = Scene::default();
        out.scroll = scene.scroll;
        for id in 0..scene.len() {
            let Some(node) = scene.node(id) else {
                continue;
            };
            out.push(match node {
                Node::Image { image, width: 0, height: 0, .. } => {
                    match self.image(*image) {
                        Some(bitmap) => Node::Image {
                            image: *image,
                            width: bitmap.width,
                            height: bitmap.height,
                            depth: bitmap.depth,
                        },
                        // An id naming no bitmap is left exactly as it came, for
                        // the reason every other unknown id in this editor is:
                        // it reached Rust from arithmetic in Lisp, and a size
                        // invented for it would be a figure of the wrong shape
                        // rather than a figure that is visibly missing.
                        None => node.clone(),
                    }
                }
                Node::Text { runs, align } => Node::Text {
                    runs: runs.iter().map(|r| self.sized_run(r)).collect(),
                    align: *align,
                },
                other => other.clone(),
            });
        }
        if let Some(root) = scene.root() {
            out.set_root(root);
        }
        out
    }

    /// [`Editor::with_image_sizes`] for one run — an image *in a sentence*,
    /// which is the case the whole thing exists for: `$x_1$` mid-line is the
    /// document this is being built against.
    fn sized_run(&self, run: &Run) -> Run {
        match run {
            Run::Image { image, width: 0, height: 0, tag, .. } => match self.image(*image) {
                Some(bitmap) => Run::Image {
                    image: *image,
                    width: bitmap.width,
                    height: bitmap.height,
                    depth: bitmap.depth,
                    tag: *tag,
                },
                None => run.clone(),
            },
            other => other.clone(),
        }
    }
}

impl HlKind {
    /// The number a scene names this face by.
    ///
    /// A `match` with no wildcard arm rather than `self as u8`, and that is the
    /// whole design of this function: `HlKind` carries no discriminants and no
    /// `#[repr]`, so `as` would silently renumber every face below any variant
    /// somebody inserted in the middle — a theme would keep compiling and a
    /// scene would come out the wrong colour, on a document nobody was editing
    /// at the time. Written out, the compiler refuses the function instead and
    /// the person adding the face is the person who picks its number.
    ///
    /// The numbers are the order of [`HlKind::ALL`], which is the order
    /// `face-list` is in — so a `FaceId` is an index into the table Lisp
    /// already talks about, and not a third naming scheme.
    pub fn face_id(self) -> FaceId {
        match self {
            HlKind::Keyword => 0,
            HlKind::Function => 1,
            HlKind::Type => 2,
            HlKind::String => 3,
            HlKind::Number => 4,
            HlKind::Comment => 5,
            HlKind::Constant => 6,
            HlKind::Variable => 7,
            HlKind::Operator => 8,
            HlKind::Punctuation => 9,
            HlKind::Default => 10,
            HlKind::Modeline => 11,
            HlKind::ModelineInactive => 12,
            HlKind::ModelineText => 13,
            HlKind::Heading1 => 14,
            HlKind::Heading2 => 15,
            HlKind::Heading3 => 16,
            HlKind::Bold => 17,
            HlKind::Italic => 18,
            HlKind::Link => 19,
            HlKind::Code => 20,
            HlKind::Markup => 21,
        }
    }

    /// The face a scene's number names, or `None` for a number naming none.
    ///
    /// `None` rather than falling back to [`HlKind::Default`], for the reason
    /// [`Scene::node`] answers `None` for an id naming no node: the number
    /// reached Rust from arithmetic in Lisp and can be anything, and a colour
    /// quietly substituted is a bug you find by squinting at a screenshot.
    ///
    /// Derived from [`HlKind::face_id`] rather than written out a second time,
    /// exactly as [`HlKind::from_name`] is derived from [`HlKind::name`]: two
    /// hand-written tables are two tables to forget to keep in step, and this
    /// way the round trip cannot drift even by one face.
    pub fn from_face_id(id: FaceId) -> Option<HlKind> {
        HlKind::ALL.into_iter().find(|k| k.face_id() == id)
    }
}

// There is deliberately no `SceneArg`. This file used to carry one, a newtype
// whose whole job was to hand-write the `Clone` and `PartialEq` that
// `EditorCommand` derives and `zemacs_gui::Scene` did not — a walk of the arena
// where a `Vec` copy would do, and a second walk to compare. `Scene` derives
// both now, and the wrapper went with the reason it existed.

#[cfg(test)]
mod tests {
    use super::*;
    use zemacs_gui::{Align, Node, Run, Style};

    /// The one invariant the whole colour bridge rests on: `crates/gui` holds a
    /// number because it cannot hold an `HlKind`, so every face has to survive
    /// the trip out and back. Over *every* variant, because the failure this
    /// prevents is one face — the one somebody added last — coming back as the
    /// wrong colour or as nothing at all.
    #[test]
    fn every_face_survives_the_trip_through_a_scene_and_back() {
        for kind in HlKind::ALL {
            assert_eq!(
                HlKind::from_face_id(kind.face_id()),
                Some(kind),
                "{} did not come back",
                kind.name()
            );
        }
        // Dense from zero, which is what makes a `FaceId` an index into
        // `face-list` rather than an opaque tag. A gap here means two faces
        // were given the same number, since the round trip above already
        // passed.
        let mut ids: Vec<FaceId> = HlKind::ALL.iter().map(|k| k.face_id()).collect();
        ids.sort_unstable();
        assert_eq!(ids, (0..HlKind::ALL.len() as FaceId).collect::<Vec<_>>());
        // A number past the end of the table names no face, and says so rather
        // than picking one: it arrived from arithmetic in Lisp.
        assert_eq!(HlKind::from_face_id(HlKind::ALL.len() as FaceId), None);
        assert_eq!(HlKind::from_face_id(FaceId::MAX), None);
    }

    /// Both shapes, because the reachability walk that keeps a bitmap alive is
    /// this one: an inline fragment the walk cannot see is a texture dropped out
    /// from under a page that is still drawing it, and an inline fragment is
    /// exactly the case a walk written for figures forgets.
    #[test]
    fn the_image_walk_finds_a_figure_and_a_fragment_in_a_sentence() {
        let mut scene = Scene::default();
        scene.push(Node::Image {
            image: 11,
            width: 40,
            height: 20,
            depth: 0,
        });
        scene.push(Node::Text {
            runs: vec![
                Run::Text {
                    text: "for all ".into(),
                    style: Style::default(),
                    tag: None,
                },
                Run::Image {
                    image: 22,
                    width: 0,
                    height: 0,
                    depth: 3,
                    tag: None,
                },
            ],
            align: Align::Start,
        });
        // A block and a spacer name no image and must not invent one.
        scene.push(Node::Block(zemacs_gui::Block::default()));
        scene.push(Node::Rect {
            w: zemacs_gui::Length::Fill,
            h: zemacs_gui::Length::Px(8),
            fill: None,
        });

        assert_eq!(
            images(&scene).collect::<Vec<_>>(),
            vec![(11, 40, 20), (22, 0, 0)]
        );
    }
}
