//! A [`Scene`] read back out of printed Lisp.
//!
//! `docs/gui.org` settles the split the same way `docs/boundary.org` settles
//! every other one: Lisp says *what nodes exist and what is in them*, Rust lays
//! them out. So the tree has to cross the C boundary, and it crosses it the way
//! everything else in this editor does — as **printed source**. One C signature
//! carries every shape, and an arbitrary Lisp value never has to survive a round
//! trip as a handle the other side keeps asking questions about.
//!
//! This is the exact inverse of [`zemacs_rpc::lisp`], which prints Rust values
//! *as* Lisp source. The two have to agree about escaping or a string with a
//! quote in it means something different at each end, so the string reader here
//! is written against that module's writer: it escapes only `\` and `"`, and
//! this undoes exactly that.
//!
//! # What it will not do
//!
//! It is a reader, not an evaluator. `docs/gui.org` writes its example as
//!
//! ```text
//! (image (org-latex-image "\\dim V = \\operatorname{rank} T + \\dim\\ker T"))
//! ```
//!
//! which is the form a *config* writes; what reaches this parser is what that
//! form prints as once the image is rendered and `org-latex-image` has answered
//! an id — `(image 7314)`. Every id, every length and every face arrives here
//! already reduced to an integer. A call left unevaluated is an error naming the
//! head it did not recognise, which is the most useful thing it can be.
//!
//! # Nothing here panics
//!
//! The input is whatever a user's config printed, which makes it hostile by
//! default rather than by intent: an unbalanced paren, a keyword with nothing
//! after it, a number too big for the field it is going into, ten thousand nested
//! parens. Every one of those is an `Err` naming what was expected and the byte
//! it was expected at. The recursion is bounded in the reader — see
//! [`MAX_DEPTH`] — and the builder cannot then recurse deeper than the tree the
//! reader agreed to build.

use zemacs_gui::{
    Align, Block, Dir, FaceId, Family, ImageId, Length, Node, NodeId, Run, Scene, Style, Tag,
};

/// How deep a form may nest before the reader refuses it.
///
/// A config can print anything, and a few thousand `(` before anything else is
/// a stack overflow — which is a crash of the whole editor, not a failed
/// command, because a blown stack is not catchable. So the reader counts.
///
/// The builder needs no counter of its own: it walks the tree the reader
/// produced, so its recursion is bounded by this transitively. The deepest thing
/// `docs/gui.org` describes is a block holding a text holding a run, which is
/// three.
const MAX_DEPTH: usize = 64;

/// Parse one printed Lisp form into a [`Scene`].
///
/// The form is a single node — `(block ...)`, `(text ...)`, `(image ...)` or
/// `(rect ...)` — and becomes the scene's root. Source after that one form is an
/// error rather than being ignored, because the usual way to produce it is a
/// missing `(` at the front, and silently parsing the first half of a document
/// is worse than refusing all of it.
pub fn parse(src: &str) -> Result<Scene, String> {
    let mut reader = Reader { src, at: 0 };
    let form = reader.form(0)?;
    reader.skip_space();
    if reader.at < src.len() {
        return Err(err(reader.at, "expected end of input after the scene form"));
    }
    let mut scene = Scene::default();
    let root = node(&form, &mut scene)?;
    scene.set_root(root);
    Ok(scene)
}

// ---------------------------------------------------------------------------
// The reader: source to forms.
// ---------------------------------------------------------------------------

/// One form, and where it started, so an error can point at it.
struct Form {
    /// Byte offset into the source. Bytes rather than line and column because
    /// the source is one printed line more often than not, and a column into a
    /// 4000-character line is no more use than a byte.
    at: usize,
    kind: Kind,
}

enum Kind {
    Int(i64),
    Str(String),
    /// A symbol, lowercased and stripped of any package prefix — see
    /// [`Reader::atom`] for why both.
    Sym(String),
    /// A keyword, without its leading colon.
    Key(String),
    List(Vec<Form>),
}

impl Form {
    /// How this form is described in an error. The article is included because
    /// every message that uses it wants one.
    fn describe(&self) -> &'static str {
        match self.kind {
            Kind::Int(_) => "an integer",
            Kind::Str(_) => "a string",
            Kind::Sym(_) => "a symbol",
            Kind::Key(_) => "a keyword",
            Kind::List(_) => "a list",
        }
    }
}

fn err(at: usize, what: &str) -> String {
    format!("{what}, at byte {at}")
}

struct Reader<'a> {
    src: &'a str,
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<char> {
        self.src[self.at..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        // Advancing by the character's own width is what keeps `self.at` on a
        // char boundary, so every later `self.src[self.at..]` is a slice and not
        // a panic on a multi-byte character.
        self.at += c.len_utf8();
        Some(c)
    }

    /// Whitespace and `;` comments. A printer never emits a comment, but a
    /// config that builds its scene form by hand in a source file may well have
    /// one in it, and refusing that would be refusing valid Lisp.
    fn skip_space(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some(';') => {
                    while let Some(c) = self.bump() {
                        if c == '\n' {
                            break;
                        }
                    }
                }
                _ => return,
            }
        }
    }

    fn form(&mut self, depth: usize) -> Result<Form, String> {
        self.skip_space();
        let at = self.at;
        match self.peek() {
            None => Err(err(at, "expected a form, found end of input")),
            Some('(') => self.list(depth),
            Some(')') => Err(err(at, "unbalanced parentheses: a `)` closing nothing")),
            Some('"') => self.string(),
            _ => Ok(self.atom()),
        }
    }

    fn list(&mut self, depth: usize) -> Result<Form, String> {
        let at = self.at;
        if depth >= MAX_DEPTH {
            return Err(err(at, &format!("nested deeper than {MAX_DEPTH} forms")));
        }
        self.bump();
        let mut items = Vec::new();
        loop {
            self.skip_space();
            match self.peek() {
                Some(')') => {
                    self.bump();
                    return Ok(Form { at, kind: Kind::List(items) });
                }
                None => return Err(err(at, "unbalanced parentheses: a `(` never closed")),
                _ => items.push(self.form(depth + 1)?),
            }
        }
    }

    /// A string literal, undoing exactly what [`zemacs_rpc::lisp::string`]
    /// does: a backslash makes the next character stand for itself, and nothing
    /// else is special. That is Common Lisp's single-escape rule as well, so a
    /// literal a human typed reads the same as one the printer produced.
    fn string(&mut self) -> Result<Form, String> {
        let at = self.at;
        self.bump();
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(err(at, "unterminated string")),
                Some('"') => return Ok(Form { at, kind: Kind::Str(out) }),
                Some('\\') => match self.bump() {
                    None => return Err(err(at, "unterminated string")),
                    Some(c) => out.push(c),
                },
                Some(c) => out.push(c),
            }
        }
    }

    /// A number or a symbol. Always consumes at least one character, which is
    /// what stops [`Reader::list`] spinning forever on something it cannot read.
    fn atom(&mut self) -> Form {
        let at = self.at;
        while let Some(c) = self.peek() {
            if c.is_whitespace() || matches!(c, '(' | ')' | '"' | ';') {
                break;
            }
            self.bump();
        }
        let token = &self.src[at..self.at];
        if let Ok(n) = token.parse::<i64>() {
            return Form { at, kind: Kind::Int(n) };
        }
        // Lowercased because the source usually comes out of `prin1`, which
        // prints `(BLOCK :PAD 48 FILL)` — the reader upcased those symbols when
        // it interned them and the printer hands them back that way. Matching
        // case-sensitively would mean this parser accepted only forms a human
        // typed and rejected every one the image printed.
        let token = token.to_ascii_lowercase();
        if let Some(name) = token.strip_prefix(':') {
            return Form { at, kind: Kind::Key(name.trim_start_matches(':').to_string()) };
        }
        // ...and stripped of a package prefix for the same reason: a symbol
        // prints as `ZEMACS::BLOCK` whenever `*package*` is not the one it lives
        // in, and which package the config happened to be in is not something a
        // scene should depend on.
        let name = match token.rfind(':') {
            Some(i) => token[i + 1..].to_string(),
            None => token,
        };
        Form { at, kind: Kind::Sym(name) }
    }
}

// ---------------------------------------------------------------------------
// The builder: forms to nodes.
// ---------------------------------------------------------------------------

/// Walk an argument list, handing every `:keyword value` pair to `keyword` and
/// everything else to `positional`.
///
/// Written once rather than per node kind because "keywords in any order, all
/// optional" is the same loop four times, and because doing it as a scan rather
/// than as a prefix is what lets a keyword sit *after* a child — which a config
/// building its arguments with `append` will produce sooner or later.
///
/// The lifetime is named rather than elided so that a caller may *keep* the
/// forms it is handed — collecting children now and building them once the
/// keywords are all in is what gets the children into the arena in source order.
fn args<'f>(
    head: &str,
    rest: &'f [Form],
    mut keyword: impl FnMut(&str, &'f Form) -> Result<(), String>,
    mut positional: impl FnMut(&'f Form) -> Result<(), String>,
) -> Result<(), String> {
    let mut i = 0;
    while i < rest.len() {
        if let Kind::Key(name) = &rest[i].kind {
            let value = rest.get(i + 1).ok_or_else(|| {
                err(rest[i].at, &format!("`:{name}` in ({head} ...) has no value after it"))
            })?;
            keyword(name, value)?;
            i += 2;
        } else {
            positional(&rest[i])?;
            i += 1;
        }
    }
    Ok(())
}

/// The head symbol of a form, and everything after it.
fn head<'a>(form: &Form, items: &'a [Form]) -> Result<(&'a str, &'a [Form]), String> {
    match items.first() {
        Some(Form { kind: Kind::Sym(name), .. }) => Ok((name.as_str(), &items[1..])),
        Some(other) => Err(err(
            other.at,
            &format!("expected a symbol naming the form, found {}", other.describe()),
        )),
        None => Err(err(form.at, "expected a form, found `()`")),
    }
}

fn node(form: &Form, scene: &mut Scene) -> Result<NodeId, String> {
    let items = match &form.kind {
        Kind::List(items) => items,
        _ => {
            return Err(err(
                form.at,
                &format!(
                    "expected a node — (block ...), (text ...), (image ...) \
                     or (rect ...) — found {}",
                    form.describe()
                ),
            ))
        }
    };
    let (head, rest) = head(form, items)?;
    match head {
        "block" => block(rest, scene),
        "text" => text(rest, scene),
        "image" => image(form, rest, scene),
        "rect" => rect(rest, scene),
        // Named separately from the unknown-head case because it is the mistake
        // that will actually be made: a run is not a node, it is a piece of a
        // paragraph's single flow, and a run promoted to a node would be a box
        // of its own — which is the one thing `Node::Text` exists to prevent.
        "run" | "image-run" => Err(err(
            form.at,
            &format!(
                "`{head}` is a run and belongs inside (text ...), \
                 not where a node was expected"
            ),
        )),
        other => Err(err(
            form.at,
            &format!("unknown node `{other}`; expected block, text, image or rect"),
        )),
    }
}

fn block(rest: &[Form], scene: &mut Scene) -> Result<NodeId, String> {
    let mut b = Block::default();
    let mut kids: Vec<&Form> = Vec::new();
    args(
        "block",
        rest,
        |key, value| {
            match key {
                "pad" => b.pad = int32(value, ":pad")?,
                "gap" => b.gap = int32(value, ":gap")?,
                "dir" => b.dir = dir(value)?,
                "width" => b.width = length(value, ":width")?,
                "height" => b.height = length(value, ":height")?,
                "align" => b.align = align(value)?,
                "background" => b.background = face(value, ":background")?,
                "border" => b.border = face(value, ":border")?,
                "tag" => b.tag = tag(value)?,
                other => {
                    return Err(err(value.at, &format!("unknown keyword `:{other}` in (block ...)")))
                }
            }
            Ok(())
        },
        |child| {
            kids.push(child);
            Ok(())
        },
    )?;
    // Built after the scan rather than during it so the children are pushed in
    // source order, which is paint order and stacking order — `Block::children`
    // is documented as all three at once.
    for kid in kids {
        let id = node(kid, scene)?;
        b.children.push(id);
    }
    Ok(scene.push(Node::Block(b)))
}

fn text(rest: &[Form], scene: &mut Scene) -> Result<NodeId, String> {
    let mut align_to = Align::default();
    let mut pieces: Vec<&Form> = Vec::new();
    args(
        "text",
        rest,
        |key, value| match key {
            "align" => {
                align_to = align(value)?;
                Ok(())
            }
            other => Err(err(value.at, &format!("unknown keyword `:{other}` in (text ...)"))),
        },
        |piece| {
            pieces.push(piece);
            Ok(())
        },
    )?;
    let mut runs = Vec::with_capacity(pieces.len());
    for piece in pieces {
        runs.push(run(piece)?);
    }
    // A paragraph with no runs is allowed, and lays out to no lines and so to no
    // height. That is deliberate: a mode building a paragraph from a list that
    // came out empty should get an empty paragraph, not a parse error a hundred
    // nodes from the thing that was actually missing.
    Ok(scene.push(Node::Text { runs, align: align_to }))
}

fn run(form: &Form) -> Result<Run, String> {
    let items = match &form.kind {
        Kind::List(items) => items,
        _ => {
            return Err(err(
                form.at,
                &format!(
                    "expected a run — (run \"...\") or (image-run ID) — found {}",
                    form.describe()
                ),
            ))
        }
    };
    let (head, rest) = head(form, items)?;
    match head {
        "run" => text_run(form, rest),
        "image-run" => image_run(form, rest),
        other => Err(err(
            form.at,
            &format!("unknown run `{other}`; expected run or image-run"),
        )),
    }
}

fn text_run(form: &Form, rest: &[Form]) -> Result<Run, String> {
    let mut style = Style::default();
    let mut carried = None;
    let mut body: Option<&str> = None;
    args(
        "run",
        rest,
        |key, value| {
            match key {
                // Zero is a legal `u16` and an illegal type size, which is the
                // reason `Style::default` is hand-written rather than derived.
                "size" => style.size = ranged(value, ":size", 1, u16::MAX as i64)? as u16,
                "bold" => style.bold = boolean(value, ":bold")?,
                "italic" => style.italic = boolean(value, ":italic")?,
                // Unstated is prose, from `Style::default` — a scene is a page,
                // and the run that has to say something is the listing.
                "family" => style.family = family(value)?,
                "face" => style.face = face(value, ":face")?,
                "tag" => carried = tag(value)?,
                other => {
                    return Err(err(value.at, &format!("unknown keyword `:{other}` in (run ...)")))
                }
            }
            Ok(())
        },
        |piece| match &piece.kind {
            Kind::Str(s) if body.is_none() => {
                body = Some(s);
                Ok(())
            }
            Kind::Str(_) => Err(err(
                piece.at,
                "(run ...) sets one string; a second run is how you set a second",
            )),
            _ => Err(err(
                piece.at,
                &format!("expected the string of a (run ...), found {}", piece.describe()),
            )),
        },
    )?;
    let text = body.ok_or_else(|| err(form.at, "(run ...) needs a string to set"))?;
    Ok(Run::Text { text: text.to_string(), style, tag: carried })
}

fn image_run(form: &Form, rest: &[Form]) -> Result<Run, String> {
    let (image, width, height, depth, carried) = image_args("image-run", form, rest, true)?;
    Ok(Run::Image { image, width, height, depth, tag: carried })
}

fn image(form: &Form, rest: &[Form], scene: &mut Scene) -> Result<NodeId, String> {
    let (image, width, height, depth, _) = image_args("image", form, rest, false)?;
    Ok(scene.push(Node::Image { image, width, height, depth }))
}

/// The four numbers both image forms share, plus the tag only a run may carry.
///
/// `depth` is read for a block image too, and kept, even though layout treats a
/// figure's depth as inert: one `Image` from core drops into either position
/// without being re-measured, and a parser that dropped the field on the way in
/// would make the two positions disagree about the same image.
fn image_args(
    head: &str,
    form: &Form,
    rest: &[Form],
    tagged: bool,
) -> Result<(ImageId, u32, u32, u32, Option<Tag>), String> {
    let mut id: Option<ImageId> = None;
    let mut width = 0;
    let mut height = 0;
    let mut depth = 0;
    let mut carried = None;
    args(
        head,
        rest,
        |key, value| {
            match key {
                "width" => width = ranged(value, ":width", 0, u32::MAX as i64)? as u32,
                "height" => height = ranged(value, ":height", 0, u32::MAX as i64)? as u32,
                "depth" => depth = ranged(value, ":depth", 0, u32::MAX as i64)? as u32,
                "tag" if tagged => carried = tag(value)?,
                other => {
                    return Err(err(
                        value.at,
                        &format!("unknown keyword `:{other}` in ({head} ...)"),
                    ))
                }
            }
            Ok(())
        },
        |piece| match &piece.kind {
            Kind::Int(n) if id.is_none() => {
                if *n < 0 {
                    return Err(err(piece.at, "an image id is not negative"));
                }
                id = Some(*n as ImageId);
                Ok(())
            }
            Kind::Int(_) => Err(err(piece.at, &format!("({head} ...) names one image"))),
            // The likeliest thing to land here is `(image (org-latex-image
            // "..."))` straight out of `docs/gui.org` — a form nobody evaluated.
            _ => Err(err(
                piece.at,
                &format!(
                    "expected an image id, an integer, found {} — \
                     an unevaluated call cannot be read as one",
                    piece.describe()
                ),
            )),
        },
    )?;
    let id = id.ok_or_else(|| err(form.at, &format!("({head} ...) needs an image id")))?;
    // ponytail: a missing `:width` is zero rather than an error, because the
    // brief is that every keyword is optional. Ceiling: an image with no size
    // lays out to nothing at all and looks like a missing texture rather than
    // like a missing argument. The upgrade path, if that ever costs anyone an
    // afternoon, is making the two size keywords required here — which is one
    // `ok_or_else` each and no change anywhere else.
    Ok((id, width, height, depth, carried))
}

fn rect(rest: &[Form], scene: &mut Scene) -> Result<NodeId, String> {
    let mut w = Length::default();
    let mut h = Length::default();
    let mut fill = None;
    args(
        "rect",
        rest,
        |key, value| {
            match key {
                "width" => w = length(value, ":width")?,
                "height" => h = length(value, ":height")?,
                "fill" => fill = face(value, ":fill")?,
                other => {
                    return Err(err(value.at, &format!("unknown keyword `:{other}` in (rect ...)")))
                }
            }
            Ok(())
        },
        |piece| {
            Err(err(
                piece.at,
                &format!("(rect ...) takes no children, found {}", piece.describe()),
            ))
        },
    )?;
    Ok(scene.push(Node::Rect { w, h, fill }))
}

// ---------------------------------------------------------------------------
// Values.
// ---------------------------------------------------------------------------

/// An integer that fits the field it is going into.
///
/// The range is checked rather than the value truncated: a `:face 300` that
/// silently became face 44 would paint the wrong colour and never say why, and
/// the arithmetic that produced it happened in Lisp where nothing bounds a
/// fixnum.
fn int32(form: &Form, what: &str) -> Result<i32, String> {
    Ok(ranged(form, what, i32::MIN as i64, i32::MAX as i64)? as i32)
}

fn ranged(form: &Form, what: &str, low: i64, high: i64) -> Result<i64, String> {
    match form.kind {
        Kind::Int(n) if n >= low && n <= high => Ok(n),
        Kind::Int(n) => Err(err(form.at, &format!("{what} is {n}, outside {low}..={high}"))),
        _ => Err(err(
            form.at,
            &format!("expected an integer for {what}, found {}", form.describe()),
        )),
    }
}

/// `t` or `nil`, and nothing else.
///
/// Not Common Lisp's generalised boolean, where every non-`NIL` value is true:
/// a printed scene only ever carries `T` or `NIL` in these places, and reading
/// `:bold 0` as *bold* — which generalised truth would — is the sort of quiet
/// wrongness this parser exists to turn into a message.
fn boolean(form: &Form, what: &str) -> Result<bool, String> {
    match &form.kind {
        Kind::Sym(s) if s == "t" => Ok(true),
        Kind::Sym(s) if s == "nil" => Ok(false),
        _ => Err(err(form.at, &format!("expected t or nil for {what}"))),
    }
}

/// A face number, or `nil` for "the pane's own colour".
///
/// `nil` is accepted as well as an integer because `:face (when hot 3)` prints
/// as `:FACE NIL`, and a conditional face is the ordinary way to write one.
fn face(form: &Form, what: &str) -> Result<Option<FaceId>, String> {
    if let Kind::Sym(s) = &form.kind {
        if s == "nil" {
            return Ok(None);
        }
    }
    Ok(Some(ranged(form, what, 0, FaceId::MAX as i64)? as FaceId))
}

/// A tag, or `nil` for none — same reasoning as [`face`].
fn tag(form: &Form) -> Result<Option<Tag>, String> {
    match &form.kind {
        Kind::Sym(s) if s == "nil" => Ok(None),
        Kind::Int(n) => Ok(Some(*n)),
        _ => Err(err(
            form.at,
            &format!("expected an integer for :tag, found {}", form.describe()),
        )),
    }
}

fn dir(form: &Form) -> Result<Dir, String> {
    match &form.kind {
        Kind::Sym(s) if s == "row" => Ok(Dir::Row),
        Kind::Sym(s) if s == "column" => Ok(Dir::Column),
        _ => Err(err(form.at, "expected row or column for :dir")),
    }
}

/// Which of the two typefaces a run is set in — `mono` or `prose`.
///
/// Two words and not a font name, for the reason [`Family`] gives: the renderer
/// caches an open face per size, weight, slant and family, and a name would make
/// the last of those a string somebody typed. A closed pair keeps the cache
/// bounded and keeps "no such font" a question answered once at startup rather
/// than once per document.
///
/// A run that says nothing is prose, because a scene is a page. The word a
/// document has to write is `mono`, on the runs of a source listing — where the
/// columns line up only if every character is the same width.
fn family(form: &Form) -> Result<Family, String> {
    match &form.kind {
        Kind::Sym(s) if s == "mono" => Ok(Family::Mono),
        Kind::Sym(s) if s == "prose" => Ok(Family::Prose),
        _ => Err(err(form.at, "expected mono or prose for :family")),
    }
}

fn align(form: &Form) -> Result<Align, String> {
    match &form.kind {
        Kind::Sym(s) if s == "start" => Ok(Align::Start),
        Kind::Sym(s) if s == "center" => Ok(Align::Center),
        Kind::Sym(s) if s == "end" => Ok(Align::End),
        _ => Err(err(form.at, "expected start, center or end for :align")),
    }
}

/// A length in the four spellings `docs/gui.org` gives it: a bare integer is
/// pixels, `(pct N)` is a percentage, and `fill` and `auto` are themselves.
///
/// Pixels are the bare case because they are the common one and a scene full of
/// `(px 48)` would be noise. Negative pixels are allowed through rather than
/// rejected — `Length::Px` documents them as zero — since the arithmetic that
/// produced one is Lisp's and clamping it is the layout engine's job, in one
/// place, rather than this parser's in another.
fn length(form: &Form, what: &str) -> Result<Length, String> {
    match &form.kind {
        Kind::Int(_) => Ok(Length::Px(int32(form, what)?)),
        Kind::Sym(s) if s == "fill" => Ok(Length::Fill),
        Kind::Sym(s) if s == "auto" => Ok(Length::Auto),
        Kind::List(items) => {
            let (head, rest) = head(form, items)?;
            if head != "pct" {
                let m = format!("expected (pct N) for {what}, found ({head} ...)");
                return Err(err(form.at, &m));
            }
            match rest {
                [n] => Ok(Length::Pct(ranged(n, what, 0, u16::MAX as i64)? as u16)),
                _ => Err(err(form.at, &format!("(pct N) takes one number, for {what}"))),
            }
        }
        _ => Err(err(
            form.at,
            &format!(
                "expected a length — N, (pct N), fill or auto — for {what}, found {}",
                form.describe()
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The root node of a scene that parsed, or the parse error, loudly.
    fn root(src: &str) -> (Scene, NodeId) {
        let scene = parse(src).unwrap_or_else(|e| panic!("{src}\n  did not parse: {e}"));
        let id = scene.root().expect("a scene that parsed has a root");
        (scene, id)
    }

    fn block_of(scene: &Scene, id: NodeId) -> &Block {
        match scene.node(id) {
            Some(Node::Block(b)) => b,
            other => panic!("expected a block, found {other:?}"),
        }
    }

    fn runs_of(scene: &Scene, id: NodeId) -> &[Run] {
        match scene.node(id) {
            Some(Node::Text { runs, .. }) => runs,
            other => panic!("expected a text node, found {other:?}"),
        }
    }

    /// The example in `docs/gui.org`, with the one change reality forces: the
    /// document writes `(image (org-latex-image "\\dim V = ..."))`, which is the
    /// *unevaluated* form a config holds. What is printed and reaches this
    /// parser is the id that call answered, so the id is what the test uses.
    /// The rest is the document's source character for character, including the
    /// arrow, which is what makes this the non-ASCII case as well.
    #[test]
    fn the_example_in_docs_gui_org_parses_to_the_tree_it_describes() {
        let (scene, id) = root(
            r#"(block :pad 48 :gap 12 :width (pct 100)
                 (text (run "The Rank-Nullity Theorem" :size 200 :bold t))
                 (text (run "For a linear map ") (run "T : V → W" :italic t)
                       (run ", the dimensions satisfy:"))
                 (image 7314 :width 320 :height 24))"#,
        );

        let b = block_of(&scene, id);
        assert_eq!(b.pad, 48);
        assert_eq!(b.gap, 12);
        assert_eq!(b.width, Length::Pct(100));
        assert_eq!(b.height, Length::Auto, "an unstated length is Auto");
        assert_eq!(b.dir, Dir::Column, "a block stacks down unless told otherwise");
        assert_eq!(b.children.len(), 3);

        let heading = runs_of(&scene, b.children[0]);
        assert_eq!(
            heading,
            [Run::Text {
                text: "The Rank-Nullity Theorem".into(),
                style: Style { size: 200, bold: true, ..Style::default() },
                tag: None,
            }]
        );

        // Three runs, in source order, one flow — the middle one italic and the
        // ones either side of it not, which is the whole reason a paragraph is
        // runs rather than nodes.
        let sentence = runs_of(&scene, b.children[1]);
        assert_eq!(sentence.len(), 3);
        assert_eq!(
            sentence.iter().map(|r| match r {
                Run::Text { text, .. } => text.as_str(),
                Run::Image { .. } => "<image>",
            }).collect::<Vec<_>>(),
            ["For a linear map ", "T : V → W", ", the dimensions satisfy:"]
        );
        assert_eq!(
            sentence.iter().map(|r| match r {
                Run::Text { style, .. } => style.italic,
                Run::Image { .. } => false,
            }).collect::<Vec<_>>(),
            [false, true, false]
        );

        assert_eq!(
            scene.node(b.children[2]),
            Some(&Node::Image { image: 7314, width: 320, height: 24, depth: 0 })
        );
    }

    #[test]
    fn blocks_nest_and_children_come_out_in_source_order() {
        let (scene, id) = root(
            r#"(block :dir row
                 (block :tag 1)
                 (block :tag 2 (block :tag 3) (block :tag 4))
                 (block :tag 5))"#,
        );
        let outer = block_of(&scene, id);
        assert_eq!(outer.dir, Dir::Row);
        let tags: Vec<_> = outer.children.iter().map(|c| block_of(&scene, *c).tag).collect();
        assert_eq!(tags, [Some(1), Some(2), Some(5)]);

        let middle = block_of(&scene, outer.children[1]);
        let inner: Vec<_> = middle.children.iter().map(|c| block_of(&scene, *c).tag).collect();
        assert_eq!(inner, [Some(3), Some(4)]);

        // Children are pushed before the parent that names them, so the root is
        // the last node in the arena. `Scene` documents that discipline and a
        // builder walking a form bottom-up is what keeps it.
        assert_eq!(scene.root(), Some(scene.len() - 1));
    }

    #[test]
    fn every_keyword_is_optional_and_order_does_not_matter() {
        let (bare, id) = root("(block)");
        assert_eq!(block_of(&bare, id), &Block::default());

        let one = r#"(block :pad 1 :gap 2 :dir row :width 3 :height 4
                            :align center :background 5 :border 6 :tag 7)"#;
        // Reversed, and with a child in the middle of the keywords rather than
        // after them: an argument list built with `append` will produce that.
        let other = r#"(block :tag 7 :border 6 (rect) :background 5 :align center
                              :height 4 :width 3 :dir row :gap 2 :pad 1)"#;
        let (a, ai) = root(one);
        let (b, bi) = root(other);
        let a = block_of(&a, ai).clone();
        let mut b = block_of(&b, bi).clone();
        assert_eq!(b.children.len(), 1, "the child among the keywords is still a child");
        b.children.clear();
        assert_eq!(a, b);
        assert_eq!(a.pad, 1);
        assert_eq!(a.gap, 2);
        assert_eq!(a.dir, Dir::Row);
        assert_eq!(a.width, Length::Px(3));
        assert_eq!(a.height, Length::Px(4));
        assert_eq!(a.align, Align::Center);
        assert_eq!(a.background, Some(5));
        assert_eq!(a.border, Some(6));
        assert_eq!(a.tag, Some(7));
    }

    #[test]
    fn all_four_lengths_have_a_spelling() {
        for (src, want) in [
            ("48", Length::Px(48)),
            ("-3", Length::Px(-3)),
            ("(pct 100)", Length::Pct(100)),
            ("fill", Length::Fill),
            ("auto", Length::Auto),
        ] {
            let (scene, id) = root(&format!("(block :width {src})"));
            assert_eq!(block_of(&scene, id).width, want, "for :width {src}");
        }
        let (scene, id) = root("(rect :width fill :height 2 :fill 9)");
        assert_eq!(
            scene.node(id),
            Some(&Node::Rect { w: Length::Fill, h: Length::Px(2), fill: Some(9) })
        );
    }

    /// The inline-equation case `docs/gui.org` argues for: `$x_1$` in the middle
    /// of a sentence hangs below the baseline by its depth, so a parser that
    /// dropped `depth` would float every subscript above the text it belongs to.
    #[test]
    fn an_image_run_and_a_block_image_both_keep_their_depth() {
        let (scene, id) = root(
            r#"(block
                 (text (run "where ") (image-run 12 :width 9 :height 20 :depth 6 :tag 3)
                       (run " is the first."))
                 (image 12 :width 400 :height 90 :depth 6))"#,
        );
        let b = block_of(&scene, id);
        let runs = runs_of(&scene, b.children[0]);
        assert_eq!(
            runs[1],
            Run::Image { image: 12, width: 9, height: 20, depth: 6, tag: Some(3) }
        );
        assert_eq!(runs[1].tag(), Some(3));
        assert_eq!(
            scene.node(b.children[1]),
            Some(&Node::Image { image: 12, width: 400, height: 90, depth: 6 })
        );
    }

    /// Escaping is the one thing the two halves of the bridge must agree about,
    /// so this reads what `zemacs_rpc::lisp::string` writes.
    #[test]
    fn a_string_survives_the_escapes_the_printer_puts_in() {
        let original = r#"say "hi\there""#;
        let printed = zemacs_rpc::lisp::string(original);
        assert_eq!(printed, r#""say \"hi\\there\"""#);
        let (scene, id) = root(&format!("(text (run {printed}))"));
        match &runs_of(&scene, id)[0] {
            Run::Text { text, .. } => assert_eq!(text, original),
            other => panic!("expected a text run, found {other:?}"),
        }
        // A newline stands for itself inside a literal, which is what that
        // module relies on when it emits one unescaped.
        let (scene, id) = root("(text (run \"a\nb\"))");
        match &runs_of(&scene, id)[0] {
            Run::Text { text, .. } => assert_eq!(text, "a\nb"),
            other => panic!("expected a text run, found {other:?}"),
        }
    }

    /// Non-ASCII survives this parser byte for byte — but *this parser* is not
    /// the whole path, and the difference matters.
    ///
    /// `crates/lisp/tests/eval.rs` writes up the boundary at length: a form
    /// handed to `zemacs_eval` arrives as a base string of UTF-8 bytes that
    /// ECL's reader takes for Latin-1 characters, so `λ` printed back out of the
    /// image comes over as `Î»`. Nothing in this file can fix that — by the time
    /// a `&str` reaches `parse` the damage, if any, is already in the bytes.
    ///
    /// So what is pinned here is what is true: the parser is transparent. Given
    /// the characters, it yields the characters; given mojibake, it yields the
    /// same mojibake unchanged rather than compounding it. The second assertion
    /// is deliberately the double-encoded form, and it is *not* a claim that the
    /// arrow arrives that way — it is a claim that this code adds no second
    /// encoding of its own.
    #[test]
    fn non_ascii_text_is_passed_through_byte_for_byte() {
        let (scene, id) = root(r#"(text (run "λ → ∀ 日本語 café"))"#);
        match &runs_of(&scene, id)[0] {
            Run::Text { text, .. } => {
                assert_eq!(text, "λ → ∀ 日本語 café");
                assert_eq!(text.as_bytes(), "λ → ∀ 日本語 café".as_bytes());
            }
            other => panic!("expected a text run, found {other:?}"),
        }

        // What a `λ` looks like after the round trip eval.rs describes. It is
        // still valid UTF-8, still a string, and comes back out identical.
        let (scene, id) = root("(text (run \"Î»\"))");
        match &runs_of(&scene, id)[0] {
            Run::Text { text, .. } => assert_eq!(text, "Î»"),
            other => panic!("expected a text run, found {other:?}"),
        }
    }

    #[test]
    fn unbalanced_parens_are_an_error_and_not_a_panic() {
        for src in [
            "(block",
            "(block (text (run \"x\")",
            "(block))",
            ")",
            "",
            "   ",
            "(text (run \"unterminated))",
        ] {
            assert!(parse(src).is_err(), "{src:?} should not have parsed");
        }
        // The message says which paren and where, because a scene is printed on
        // one long line and "unbalanced" on its own is not findable.
        let e = parse("(block (text)").unwrap_err();
        assert!(e.contains("unbalanced"), "{e}");
        assert!(e.contains("at byte 0"), "{e}");
    }

    #[test]
    fn an_unknown_head_names_itself_in_the_error() {
        let e = parse("(paragraph (run \"x\"))").unwrap_err();
        assert!(e.contains("paragraph"), "{e}");
        assert!(e.contains("block"), "{e}");

        // A run is a piece of a flow, never a node, and says so.
        let e = parse("(run \"x\")").unwrap_err();
        assert!(e.contains("belongs inside (text ...)"), "{e}");

        let e = parse("(text (spam \"x\"))").unwrap_err();
        assert!(e.contains("spam"), "{e}");

        // The document's own unevaluated example, which is the mistake most
        // likely to be made, gets an error that says what is wrong with it.
        let e = parse(r#"(image (org-latex-image "\\dim V"))"#).unwrap_err();
        assert!(e.contains("unevaluated call"), "{e}");
    }

    #[test]
    fn a_keyword_with_no_value_is_an_error() {
        for src in ["(block :pad)", "(block :pad 1 :gap)", "(text (run \"x\" :size))"] {
            let e = parse(src).unwrap_err();
            assert!(e.contains("has no value after it"), "{src}: {e}");
        }
        // ...and so is a keyword nothing knows.
        let e = parse("(block :margin 4)").unwrap_err();
        assert!(e.contains(":margin"), "{e}");
    }

    #[test]
    fn a_bare_atom_where_a_node_was_expected_is_an_error() {
        for src in ["42", "fill", "\"a string\"", ":pad", "()", "(block 42)", "(block \"x\")"] {
            assert!(parse(src).is_err(), "{src:?} should not have parsed");
        }
        let e = parse("42").unwrap_err();
        assert!(e.contains("expected a node"), "{e}");
        // A child that is an atom is reported where the atom is, not where the
        // block is, which is the difference between a findable error and a hunt.
        let e = parse("(block\n  (text)\n  99)").unwrap_err();
        assert!(e.contains("at byte 18"), "{e}");
    }

    /// A blown stack is not a failed command, it is a dead editor, and it is not
    /// catchable — so the depth is a number the reader checks rather than a
    /// thing the hardware notices.
    #[test]
    fn nesting_deeper_than_the_reader_allows_is_an_error_rather_than_a_stack_overflow() {
        let deep = format!("{}{}", "(block ".repeat(10_000), ")".repeat(10_000));
        let e = parse(&deep).unwrap_err();
        assert!(e.contains("nested deeper than"), "{e}");

        // Unbalanced *and* deep, which is the shape a truncated message has.
        assert!(parse(&"(".repeat(100_000)).is_err());

        // ...and something within the bound still parses, so the guard is not
        // simply refusing everything.
        let ok = format!("{}{}", "(block ".repeat(60), ")".repeat(60));
        assert!(parse(&ok).is_ok());
    }

    /// `prin1` upcases every symbol it prints, so a scene built by the image and
    /// printed back is all `BLOCK` and `:PAD`. Matching only lowercase would
    /// accept forms a human typed and reject every one the editor produced.
    #[test]
    fn symbols_are_matched_however_the_printer_cased_them() {
        let (scene, id) =
            root(r#"(BLOCK :DIR ROW :WIDTH (PCT 50) :ALIGN CENTER (TEXT (RUN "x" :BOLD T)))"#);
        let b = block_of(&scene, id);
        assert_eq!(b.dir, Dir::Row);
        assert_eq!(b.width, Length::Pct(50));
        assert_eq!(b.align, Align::Center);
        match &runs_of(&scene, b.children[0])[0] {
            Run::Text { style, .. } => assert!(style.bold),
            other => panic!("expected a text run, found {other:?}"),
        }
        // ...and a symbol still carrying the package it was interned in.
        assert!(parse("(zemacs::block :dir zemacs::row)").is_ok());
    }

    /// Lisp arithmetic is unbounded and these fields are not, so a number too
    /// big is a message rather than a wrap-around that paints the wrong colour.
    #[test]
    fn out_of_range_numbers_are_errors_rather_than_silent_truncation() {
        for src in [
            "(block :background 256)",
            "(block :background -1)",
            "(block :width (pct 70000))",
            "(block :pad 99999999999)",
            "(text (run \"x\" :size 0))",
            "(image -4)",
            // Beyond an i64 entirely: read as a symbol, and reported as one.
            "(block :pad 99999999999999999999999)",
        ] {
            assert!(parse(src).is_err(), "{src:?} should not have parsed");
        }
        let e = parse("(block :background 256)").unwrap_err();
        assert!(e.contains("0..=255"), "{e}");
    }

    /// Generalised truth would read `:bold 0` as bold, which is exactly the kind
    /// of quiet wrongness a parse error is for.
    #[test]
    fn a_boolean_is_t_or_nil_and_nothing_else() {
        let (scene, id) = root(r#"(text (run "x" :bold t :italic nil))"#);
        match &runs_of(&scene, id)[0] {
            Run::Text { style, .. } => {
                assert!(style.bold);
                assert!(!style.italic);
            }
            other => panic!("expected a text run, found {other:?}"),
        }
        assert!(parse(r#"(text (run "x" :bold 0))"#).is_err());
        assert!(parse(r#"(text (run "x" :bold "yes"))"#).is_err());
    }

    /// `:family` reads both words, in either case, and an unstated one is prose.
    ///
    /// The default is the assertion that matters. A scene is a page, so nearly
    /// every run in a document says nothing about a family and has to come out
    /// proportional — a default of `mono` would leave every page set in the
    /// coding font and every one of them looking like it worked.
    #[test]
    fn a_family_is_mono_or_prose_and_an_unstated_one_is_prose() {
        let family_of = |src: &str| {
            let (scene, id) = root(src);
            match &runs_of(&scene, id)[0] {
                Run::Text { style, .. } => style.family,
                other => panic!("expected a text run, found {other:?}"),
            }
        };
        assert_eq!(family_of(r#"(text (run "x"))"#), Family::Prose);
        assert_eq!(family_of(r#"(text (run "x" :family prose))"#), Family::Prose);
        assert_eq!(family_of(r#"(text (run "x" :family mono))"#), Family::Mono);
        // Upcased, which is how a form the image printed arrives.
        assert_eq!(family_of(r#"(TEXT (RUN "x" :FAMILY MONO))"#), Family::Mono);
        // A family is orthogonal to the rest of the style: a bold monospace
        // heading is a thing a listing's caption wants and must not have to
        // choose between the two.
        let (scene, id) = root(r#"(text (run "x" :family mono :size 150 :bold t :italic t))"#);
        match &runs_of(&scene, id)[0] {
            Run::Text { style, .. } => {
                assert_eq!(style.family, Family::Mono);
                assert_eq!(style.size, 150);
                assert!(style.bold && style.italic);
            }
            other => panic!("expected a text run, found {other:?}"),
        }
        // A third word is a message naming the two there are, rather than a
        // silent fall back to prose — the way to write one is a typo, and a
        // listing quietly set in a serif is a bug you find by reading it.
        let e = parse(r#"(text (run "x" :family serif))"#).unwrap_err();
        assert!(e.contains("mono or prose"), "{e}");
        assert!(parse(r#"(text (run "x" :family 0))"#).is_err());
        assert!(parse(r#"(text (run "x" :family "mono"))"#).is_err());
        // And it is a run's property, not a paragraph's: `text` takes an align
        // and nothing else, so a family on it is a message rather than a
        // keyword that silently did nothing.
        assert!(parse("(text :family mono)").is_err());
    }

    /// A conditional face prints as `NIL` when the condition failed, and that
    /// has to mean "the pane's own colour" rather than an error, or every
    /// `(when hot 3)` in a config becomes a broken scene.
    #[test]
    fn nil_where_a_face_or_a_tag_goes_means_none() {
        let (scene, id) = root("(block :background nil :border nil :tag nil)");
        let b = block_of(&scene, id);
        assert_eq!(b.background, None);
        assert_eq!(b.border, None);
        assert_eq!(b.tag, None);
        let (scene, id) = root(r#"(text (run "x" :face nil :tag nil))"#);
        assert_eq!(runs_of(&scene, id)[0].tag(), None);
    }

    #[test]
    fn a_comment_and_stray_whitespace_are_not_part_of_the_scene() {
        let (scene, id) = root("(block ; the page\n  :pad 4\n  (rect)) ; and that is all\n");
        assert_eq!(block_of(&scene, id).pad, 4);
        // A second form after the first is refused rather than ignored: the way
        // to produce one is a missing `(`, and half a document is worse than none.
        assert!(parse("(block) (block)").is_err());
    }

    #[test]
    fn a_paragraph_aligns_and_may_be_empty() {
        let (scene, id) = root("(text :align end)");
        match scene.node(id) {
            Some(Node::Text { runs, align }) => {
                assert!(runs.is_empty());
                assert_eq!(*align, Align::End);
            }
            other => panic!("expected a text node, found {other:?}"),
        }
    }
}
