//! zemacs-render — the SDL2 window and text renderer.
//!
//! The whole frame is drawn with two primitives: filled rectangles
//! (`Canvas::fill_rect`) and single-glyph textures blitted at monospace cell
//! positions. That is *why* SDL2 instead of a GPU text stack: the cursor, the
//! visual selection, the current-line highlight and the modeline — bevel and
//! all — are just rects, and the text is a grid — no shaping, no atlas, no
//! pipeline.
//!
//! Glyphs are cached one texture per `char`, rasterised white/blended once and
//! recoloured per draw with `set_color_mod`. So a frame costs one `copy` per
//! visible cell and zero rasterisation after the first sighting of a character.
//! The cache is thrown away when the font size (or the display's DPI) changes.
//!
//! Everything is measured in *cells*: `cell_w` is the font's advance for `'M'`
//! and `line_h` is `recommended_line_spacing()`. Layout therefore assumes a
//! monospace font; the font search below only offers monospace faces.
//!
//! A *character* is not a cell. `漢` and `😀` are two cells wide, a combining
//! mark is zero, and [`char_cells`] is the only thing allowed to have an opinion
//! about which. Every offset crossing into core stays a **character** index —
//! that is what the rope, the markers and the whole Lisp API are counted in —
//! and [`expand_line`] is the one place the two units meet.
//!
//! One `Renderer` is one OS window, drawing one [`zemacs_core::Frame`]. Where
//! the panes go is not decided here: [`Renderer::render`] asks `Frame::panes`
//! for rectangles inside [`Renderer::content_area`] and draws a document, a
//! cursor and a modeline into each, so the renderer and the app's mouse
//! hit-testing are reading the same arithmetic rather than two copies of it.
//!
//! A line wider than its pane is never allowed to bleed into the next one.
//! [`zemacs_core::LineOverflow`] picks which way it is contained: `Truncate`
//! cuts it and puts a marker glyph in the last column, `Wrap` continues it on
//! the rows below. Scrolling stays by *buffer line* either way — core owns
//! `Window::scroll` — so a wrapped line taller than the room left in the pane
//! is simply cut at the bottom edge, which is what Emacs does too.
//!
//! There is one thing a pane can show that is not a grid: a **scene**, a tree
//! laid out in pixels by `crates/gui` and painted by [`Renderer::draw_scene`].
//! It is a second *layout*, not a second renderer — the faces, the glyph cache,
//! the image uploads, the clip and the frame digest are all the ones above, and
//! `docs/gui.org` says why that is the constraint the whole design was built
//! around. See the scene section near the foot of this file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use sdl2::pixels::{Color, PixelFormatEnum};
use sdl2::rect::Rect;
use sdl2::render::{BlendMode, Texture, TextureCreator, WindowCanvas};
use sdl2::surface::Surface;
use sdl2::ttf::{Font, Hinting, Sdl2TtfContext};
use sdl2::video::WindowContext;
use zemacs_core::display::{char_cells, expand_line, str_cells, visual_col, wrap_row_count};
use zemacs_core::modeline;
use zemacs_core::{
    fold_hiding, fold_starts_in, Buffer, BufferKind, CompletionStyle, Editor, HlKind, Image, ImageId,
    LineOverflow, Mode, Overlay, Settings, Span, Window,
};
// `zemacs_gui::Rect` is deliberately not imported: `Rect` in this file is
// SDL's, and three rectangle types in one namespace is how a blit ends up in
// the wrong coordinate space. The scene's is spelled out where it is used.
use zemacs_gui::{FaceId, Frame as SceneFrame, Layout, Measure, Node, Run, Scene, Style};
use zemacs_term::Screen;

/// Outer margin, in pixels, around the text area and inside the modeline.
const PAD: i32 = 8;

/// Vertical breathing room inside a completion popup. Half of [`PAD`], because
/// a full one between every rule and row makes the box look inflated.
const PADV: i32 = 4;

/// Most candidate rows a popup will ever ask for. The real count is also capped
/// by what fits above the document — see [`bottom_popup`] / [`center_popup`].
const POPUP_ROWS: usize = 10;

/// Centered popup width as a percentage of the window, then clamped to these
/// column counts so it is neither a slit on a small window nor a full-bleed
/// banner on a 5K one.
const CENTER_PCT: i32 = 66;
const CENTER_MIN_COLS: i32 = 40;
const CENTER_MAX_COLS: i32 = 100;

/// Alpha of the scrim painted over the frame before a popup. Enough to push the
/// document back, not enough to hide it.
const DIM_ALPHA: u8 = 130;

/// Drawn on the visible head of a fold. Emacs writes `...`; one ellipsis costs
/// one column instead of three and reads the same, which matters because it is
/// spent out of the line's own text.
const FOLD_MARKER: char = '…';

/// Monospace faces to try, in order. Earlier entries win.
///
/// The order is coverage-driven, not preference-driven: the dashboard banner is
/// box-drawing art (`█ ╔ ═ ╝ ▸`) and Monaco is missing the double-line box
/// glyphs, so it sits *below* Menlo even though it is the prettier terminal
/// face at small sizes.
const FONT_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/SFNSMono.ttf",
    "/System/Library/Fonts/Menlo.ttc",
    "/System/Library/Fonts/Monaco.ttf",
    "/System/Library/Fonts/Supplemental/Andale Mono.ttf",
    "/System/Library/Fonts/Supplemental/Courier New.ttf",
    "/Library/Fonts/Andale Mono.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
];

pub struct Renderer {
    canvas: WindowCanvas,

    // ponytail: the TTF context and the TextureCreator are leaked so `Font` and
    // `Texture` get `'static` lifetimes instead of forcing a self-referential
    // struct (both normally borrow from something we'd have to store next to
    // them). Ceiling: one `Renderer` per process, and the SDL renderer + TTF
    // subsystem live until exit. Upgrade path: `self_cell`/`ouroboros`, or the
    // sdl2 `unsafe_textures` feature (which would also leak into the app crate).
    ttf: &'static Sdl2TtfContext,
    textures: &'static TextureCreator<WindowContext>,
    font: Font<'static, 'static>,

    font_path: PathBuf,
    /// Point size the font is currently open at — already multiplied by the
    /// display scale, so it is *not* `settings.font_size`.
    point_size: u16,

    glyphs: HashMap<char, Option<Texture<'static>>>,
    /// The same font emboldened, with its own cache. SDL_ttf's bold is a style
    /// on the `Font`, so real and bold text cannot come from one handle — and
    /// the rasterised glyphs differ, so they cannot share a cache either.
    bold: Font<'static, 'static>,
    bold_glyphs: HashMap<char, Option<Texture<'static>>>,
    /// Every *other* face: any size an overlay's `scale` asked for, and italic
    /// at any size. See [`Face`] for what one is and [`Renderer::draw_glyph`]
    /// for the three-way split.
    ///
    /// The two fields above are not folded in here, deliberately. They are the
    /// faces the editor draws essentially all of its text in — every buffer,
    /// the modeline, every popup — and their cost is a property of the document
    /// rather than of anyone's config. This map's is the opposite: it exists
    /// only because a config asked for typesetting, and every dimension of it is
    /// therefore something policy can be held to a bound on.
    ///
    /// **The bound.** The key space is closed by [`SCALE_STEPS`] × four styles =
    /// 16, of which the body's plain and bold live above, so at most 14 faces
    /// are ever open here; [`MAX_FACES`] is the assertion of that. Each holds at
    /// most [`MAX_FACE_GLYPHS`] textures and empties itself rather than growing
    /// past it. `None` records a face that would not open, so it is not retried
    /// every frame. All of it is dropped by [`Renderer::sync`], exactly as the
    /// body caches are, since a font-size or DPI change makes every pixel of it
    /// stale.
    faces: HashMap<FaceKey, Option<Face>>,
    /// One texture per overlay image, keyed the way core keys the pixels — by
    /// what produced them. So a second `org-latex-preview` over the same buffer
    /// re-uses these rather than re-uploading a screenful of equations, which is
    /// the whole reason the id is a hash of the job and not a counter.
    ///
    /// `None` records an upload that failed, so it is not retried every frame.
    /// ponytail: nothing evicts, for the same reason nothing evicts core's
    /// bitmaps — core drops an image when the last overlay naming it goes, and
    /// the texture then sits there costing a few hundred KB of VRAM until exit.
    /// Upgrade path: drop entries `editor.image` no longer resolves.
    images: HashMap<ImageId, Option<Texture<'static>>>,
    /// The last scene laid out, and the fingerprint of what it was laid out
    /// *for* — see [`scene_key`], which is where the staleness question is
    /// actually decided.
    ///
    /// ponytail: one slot, so two panes showing scenes at once take turns
    /// evicting each other and both relay out every frame. That is exactly the
    /// cost of having no cache at all, which is what this replaced, so the
    /// degradation is to the old behaviour rather than to a wrong picture — and
    /// one scene on screen is the case `math-code-edit`'s split has today.
    /// Upgrade path: key it by `WindowId`, and drop the entries for windows the
    /// frame did not draw.
    scenes: Option<(u64, Layout)>,
    /// Digest of every draw call `render` has made this frame, and the digest of
    /// the frame that is actually on screen. Equal means the picture did not
    /// change and [`Renderer::present`] can be skipped — which is the whole
    /// reason they exist, because a present blocks until the next vertical blank
    /// and that is where a keystroke's latency goes.
    ///
    /// Folded *inside the draw primitives* rather than computed from the editor,
    /// and that is the load-bearing part: there is no list of "fields that mean a
    /// redraw" to keep in step with the renderer. Everything that reaches the
    /// canvas — the clear, `fill`, the two glyph blits, an image, and the clip
    /// that decides what any of them are allowed to touch — goes through `mark`,
    /// so a visible change cannot fail to be noticed.
    ///
    /// The one rule it cannot enforce for itself: a new primitive that talks to
    /// `self.canvas` has to fold itself in too, or its pixels become invisible
    /// to this. `grep 'self.canvas'` in this file is the whole audit.
    ///
    /// `None` means "nothing has been shown yet, or somebody invalidated it",
    /// which forces the next present.
    drawn: u64,
    shown: Option<u64>,
    /// Draw calls in the last frame — one per glyph, fill, blit and clear. Only
    /// [`Renderer::draw_calls`] reads it, and only the perf report reads that,
    /// but it costs an increment on a path that is already touching the GPU.
    draws: u32,
    cell_w: i32,
    line_h: i32,
    /// Pixels from the top of a cell down to the text baseline. Only images need
    /// it — a glyph is blitted at the cell origin — but an inline LaTeX preview
    /// is *positioned* against the baseline, which is what makes `$x_1$` hang
    /// below the line instead of floating over it.
    ascent: i32,
}

impl Renderer {
    /// The SDL render backend actually in use — `"metal"` on macOS, `"opengl"`
    /// or `"vulkan"` elsewhere. `accelerated()` below *asks* for a GPU driver;
    /// this is how you check it got one rather than falling back to `"software"`.
    pub fn backend(&self) -> String {
        self.canvas.info().name.to_string()
    }

    /// `sdl` is created (and the event pump owned) by the app.
    pub fn new(sdl: &sdl2::Sdl, title: &str, width: u32, height: u32) -> anyhow::Result<Self> {
        let video = sdl.video().map_err(|e| anyhow::anyhow!("SDL video init: {e}"))?;
        let window = video
            .window(title, width, height)
            .position_centered()
            .resizable()
            .allow_highdpi()
            .build()?;
        let mut canvas = window.into_canvas().accelerated().present_vsync().build()?;
        canvas.set_blend_mode(BlendMode::Blend);

        let textures: &'static TextureCreator<WindowContext> =
            Box::leak(Box::new(canvas.texture_creator()));
        let ttf: &'static Sdl2TtfContext = Box::leak(Box::new(
            sdl2::ttf::init().map_err(|e| anyhow::anyhow!("SDL_ttf init: {e}"))?,
        ));

        let font_path = find_font()?;
        // Matches `Settings::default().font_size`; `sync` fixes it up on frame one.
        let point_size = scale_point_size(18.0, dpi_scale(&canvas));
        let mut font = ttf
            .load_font(&font_path, point_size)
            .map_err(|e| anyhow::anyhow!("cannot open font {}: {e}", font_path.display()))?;
        set_hinting(&mut font);
        let bold = open_bold(ttf, &font_path, point_size)?;
        let (cell_w, line_h) = metrics(&font);

        let ascent = font.ascent();

        Ok(Self {
            canvas,
            ttf,
            textures,
            font,
            bold,
            bold_glyphs: HashMap::new(),
            faces: HashMap::new(),
            images: HashMap::new(),
            scenes: None,
            font_path,
            point_size,
            glyphs: HashMap::new(),
            drawn: FNV_SEED,
            shown: None, // nothing on screen yet, so frame one always presents
            draws: 0,
            cell_w,
            line_h,
            ascent,
        })
    }

    /// Re-open the font when `settings.font_size` changes or the window moves to
    /// a display with a different scale factor. Resizes need no work: the canvas
    /// tracks the window, so this is a no-op for them.
    pub fn sync(&mut self, editor: &Editor) -> anyhow::Result<()> {
        let want = scale_point_size(editor.settings.font_size, dpi_scale(&self.canvas));
        if want == self.point_size {
            return Ok(());
        }
        let ttf = self.ttf;
        let path = self.font_path.clone();
        self.font = ttf
            .load_font(&path, want)
            .map_err(|e| anyhow::anyhow!("cannot re-open font {}: {e}", path.display()))?;
        set_hinting(&mut self.font);
        self.bold = open_bold(ttf, &path, want)?;
        self.point_size = want;
        self.glyphs.clear(); // rasterised at the old size, all of it is stale
        self.bold_glyphs.clear();
        // ...and the scaled faces doubly so: their *point sizes* were derived
        // from the old one, so both the fonts and their glyphs are wrong.
        self.faces.clear();
        let (cell_w, line_h) = metrics(&self.font);
        self.cell_w = cell_w;
        self.line_h = line_h;
        self.ascent = self.font.ascent();
        // Images are rasterised to match the *text*, so a font-size change makes
        // every one of them the wrong size — but they are core's, produced by
        // whoever asked for them, so this only drops the uploads. The next
        // preview renders at the new size and the stale entries are unreachable.
        self.images.clear();
        Ok(())
    }

    /// This window's SDL id, so the app can route events to the right frame.
    pub fn window_id(&self) -> u32 {
        self.canvas.window().id()
    }

    /// The rectangle the window tree is laid out in — exactly what gets handed
    /// to [`zemacs_core::Frame::panes`], and what the app hit-tests dividers
    /// against. The whole drawable: every pane carries its own modeline, so
    /// nothing is reserved at frame level.
    pub fn content_area(&self) -> zemacs_core::Rect {
        // `output_size` only fails on a dead renderer, which is not something a
        // hit test can do anything about — an empty area yields no panes.
        let (w, h) = self.canvas.output_size().unwrap_or((0, 0));
        zemacs_core::Rect::new(0, 0, w as i32, h as i32)
    }

    /// An SDL mouse position (logical window coordinates) in the pixel space
    /// [`Renderer::content_area`] and the panes use.
    ///
    /// The canvas is `allow_highdpi`, so on a Retina display the drawable is
    /// twice the window and the two spaces are a factor of two apart. Skipping
    /// this puts every divider grab and pane click in the wrong half of the
    /// window.
    pub fn to_pixels(&self, x: i32, y: i32) -> (i32, i32) {
        scale_point(dpi_scale(&self.canvas), x, y)
    }

    /// Draw frame `frame_index` of the editor. Writes `viewport_lines` and
    /// `wrap_cols` back — per window, and on the editor from the focused pane —
    /// because only the renderer knows how many lines actually fit or how many
    /// cells a line has before it wraps, and core needs both: one to clamp
    /// scrolling, the other so `j` and `k` can move by visual line.
    ///
    /// The app calls `Editor::sync_focused_window` first, so every window's
    /// `cursor`/`scroll` is live and no pane needs a special case.
    ///
    /// Drawing only — [`Renderer::present`] puts it on screen. They are separate
    /// because the canvas is vsynced, so `present` parks the thread for most of
    /// a frame, and the editor lock must not be held across that or every Lisp
    /// primitive would queue behind the display.
    pub fn render(
        &mut self,
        editor: &mut Editor,
        frame_index: usize,
        terminal: Option<&Screen>,
    ) -> anyhow::Result<()> {
        self.sync(editor)?; // cheap no-op; makes an app-side `sync` call optional
        // Start this frame's digest from the two things that move *every* glyph
        // rather than one of them: the size the font is open at, and the size of
        // the canvas they land on. A resize that happens to leave the same
        // characters at the same coordinates is otherwise indistinguishable.
        let (out_w, out_h) = self.canvas.output_size().unwrap_or((0, 0));
        self.drawn = FNV_SEED;
        self.draws = 0;
        self.mark([
            TAG_FRAME,
            pack(out_w as i32, out_h as i32),
            u64::from(self.point_size),
            frame_index as u64,
        ]);
        // The one thing core cannot work out for itself: how big the em really
        // is once the display's scale factor is in. Parked here rather than
        // asked for, exactly as `viewport_lines` is, and read by the LaTeX
        // primitive so a preview is the size of the text it sits in.
        editor.font_px = f32::from(self.point_size);

        let area = area_of(self.content_area());
        let status_h = modeline_h(self.line_h, &editor.settings);
        let focused = frame_index == editor.focus_frame;

        let bg = editor.settings.background;
        self.canvas.set_draw_color(rgb(bg));
        self.canvas.clear();
        self.mark([TAG_CLEAR, 0, 0, bits(rgb(bg))]);
        self.draws += 1;

        // Layout once, then two passes: the first needs `&mut editor` to park
        // the row counts, the second only `&`.
        let Some(frame) = editor.frames.get(frame_index) else {
            return Ok(());
        };
        let current = frame.current;
        let panes = frame.panes(area_rect(area));
        let dividers = frame.dividers(area_rect(area));

        for p in &panes {
            let pane = area_of(p.rect);
            let rows = doc_lines(pane, status_h, self.line_h);
            // Wrapping breaks the one-row-per-line assumption this used to be a
            // division by, so the lines have to be laid out to be counted.
            let laid_out = editor.frames[frame_index]
                .window(p.window)
                .map(|w| (w.buffer, w.scroll))
                .and_then(|(id, scroll)| {
                    let buf = editor.buffer_by_id(id)?;
                    let set = &editor.settings;
                    let doc = doc_rect(pane, status_h, measure_px(set, self.cell_w));
                    let text_w = doc.w - gutter_w(buf, set, self.cell_w);
                    let cols = visible_cols(text_w, self.cell_w);
                    Some((
                        visible_lines(buf, scroll, rows, text_w, self.cell_w, set),
                        cols,
                    ))
                });
            // `cols` goes back too, and it is not decoration: `j` and `k` move
            // by *visual* line, and where a visual line breaks is this number.
            // Core has no other way to learn it — the width of a cell is a font
            // fact and the gutter's width is a layout one.
            //
            // ponytail: it is the *body* line's column count. A line an overlay
            // has scaled or given a prefix to breaks earlier than this says, so
            // `j` over a wrapped heading steps by the wrong visual line. Ceiling:
            // a heading long enough to wrap, which a heading rarely is. Upgrade
            // path is the one core already needs for the same reason — see the
            // note on `visible_lines` in the draw loop — handing core the overlay
            // list so it can lay a line out the way the renderer does.
            let (lines, cols) = laid_out.unwrap_or((rows, 0));
            if let Some(w) = editor.frames[frame_index].window_mut(p.window) {
                w.viewport_lines = lines;
                w.wrap_cols = cols;
            }
            // Core clamps scrolling against this one, and it belongs to
            // whichever window is being typed into.
            if focused && p.window == current {
                editor.viewport_lines = lines;
                editor.wrap_cols = cols;
            }
        }

        let editor = &*editor;
        for p in &panes {
            let pane = area_of(p.rect);
            if pane.w <= 0 || pane.h <= 0 {
                continue; // a frame dragged to nothing; SDL dislikes empty clips
            }
            let Some(win) = editor.frames[frame_index].window(p.window) else {
                continue;
            };
            let Some(buf) = editor.buffer_by_id(win.buffer) else {
                continue;
            };
            let active = focused && p.window == current;
            // Everything inside a pane is clipped to it, so a long line, a
            // selection or an overhanging glyph cannot paint into its
            // neighbour. Cheaper than teaching every primitive about the pane.
            self.set_clip(pane);
            // A scene is asked for; a buffer's *kind* is merely what it is. So
            // the page wins over both special-cased renderers below, and the
            // dashboard in particular: it is the one buffer live at startup, so
            // a config that installs a page from `init.lisp` installs it there,
            // and matching on kind first meant that page was stored and silently
            // never drawn. A scene on a terminal is the same argument — you
            // asked for it over the grid.
            if let Some(scene) = &buf.scene {
                self.draw_pane_scene(editor, scene, doc_rect(pane, status_h, 0));
            } else {
                match (buf.kind, terminal) {
                    // Neither of these takes the measure. The dashboard centres
                    // its own art in whatever it is given, and a terminal's
                    // width is a fact the child process has already been told —
                    // narrowing either would be the setting reaching past the
                    // documents it is about.
                    (BufferKind::Dashboard, _) => {
                        self.draw_dashboard(editor, doc_rect(pane, status_h, 0))
                    }
                    // A terminal is drawn from its live grid rather than from
                    // the rope, because per-cell colour and the block cursor are
                    // both gone by the time the grid has been flattened into
                    // text. The rope is still what the buffer switcher reads.
                    (BufferKind::Terminal, Some(screen)) => {
                        let (bg, fg) = (editor.settings.background, editor.settings.foreground);
                        self.draw_terminal(screen, doc_rect(pane, status_h, 0), rgb(bg), rgb(fg));
                    }
                    _ => self.draw_document(editor, buf, win, pane, status_h, active),
                }
            }
            self.draw_modeline(editor, buf, modeline_rect(pane, status_h), active);
            self.clear_clip();
        }

        let div_c = rgb(divider_shade(&editor.settings));
        for d in &dividers {
            let r = area_of(d.rect);
            self.fill(r.x, r.y, r.w, r.h, div_c);
        }

        // Ace labels: one big letter per pane, drawn over everything so it is
        // readable whatever is underneath. Only on the focused frame, since
        // that is the one whose next keystroke will pick.
        if focused {
            if let Some(labels) = &editor.ace {
                let bg = rgb(editor.theme.color(HlKind::Keyword, editor.settings.foreground));
                let fg = rgb(editor.settings.background);
                for pane in &panes {
                    let Some((label, _)) = labels.iter().find(|(_, id)| *id == pane.window) else {
                        continue;
                    };
                    let r = area_of(pane.rect);
                    let (w, h) = (self.cell_w * 3, self.line_h + PADV * 2);
                    let (x, y) = (r.x + r.w / 2 - w / 2, r.y + r.h / 2 - h / 2);
                    self.fill(x, y, w, h, bg);
                    self.draw_str(
                        &label.to_string(),
                        x + self.cell_w,
                        y + PADV,
                        fg,
                    );
                }
            }
        }

        // The prompt and its popup belong to the editor, not to a pane, so they
        // are drawn once over everything — and only on the frame that owns the
        // keyboard, or every open frame would show the same M-x box.
        if focused {
            self.draw_which_key(editor, area.w, area.h, status_h);
            self.draw_completion(editor, area.w, area.h, status_h);
        }

        Ok(())
    }

    /// Put the drawn frame on screen. Blocks until the next vertical blank, so
    /// this is what paces the main loop — and why the caller drops the editor
    /// lock before getting here.
    pub fn present(&mut self) {
        self.canvas.present();
        self.shown = Some(self.drawn);
    }

    /// Throw the frame just drawn away instead of putting it on screen.
    ///
    /// **The other half of [`Renderer::changed`], and not optional.** Nothing in
    /// `render` talks to the GPU: every `fill` and every glyph blit is appended
    /// to a command list inside SDL, and that list is drained only when the
    /// renderer is *flushed* — which `present` does on its way to the swap.
    /// Skipping the present therefore does not skip the work; it leaves it
    /// queued, and the next frame appends to the same list. Nothing ever drops
    /// it, so an editor sitting still and drawing frames it decides not to show
    /// grows by a whole frame's worth of commands per display frame.
    ///
    /// Measured before this existed, on an untouched window: 3 MB a second, a
    /// 96-byte command per glyph plus the vertex buffer they index — reallocated
    /// ever larger because SDL resets its high-water mark only on a flush. An
    /// idle hour was 11 GB.
    ///
    /// So the list is executed and dropped. That spends the GPU the frame we
    /// just decided nobody wanted, which is a real cost and exactly why this is
    /// not simply a `present`: what a present *also* does is block until the
    /// next vertical blank, and that blank is the 14 ms the skip exists to save.
    pub fn discard(&mut self) {
        // Safe in spite of the signature. `SDL_RenderFlush` takes the renderer
        // and answers a status; `sdl2` marks the entry points it added late as
        // `unsafe` rather than auditing them.
        unsafe { self.canvas.render_flush() };
    }

    /// Whether the frame [`Renderer::render`] just drew differs from the one on
    /// screen. False means `present` would block for a vertical blank to put up
    /// a picture identical to the one already there.
    ///
    /// Skipping the present leaves the window showing its last presented frame.
    /// That is not a bet on the back buffer surviving — we clear and redraw the
    /// whole canvas every frame, so nothing is ever *carried over* into a frame;
    /// it is only the front buffer standing still, which is what every
    /// event-driven GUI on every windowing system relies on. What it does assume
    /// is that a window needing repainting says so, hence [`Renderer::invalidate`].
    pub fn changed(&self) -> bool {
        self.shown != Some(self.drawn)
    }

    /// Force the next `present`, whatever was drawn. For when the system, rather
    /// than the editor, is the reason the window needs putting up again: exposed,
    /// resized, un-minimised, moved to another display.
    pub fn invalidate(&mut self) {
        self.shown = None;
    }

    /// Draw calls in the last rendered frame — roughly one per visible character.
    /// Instrumentation only; see the perf report in the app.
    pub fn draw_calls(&self) -> u32 {
        self.draws
    }

    /// One frame of this window's display, in milliseconds. The loop sleeps this
    /// long when it has nothing to put on screen, so it wants the real refresh
    /// rate rather than a guess: on a 120 Hz panel, guessing 60 would double how
    /// long a change made off the event queue waits to be noticed.
    ///
    /// 60 Hz when SDL cannot say. Asked per sleep rather than cached because a
    /// window can be dragged to a display with a different rate, and the call is
    /// a field read behind an SDL lock — far cheaper than the sleep it sizes.
    pub fn frame_ms(&self) -> u32 {
        let hz = self
            .canvas
            .window()
            .display_mode()
            .ok()
            .map(|m| m.refresh_rate)
            .filter(|hz| *hz > 0)
            .unwrap_or(60);
        (1000 / hz).max(1) as u32
    }

    // --- pointing ----------------------------------------------------------

    /// The window a click at `(x, y)` landed in, and the buffer offset under
    /// the pointer.
    ///
    /// The inverse of [`Renderer::draw_document`]'s row loop, and it has to
    /// live next to it: every step from a pixel to a character goes through a
    /// layout fact core cannot see. The gutter's width depends on the font;
    /// where a line breaks depends on the pane's width; a folded line occupies
    /// no row at all; a wrapped one occupies several; and a tab or a CJK glyph
    /// occupies several *cells* per character. Core knows none of that, which
    /// is why the event loop cannot do this arithmetic itself and why the
    /// answer comes back as an offset rather than as a (line, column).
    ///
    /// `None` when the click was on a divider, or in a pane showing something
    /// that is not buffer text — the dashboard and a terminal both draw rows
    /// that no rope position corresponds to.
    pub fn click_target(
        &self,
        editor: &Editor,
        frame_index: usize,
        x: i32,
        y: i32,
    ) -> Option<(zemacs_core::frame::WindowId, usize)> {
        let area = area_rect(area_of(self.content_area()));
        let status_h = modeline_h(self.line_h, &editor.settings);
        let frame = editor.frames.get(frame_index)?;
        let p = frame.panes(area).into_iter().find(|p| p.rect.contains(x, y))?;
        let win = frame.window(p.window)?;
        let buf = editor.buffer_by_id(win.buffer)?;
        if matches!(buf.kind, BufferKind::Dashboard | BufferKind::Terminal) {
            return None;
        }

        let at = offset_at(
            buf,
            win,
            &editor.settings,
            area_of(p.rect),
            status_h,
            self.line_h,
            self.cell_w,
            x,
            y,
        );
        Some((p.window, at))
    }

    // --- document ---------------------------------------------------------

    /// One pane's buffer. `win` carries the cursor and scroll — the focused
    /// window's are copied onto it before the frame, so this never reads
    /// `editor.buffer.cursor`.
    ///
    /// Two nested loops: buffer lines down the pane, and the display rows each
    /// of them occupies. Truncation is the degenerate case of wrapping — the
    /// first row only, plus a marker — so both modes walk the same code and
    /// there is one place where a cell can be placed outside the pane.
    fn draw_document(
        &mut self,
        editor: &Editor,
        buf: &Buffer,
        win: &Window,
        pane: Area,
        status_h: i32,
        active: bool,
    ) {
        let set = &editor.settings;
        let (bg, fg) = (set.background, set.foreground);
        let doc = doc_rect(pane, status_h, measure_px(set, self.cell_w));
        let (cur_line, cur_col) = line_col(buf, win.cursor);
        // The selection and the highlight spans both describe `editor.buffer`
        // as the focused window sees it, and there is exactly one of each in
        // the editor — so no other pane may borrow them.
        // `selection_ranges`, not `selection`: a block selection is genuinely
        // several disjoint runs, and its bounding span would paint the middle
        // of every line it covers.
        let selection: Vec<(usize, usize)> = if active {
            editor.selection_ranges()
        } else {
            Vec::new()
        };
        // Every pane draws its own buffer's spans. Handing an empty slice to
        // anything but the live buffer is what used to make a split lose its
        // colours the moment focus moved.
        let spans: &[Span] = &buf.highlights;
        // ...and its own overlays, for the same reason. Empty on nearly every
        // buffer, and everything below early-outs on that, so a config that
        // never makes one pays a slice length per line.
        //
        // Priority, since two things now claim the same cell:
        //   1. an overlay's `face` / `background` beats the syntax highlight;
        //   2. among overlapping overlays the *most recently made* wins, and per
        //      attribute — one that sets only a background does not wipe an
        //      earlier one's foreground;
        //   3. the selection and the cursor beat both, because they are claims
        //      about the editor rather than about the text and must never be
        //      paintable over.
        let overlays: &[Overlay] = buf.overlays();
        let block_cursor = editor.mode != Mode::Insert;

        let digits = gutter_digits(buf);
        // The *same* question `gutter_w` answers, asked once and used twice.
        // Reading `set.line_numbers` here instead is what put an org buffer's
        // numbers on top of its own first three characters: the width came from
        // the buffer's answer and the ink came from the editor's.
        let numbered = gutter_on(buf, set);
        let gutter = gutter_w(buf, set, self.cell_w);
        let x0 = doc.x + gutter;
        // Pixels rather than columns, and that is the shape of the whole change
        // that let a line be typeset: how many columns a line has depends on how
        // big its type is and how far its prefix pushed it in, so the pane hands
        // down a width and each line divides it — see [`line_box`].
        let text_w = doc.w - gutter;
        let wrap = set.line_overflow == LineOverflow::Wrap;

        let sel_bg = rgb(mix(bg, fg, 0.28));
        let cur_bg = rgb(mix(bg, fg, 0.05));
        let cursor_c = rgb(mix(bg, fg, 0.85));
        let num_c = rgb(mix(bg, fg, 0.35));
        let num_cur_c = rgb(mix(bg, fg, 0.7));
        // The truncation marker is chrome, not a character in the file, so it
        // gets the accent hue rather than a shade of the text colour — dimmed,
        // so it does not out-shout the line it is annotating.
        let marker_c = rgb(mix(bg, editor.theme.color(HlKind::Function, fg), 0.75));

        // Highlight spans are whole-buffer char offsets and sorted, so one
        // monotonic cursor walks them alongside the lines — no rescan per line.
        let mut si = 0usize;
        let rows = doc_lines(pane, status_h, self.line_h);
        // The cursor's *line* is all a relative number needs — `cur_line` above
        // — because the count is of buffer lines. Working out which display row
        // the cursor sits on used to be needed here and is not any more; see
        // `gutter_number`. Per window either way: each pane counts from its own
        // cursor, which is what makes an inactive split's gutter describe
        // itself rather than the focused pane.

        // `row` is a display row in the pane, `line` a buffer line: with
        // wrapping the two advance at different rates.
        let (mut row, mut line) = (0usize, win.scroll);
        while row < rows && line < buf.len_lines() {
            let start = buf.line_start(line);
            let len = buf.line_len(line);
            let end = start + len;

            // A folded line occupies no row at all — the one thing an overlay
            // could not do before, since every other payload replaces cells and
            // this makes them stop existing. `row` is deliberately not advanced:
            // the pane's rows are spent on lines that are drawn.
            //
            // The highlight cursor is *not* advanced past it either, and must
            // not be: `spans_for_line` only retires a span that ends before the
            // line it is asked about, so skipping here leaves the walk exactly
            // where the next drawn line needs it.
            if fold_hiding(overlays, start).is_some() {
                line += 1;
                continue;
            }

            let (next, runs) = spans_for_line(spans, si, start, end);
            si = next;

            let ov_runs = overlays_for_line(overlays, start, end);
            // What the overlays say about the *line*, resolved before its cells
            // because the cell width it decides is the unit everything below is
            // measured in. `pct` is the type size; 100 is the body, and on a
            // buffer with no typesetting overlays every derived quantity here
            // collapses to what it was before any of this existed.
            let style = line_style(overlays, start, end);
            let pct = style.scale.max(100);
            let lb = line_box(&style, text_w, self.cell_w);
            let lx0 = x0 + lb.indent;
            let row_h = lb.tall as i32 * self.line_h;
            // Where the type sits inside the block of rows it claimed: on the
            // bottom, so the slack is *above* the line. That is the right way up
            // for the case this exists for — a heading gets its air before it and
            // its body text tucked under it — and it is also the only placement
            // that cannot spill: the em box is `line_h * pct/100` tall and the
            // block is `ceil(pct/100)` rows of `line_h`.
            let drop = row_h - scaled(self.line_h, pct);
            let mut cells = expand_line(&buf.slice_string(start, end), set.tab_width);
            // `display` and an image both *replace* the cells they cover, which
            // is what makes wrapping, the cursor and the selection follow the
            // substitution instead of having to be told about it separately.
            //
            // An overlay reaching onto later lines substitutes on the first one
            // and blanks the rest: one image, then the empty rows its own source
            // lines have become.
            let mut images: Vec<(usize, zemacs_core::ImageId)> = Vec::new();
            // Rows this line must own whatever its text says — the one thing in
            // the renderer that makes a row's height variable. See `need` below.
            let mut tall = 1usize;
            if !ov_runs.is_empty() {
                let mut subs: Vec<(usize, usize, String)> = Vec::new();
                for &(s, e, o) in &ov_runs {
                    // An image is the more specific claim, so an overlay
                    // carrying both never shows its `display` string.
                    let image = o.image.and_then(|i| editor.image(i).map(|img| (i, img)));
                    let text = match (&image, &o.display) {
                        (None, None) => continue, // a face-only overlay hides nothing
                        _ if o.start < start => String::new(), // continuation row
                        (Some((id, img)), _) => {
                            images.push((s, *id));
                            // A tall image grows the line it starts on rather
                            // than painting over the text below it. A *multi*-
                            // line fragment already owns one blank row per
                            // further source line it covers — the arm above is
                            // what blanked them — so only the shortfall is
                            // charged here. That is why `\begin{equation}`,
                            // whose three source lines usually already hold it,
                            // looks exactly as it did, while a `$$…$$` written
                            // on one line stops overwriting its neighbour.
                            let last = buf.text.char_to_line(o.end.saturating_sub(1));
                            let blanked = last.saturating_sub(line);
                            let want =
                                image_rows(img.height, img.depth, self.line_h, self.ascent);
                            tall = tall.max(want.saturating_sub(blanked).max(1));
                            " ".repeat(image_cells(img.width, lb.cw))
                        }
                        (None, Some(d)) => d.clone(),
                    };
                    subs.push((s, e, text));
                }
                if !subs.is_empty() {
                    subs.sort_by_key(|&(s, _, _)| s);
                    cells = substitute(&cells, &subs);
                    // The blit needs a *column*, and the substitution is what
                    // decided which one, so this cannot be worked out earlier.
                    for entry in &mut images {
                        entry.0 = visual_col(&cells, entry.0);
                    }
                }
            }

            // Selection as display-cell ranges, resolved once per line so the
            // row loop only has to clip. `end + 1` is the newline, kept as one
            // trailing cell so a linewise selection visibly swallows the break.
            let sel_cells: Vec<(usize, usize)> = selection
                .iter()
                .filter(|&&(s, e)| e > start && s <= end)
                .map(|&(s, e)| {
                    let b = e.min(end + 1) - start;
                    (
                        visual_col(&cells, s.max(start) - start),
                        if b > len { cells.len() + 1 } else { visual_col(&cells, b) },
                    )
                })
                .collect();

            // Rows this line wants. Three claims, resolved as a `max` on one
            // integer rather than a table: what its text needs at its own type
            // size, and never fewer than what an image on it needs.
            //
            // A *visual* row — one pass across the pane — is `lb.tall` display
            // rows, which is how a scaled line is taller without any fractional
            // row heights existing anywhere. `image_rows` established the shape
            // (a line may own several rows) and this reuses it rather than
            // teaching `doc_lines`, `line_rows`, `cursor_pane_row`, `scroll` and
            // `offset_at` about pixels.
            //
            // ponytail: a line taller than the rows left in the pane is cut at
            // the bottom edge, because `scroll` counts buffer lines and there is
            // no row to scroll to. Ceiling: a paragraph-length line, a full-page
            // equation, or a 2× heading, at the foot of a short pane. Upgrade
            // path: a scroll position of (line, row).
            //
            // ponytail: `visible_lines` and `cursor_pane_row` count a scale and
            // a fold but still not an *image*, so a pane showing a display
            // equation reports a line or two more than it draws. That one is
            // core's ceiling as much as this file's — core lays a line out with
            // no overlays at all, see boundary.org — and closing it is handing
            // both of them the overlay list this loop is already walking.
            let text_rows = if wrap {
                wrap_row_count(cells.len(), lb.cols)
            } else {
                1
            };
            let need = (text_rows * lb.tall).max(tall);
            let fits = need.min(rows - row);
            let shown_rows = drawn_rows(fits, lb.tall, text_rows);

            // Only the focused window of the focused frame gets a cursor: two
            // blocks on screen at once is two claims about where typing goes.
            let cursor_at = (active && line == cur_line)
                .then(|| cursor_pos(visual_col(&cells, cur_col), lb.cols, text_rows.max(1)));

            // ponytail: no horizontal scrolling. A truncated line gives up its
            // last column to the marker; the tail is simply not reachable.
            // Upgrade path: an `hscroll` alongside `scroll`.
            let marker = (!wrap)
                .then(|| truncation_marker(cells.len(), lb.cols))
                .flatten();
            // This line is the visible head of a fold, so it says so.
            let folds_here = fold_starts_in(overlays, start, end);

            let mut ri = 0usize;
            for (r, (rs, re)) in wrap_rows(cells.len(), lb.cols)
                .take(shown_rows)
                .enumerate()
            {
                let y = doc.y + (row + r * lb.tall) as i32 * self.line_h;
                // Where this row's *type* goes. Body text sits at the top of its
                // one row, as it always has, because `drop` is zero when nothing
                // scaled the line.
                let ty = y + drop;
                let cursor_col = cursor_at.and_then(|(cr, cc)| (cr == r).then_some(cc));
                // The marker replaces the last cell rather than following it:
                // drawn one column further right it would be the overflow it
                // exists to warn about.
                let shown = marker.unwrap_or(re - rs);

                // A band across the whole pane, and the point of it being a band
                // rather than a run of cell backgrounds: a source block drawn the
                // other way is a stripe as ragged as its own text. It replaces
                // the current-line stripe rather than sitting under it — both are
                // backgrounds, and the one a config asked for by name wins.
                match style.background {
                    Some(k) => {
                        let c = rgb(editor.theme.color(k, fg));
                        self.fill(pane.x, y, pane.w, row_h, c);
                    }
                    None if line == cur_line && selection.is_empty() => {
                        self.fill(pane.x, y, pane.w, row_h, cur_bg);
                    }
                    None => {}
                }
                // Overlay backgrounds go under the selection and the cursor: a
                // config painting a range must not be able to hide where the
                // editor thinks you are.
                for (col, &(_, src)) in cells[rs..rs + shown].iter().enumerate() {
                    if let (_, Some(k)) = overlay_face(&ov_runs, src) {
                        let x = lx0 + col as i32 * lb.cw;
                        let c = rgb(editor.theme.color(k, fg));
                        self.fill(x, y, lb.cw, row_h, c);
                    }
                }
                for &s in &sel_cells {
                    if let Some((a, b)) = row_span(s, rs, lb.cols) {
                        let x = lx0 + a as i32 * lb.cw;
                        self.fill(x, y, (b - a) as i32 * lb.cw, row_h, sel_bg);
                    }
                }
                // The prefix, in the gap its own width opened. Every row of the
                // line gets it, continuation rows included — a quote bar with a
                // hole in it where a line wrapped is not a quote bar.
                if let Some((p, face)) = style.prefix {
                    let c = match face {
                        Some(k) => rgb(editor.theme.color(k, fg)),
                        None => rgb(fg),
                    };
                    self.draw_run(p, x0, ty, c, Cut::plain(pct));
                }

                // An *absolute* number belongs to the buffer line, not to each
                // of the rows it occupies: repeating it down a wrapped line
                // would read as several lines, so continuation rows get a blank
                // gutter. A *relative* one is a fact about the row — it counts
                // screen lines, which is what `display-line-numbers-type
                // 'visual` means — so every row gets one. After the stripe,
                // which is painted across the gutter too.
                if numbered && r == 0 {
                    let c = if line == cur_line { num_cur_c } else { num_c };
                    let n = gutter_number(line, cur_line, set.relative_line_numbers);
                    // Body size, always: the gutter is chrome and belongs to the
                    // pane rather than to the line it happens to be beside.
                    self.draw_str(&format!("{n:>digits$}"), doc.x, y, c);
                }

                if block_cursor {
                    if let Some(cc) = cursor_col {
                        let x = lx0 + cc as i32 * lb.cw;
                        // As wide as the character under it. A one-cell block on
                        // 漢 covers half a glyph and knocks out the whole of it,
                        // so the character under the cursor would vanish. A
                        // tab's cells are spaces, so this leaves tabs at one.
                        let w = cells.get(rs + cc).map_or(1, |&(c, _)| char_cells(c).max(1));
                        self.draw_cursor(x, y, w as i32 * lb.cw, row_h, pane, cursor_c);
                    }
                }

                for (col, &(ch, src)) in cells[rs..rs + shown].iter().enumerate() {
                    let x = lx0 + col as i32 * lb.cw;
                    while ri < runs.len() && runs[ri].1 <= src {
                        ri += 1;
                    }
                    let kind = match runs.get(ri) {
                        Some(&(s, _, k)) if s <= src => k,
                        _ => HlKind::Default,
                    };
                    let color = match overlay_face(&ov_runs, src).0 {
                        _ if block_cursor && cursor_col == Some(col) => rgb(bg), // knocked out of the cursor block
                        Some(k) => rgb(editor.theme.color(k, fg)),
                        None => rgb(editor.theme.color(kind, fg)),
                    };
                    // Weight and slant are per *cell*, unlike the size: they pick
                    // which face rasterises the glyph and change no metric, so
                    // `*bold*` really can be three bold characters in the middle
                    // of a body line while `scale` cannot.
                    let (bold, italic) = overlay_emphasis(&ov_runs, src);
                    self.draw_glyph(ch, x, ty, color, Cut { pct, bold, italic });
                }
                // Images last of the text, over the blanks their substitution
                // reserved. One that started on an earlier display row is
                // skipped rather than clipped — a wrapped line's continuation
                // has no column for it.
                for &(vc, id) in &images {
                    if vc < rs || vc >= re {
                        continue;
                    }
                    let x = lx0 + (vc - rs) as i32 * lb.cw;
                    self.draw_image(editor, id, x, y);
                }
                if let Some(mc) = marker {
                    let c = if block_cursor && cursor_col == Some(mc) {
                        rgb(bg)
                    } else {
                        marker_c
                    };
                    let x = lx0 + mc as i32 * lb.cw;
                    self.draw_glyph(LineOverflow::MARKER, x, ty, c, Cut::plain(pct));
                }
                // The fold indicator, one glyph past the head line's own text on
                // the last row it occupies, in the same accent-tinted shade the
                // truncation marker gets: both are the renderer admitting there
                // is more than it drew. ponytail: on a line that exactly fills
                // the pane it lands on the last column and covers a character,
                // which is the same trade the truncation marker already makes.
                if folds_here && r + 1 == shown_rows && lb.cols > 0 {
                    let col = shown.min(lb.cols - 1);
                    let x = lx0 + col as i32 * lb.cw;
                    self.draw_glyph(FOLD_MARKER, x, ty, marker_c, Cut::plain(pct));
                }

                if !block_cursor {
                    if let Some(cc) = cursor_col {
                        let x = lx0 + cc as i32 * lb.cw;
                        self.draw_cursor(x, y, 2, row_h, pane, cursor_c);
                    }
                }
            }
            row += fits;
            line += 1;
        }
    }

    // --- dashboard --------------------------------------------------------

    /// The startup screen, centred in whichever pane holds the dashboard
    /// buffer — `doc` is that pane's text rectangle, not the window's.
    fn draw_dashboard(&mut self, editor: &Editor, doc: Area) {
        let (bg, fg) = (editor.settings.background, editor.settings.foreground);
        let accent = editor.theme.color(HlKind::Function, fg);
        let banner_c = rgb(editor.theme.color(HlKind::Keyword, fg));
        // Full foreground, not a shade of it. This is the first thing the editor
        // ever shows and the rows *are* the interface — a dimmed menu reads as a
        // disabled one. The hierarchy is carried by the banner's hue and the
        // footer's dimming instead, which is where dimming means something.
        let row_c = rgb(fg);
        let foot_c = rgb(mix(bg, fg, 0.45));
        let sel_bg = rgb(mix(bg, accent, 0.18));

        let lines = editor.dashboard.lines();
        let cols = visible_cols(doc.w, self.cell_w);

        // The logo claims whole rows, so the block below it stays on the text
        // grid and the whole arrangement is still centred as one thing rather
        // than as a picture with a list under it.
        let logo = editor.dashboard.logo.and_then(|id| Some((id, editor.image(id)?)));
        let logo_rows = logo.map_or(0, |(_, im)| {
            (im.height as i32 + self.line_h - 1) / self.line_h.max(1)
        });
        // One blank row between the picture and the text, when there is one.
        let gap = if logo_rows > 0 { 1 } else { 0 };
        let total = (lines.len() as i32 + logo_rows + gap) * self.line_h;
        let y0 = doc.y + ((doc.h - total) / 2).max(0);

        if let Some((id, im)) = logo {
            let x = doc.x + (doc.w - im.width as i32) / 2;
            self.draw_image_at(id, im, x, y0);
        }
        let y0 = y0 + (logo_rows + gap) * self.line_h;

        for (i, (text, selected)) in lines.iter().enumerate() {
            let y = y0 + i as i32 * self.line_h;
            if y + self.line_h > doc.y + doc.h {
                break;
            }
            let n = str_cells(text) as i32;
            let x = doc.x + center_col(text, cols) as i32 * self.cell_w;
            if *selected {
                self.fill(
                    x - self.cell_w,
                    y,
                    (n + 2) * self.cell_w,
                    self.line_h,
                    sel_bg,
                );
            }
            // The footer is the only thing here that is genuinely secondary —
            // it says where the config lives, once, and is not a row you act
            // on. Everything else is at full contrast.
            let color = if *selected {
                rgb(accent)
            } else if is_banner_line(text) {
                banner_c
            } else if i + 1 == lines.len() {
                foot_c
            } else {
                row_c
            };
            self.draw_str(text, x, y, color);
        }
    }

    /// A picture at an exact rectangle, for chrome rather than for a line of a
    /// document.
    ///
    /// [`Renderer::draw_image`] places by the *text baseline* — ascent, depth,
    /// the rows the line claimed — because an inline figure has to sit on the
    /// line it belongs to. The dashboard has no line to sit on, and passing it
    /// through that arithmetic would put the logo wherever a baseline would
    /// have been. Same cache, same texture lifetime; only the placement differs.
    fn draw_image_at(&mut self, id: ImageId, image: &Image, x: i32, y: i32) {
        let (w, h) = (image.width, image.height);
        // Split borrows, as `draw_image` does: the cache needs `&mut images`
        // while uploading needs the texture creator, and blitting needs the
        // canvas.
        let Renderer {
            images,
            textures,
            canvas,
            ..
        } = self;
        let slot = images
            .entry(id)
            .or_insert_with(|| image_texture(textures, image));
        let Some(tex) = slot else { return };
        let _ = canvas.copy(tex, None, Rect::new(x, y, w, h));
        self.mark([id, pack(x, y), pack(w as i32, h as i32), 0]);
        self.draws += 1;
    }

    // --- modeline ---------------------------------------------------------

    /// One window's modeline, drawn into `rect`.
    ///
    /// Takes its rectangle rather than deriving one from the window because
    /// Emacs draws a modeline per *window*, not per frame: this is called once
    /// per pane with the rect the layout hands it. `is_active` is the
    /// mode-line / mode-line-inactive distinction: only the window holding the
    /// cursor gets the bright face.
    ///
    /// The 3D look is Emacs's `:box` attribute, nothing cleverer: a lit edge
    /// along the top and left, a shadowed one along the bottom and right, over
    /// a background lighter than the buffer's. Two tones on opposite edges read
    /// as a light source above-left, which is the whole trick — see [`bevel`]
    /// for the sunken case.
    fn draw_modeline(&mut self, editor: &Editor, buf: &Buffer, rect: Area, is_active: bool) {
        let set = &editor.settings;
        let bg = modeline_bg(editor, is_active);
        let relief = relief(set);

        self.fill(rect.x, rect.y, rect.w, rect.h, rgb(bg));
        let (lit, dark) = (rgb(highlight_shade(bg)), rgb(shadow_shade(bg)));
        for (e, is_lit) in bevel(rect, relief) {
            self.fill(e.x, e.y, e.w, e.h, if is_lit { lit } else { dark });
        }

        // Text sits inside the box and centred in whatever height is left, so
        // changing the relief or the padding moves the whole label with it.
        let inset = bevel_width(rect, relief) + PAD;
        let x = rect.x + inset;
        let y = rect.y + ((rect.h - self.line_h) / 2).max(0);
        let cols = ((rect.w - 2 * inset).max(0) / self.cell_w) as usize;

        // A prompt takes over the active strip, so it gets full contrast and a
        // caret regardless of how dim an inactive modeline would otherwise be.
        // ponytail: it lands on the *focused* pane's modeline, which may be at
        // the top of the window while the popup grows from the bottom. Ceiling:
        // a split with the prompt open. Upgrade path: an echo area of its own,
        // reserved out of `content_area`.
        let prompting = is_active && editor.prompt.is_some();
        let color = rgb(if prompting {
            set.foreground
        } else {
            modeline_fg(editor, is_active)
        });
        // A prompt replaces the whole strip — what is being typed matters more
        // than where the cursor used to be.
        if prompting {
            let end = self.draw_str(&truncate(&editor.status_line(), cols), x, y, color);
            self.fill(end, y, 2, self.line_h, rgb(set.foreground));
            return;
        }

        let (left, right) = modeline::segments(editor, buf, is_active);
        // The right group is placed from the edge inwards, so the position and
        // the mode stay put as the file name and the status message change
        // length. Dropped whole rather than clipped when the pane is too narrow
        // to hold both: half a percentage is worse than none.
        let right_cols: usize = right.iter().map(|s| str_cells(&s.text)).sum();
        let left_budget = cols.saturating_sub(right_cols + 1);
        self.draw_segments(&left, x, y, left_budget, color, editor);
        if right_cols + 1 <= cols {
            let rx = rect.x + rect.w - inset - right_cols as i32 * self.cell_w;
            self.draw_segments(&right, rx, y, right_cols, color, editor);
        }
    }

    /// Draw modeline segments left to right, stopping at `cols`.
    ///
    /// `default` is the strip's own foreground; a segment naming a face takes
    /// that face's colour, which is what lets the theme drive the modeline
    /// through the `set-syntax-color` it already has.
    fn draw_segments(
        &mut self,
        segments: &[modeline::Segment],
        x: i32,
        y: i32,
        cols: usize,
        default: Color,
        editor: &Editor,
    ) {
        let mut x = x;
        let mut left = cols;
        for seg in segments {
            if left == 0 {
                return;
            }
            let text = truncate(&seg.text, left);
            // Cells, matching what `truncate` measured — a character count here
            // would let a segment of combining marks underflow `left`.
            //
            // `saturating_sub` and an assertion rather than a bare `-`, because
            // this subtraction is only safe if `truncate` never answers wider
            // than its budget, and that is a promise made in another function.
            // It was once broken — `str_cells` and `char_cells` disagreed about
            // a tab — and the symptom was the editor *panicking* in a modeline,
            // which is a bad way to learn about an off-by-one. The invariant is
            // restored and tested at both ends; this keeps a future regression a
            // failing test rather than a crash in someone's session.
            let used = str_cells(&text);
            debug_assert!(used <= left, "truncate({:?}, {left}) gave {used}", seg.text);
            left = left.saturating_sub(used);
            let color = match seg.face {
                Some(kind) => rgb(editor.theme.color(kind, editor.settings.foreground)),
                None => default,
            };
            x = self.draw_weighted(&text, x, y, color, seg.bold);
        }
    }

    // --- completion popup -------------------------------------------------

    /// The candidate list for a completing prompt, in whichever of the two
    /// popup styles is configured. A no-op for `Minibuffer`, for `:`/`/` (they
    /// have no candidates) and when no prompt is open — in all three cases the
    /// status strip drawn above is already the whole story.
    ///
    /// One function rather than one per style: the two boxes differ in where
    /// they sit and whether they have a frame, not in how a row is drawn.
    fn draw_completion(&mut self, editor: &Editor, w: i32, h: i32, status_h: i32) {
        let Some(p) = editor.prompt.as_ref() else {
            return;
        };
        let style = editor.settings.completion_style;
        if !p.kind.completes() || style == CompletionStyle::Minibuffer {
            return;
        }

        // One row for "no matches" so the box never collapses to nothing.
        let want = p.matches.len().clamp(1, POPUP_ROWS);
        let framed = style == CompletionStyle::Center;
        let b = if framed {
            center_popup(w, h, status_h, self.line_h, self.cell_w, want)
        } else {
            bottom_popup(w, h, status_h, self.line_h, want)
        };

        let (bg, fg) = (editor.settings.background, editor.settings.foreground);
        let accent = editor.theme.color(HlKind::Function, fg);
        let panel_c = rgb(mix(bg, fg, 0.07));
        let rule_c = rgb(mix(bg, fg, 0.20));
        let border_c = rgb(mix(bg, accent, 0.50));
        let sel_bg = rgb(mix(bg, accent, 0.22));
        let row_c = rgb(mix(bg, fg, 0.78));
        let label_c = rgb(editor.theme.color(HlKind::Keyword, fg));
        let count_c = rgb(mix(bg, fg, 0.45));
        let none_c = rgb(editor.theme.color(HlKind::Comment, fg));

        // Scrim over everything drawn so far, so the popup reads as floating.
        // Tinting *towards the theme background* rather than towards black is
        // what makes this work on a light theme too.
        self.fill(0, 0, w, h, rgba(bg, DIM_ALPHA));
        self.fill(b.x, b.y, b.w, b.h, panel_c);

        let inset = if framed { 1 } else { 0 };
        let x0 = b.x + inset + PAD;
        let cols = ((b.w - 2 * (inset + PAD)).max(0) / self.cell_w) as usize;
        let bottom = b.y + b.h;
        let count = format!("{}/{}", p.matches.len(), p.items.len());

        let mut y = b.y;
        if framed {
            self.fill(b.x, b.y, b.w, 1, border_c);
            self.fill(b.x, bottom - 1, b.w, 1, border_c);
            self.fill(b.x, b.y, 1, b.h, border_c);
            self.fill(b.x + b.w - 1, b.y, 1, b.h, border_c);

            y += PADV;
            self.draw_str(&truncate(title_of(&p.label), cols), x0, y, rgb(accent));
            self.draw_right(&count, b.x + b.w - inset - PAD, y, count_c);
            y += self.line_h + PADV;
            self.fill(b.x + inset, y, b.w - 2 * inset, 1, rule_c);
            y += 1 + PADV;
        } else {
            self.fill(b.x, b.y, b.w, 1, rule_c); // top edge of the panel
            y += PADV;
        }

        // Input line: label, what was typed, caret. `reserve` keeps the typed
        // text from running under the count when there is no title row to put
        // the count on.
        let reserve = if framed { 0 } else { count.chars().count() + 2 };
        let label_cols = p.label.chars().count();
        let text_cols = cols.saturating_sub(label_cols).saturating_sub(reserve);
        // ponytail: the typed text is truncated at its *tail*, so on a very
        // narrow window you stop seeing the characters you just typed. Ceiling:
        // a long path in a 40-column window. Upgrade path: scroll the input line
        // to keep the caret visible, like the candidate list already does.
        let end = self.draw_str(&truncate(&p.label, cols), x0, y, label_c);
        let end = self.draw_str(&truncate(&p.text, text_cols), end, y, rgb(fg));
        self.fill(end, y, 2, self.line_h, rgb(fg));
        if !framed {
            self.draw_right(&count, b.x + b.w - PAD, y, count_c);
        }
        y += self.line_h;

        if framed {
            y += PADV;
            self.fill(b.x + inset, y, b.w - 2 * inset, 1, rule_c);
            y += 1 + PADV;
        }

        // Candidates. `visible` has already scrolled the window to hold the
        // selection, so row 0 here is whatever it decided row 0 is.
        let rows = p.visible(b.rows);
        if rows.is_empty() {
            if b.rows > 0 && y + self.line_h <= bottom {
                self.draw_str("  (no matches)", x0, y, none_c);
            }
            return;
        }
        let text_cols = cols.saturating_sub(2); // the "▸ " / "  " marker
        for (i, &(item, text, selected)) in rows.iter().enumerate() {
            let ry = y + i as i32 * self.line_h;
            if ry + self.line_h > bottom {
                break;
            }
            let c = if selected {
                self.fill(b.x + inset, ry, b.w - 2 * inset, self.line_h, sel_bg);
                self.draw_str("▸", x0, ry, rgb(accent));
                rgb(accent)
            } else {
                row_c
            };
            let shown = truncate(text, text_cols);
            let runs = candidate_runs(editor, p, item, &shown);
            let rx = x0 + 2 * self.cell_w;
            match runs.is_empty() {
                true => {
                    self.draw_str(&shown, rx, ry, c);
                }
                false => self.draw_coloured(&shown, &runs, rx, ry, c, editor),
            }
        }
    }

    /// One candidate row, painted in runs. Anything no run claims keeps `base`,
    /// so a partly-understood row degrades to the plain one rather than to a
    /// half-coloured mess.
    ///
    /// Segments rather than characters because a run is usually the whole of a
    /// token: `draw_str` already advances by cells and handles wide glyphs, and
    /// walking it per character would rasterise the same string a piece at a
    /// time for no gain.
    fn draw_coloured(
        &mut self,
        text: &str,
        runs: &[(usize, usize, HlKind)],
        x: i32,
        y: i32,
        base: Color,
        editor: &Editor,
    ) {
        let fg = editor.settings.foreground;
        let chars: Vec<char> = text.chars().collect();
        let mut x = x;
        let mut at = 0usize;
        let piece = |r: &mut Self, from: usize, to: usize, c: Color, x: &mut i32| {
            if to > from {
                let s: String = chars[from.min(chars.len())..to.min(chars.len())].iter().collect();
                *x = r.draw_str(&s, *x, y, c);
            }
        };
        for &(s, e, kind) in runs {
            let (s, e) = (s.max(at), e.min(chars.len()));
            if e <= s {
                continue;
            }
            piece(self, at, s, base, &mut x); // the gap before this run
            piece(self, s, e, rgb(editor.theme.color(kind, fg)), &mut x);
            at = e;
        }
        piece(self, at, chars.len(), base, &mut x);
    }

    // --- which-key ---------------------------------------------------------

    /// What continues the half-typed key sequence, in a grid above the status
    /// line.
    ///
    /// A sibling of [`Renderer::draw_completion`] and not a reuse of it: the two
    /// share a *shape* — full width, bottom edge on the status strip, one rule
    /// along the top — and nothing else. A completion box has an input line, a
    /// selection and a count; this has none of those, because it is not a
    /// question. It is a hint about a keystroke the keymap is still going to
    /// receive.
    ///
    /// Which is the whole reason it is drawn here rather than assembled in Lisp
    /// out of things Lisp can already draw with. `message` is one line, and a
    /// prompt — the only other multi-row surface — would *swallow* the next key.
    /// Overlays are the other tempting answer and are also wrong: an overlay is
    /// anchored to a range of buffer text and moves when you type in front of
    /// it, and none of this describes the document.
    ///
    /// Yields to an open prompt rather than stacking under it: `M-x` puts a box
    /// in the same place, and two panels arguing over the bottom of the window
    /// is worse than either.
    fn draw_which_key(&mut self, editor: &Editor, w: i32, h: i32, status_h: i32) {
        let items = &editor.which_key;
        if items.is_empty() || editor.prompt.is_some() {
            return;
        }
        let (bg, fg) = (editor.settings.background, editor.settings.foreground);
        let panel_c = rgb(mix(bg, fg, 0.07));
        let rule_c = rgb(mix(bg, fg, 0.20));
        let key_c = rgb(editor.theme.color(HlKind::Keyword, fg));
        let label_c = rgb(mix(bg, fg, 0.78));

        // One document line kept visible above the panel, as the completion box
        // does, so it can never eat the whole window.
        let avail = (h - status_h).max(self.line_h);
        let room = ((avail - self.line_h - 2 * PADV) / self.line_h).max(0) as usize;
        if room == 0 {
            return;
        }
        let widest = items.iter().map(|s| str_cells(s)).max().unwrap_or(0);
        let cols = ((w - 2 * PAD).max(0) / self.cell_w.max(1)) as usize;
        let rows = which_key_rows(widest, items.len(), cols, room);
        let col_w = (widest + WHICH_KEY_GUTTER) as i32 * self.cell_w;

        let box_h = 2 * PADV + rows as i32 * self.line_h;
        let (bx, by) = (0, avail - box_h);
        self.fill(bx, by, w, box_h, panel_c);
        self.fill(bx, by, w, 1, rule_c);

        for (i, item) in items.iter().enumerate() {
            // Column-major: reading a which-key is scanning a column for the key
            // you want, so the alphabet has to run *down* rather than across.
            let (col, row) = (i / rows, i % rows);
            let x = bx + PAD + col as i32 * col_w;
            // Once a column starts past the right edge every later one does too,
            // so this is a `break`. ponytail: the entries in it are simply not
            // drawn — Lisp has already capped the list at `*which-key-limit*`
            // and said in the status line how many it left out.
            if x + col_w > w - PAD && col > 0 {
                break;
            }
            let y = by + PADV + row as i32 * self.line_h;
            let (k, label) = item.split_once(' ').unwrap_or((item, ""));
            let end = self.draw_str(k, x, y, key_c);
            let left = ((w - PAD - end).max(0) / self.cell_w.max(1)) as usize;
            self.draw_str(&truncate(label, left), end + self.cell_w, y, label_c);
        }
    }

    // --- primitives -------------------------------------------------------

    /// Fold one draw call's parameters into this frame's digest. FNV-1a: an xor
    /// and a multiply per word, four words per call. A full screen is three or
    /// four thousand calls, so the whole frame's digest costs tens of
    /// microseconds against a 16 ms budget — and buys back a 16 ms present.
    ///
    /// It does not have to be collision-proof, only cheap and total: every
    /// parameter that reaches the canvas goes in, so two frames with the same
    /// digest drew the same picture short of a 64-bit accident.
    #[inline]
    fn mark(&mut self, words: [u64; 4]) {
        let mut h = self.drawn;
        for w in words {
            h = (h ^ w).wrapping_mul(FNV_PRIME);
        }
        self.drawn = h;
    }

    /// Confine every following draw to `a`. Nothing else in the renderer knows
    /// about pane boundaries — this is what keeps a long line, a wide selection
    /// or an overhanging glyph inside the window it belongs to.
    fn set_clip(&mut self, a: Area) {
        let r = Rect::new(a.x, a.y, a.w.max(0) as u32, a.h.max(0) as u32);
        self.canvas.set_clip_rect(Some(r));
        // In the digest even though it draws nothing: it decides what the calls
        // after it are *allowed* to draw, so two frames whose panes moved can
        // otherwise issue the identical list of blits.
        self.mark([TAG_CLIP, pack(a.x, a.y), pack(a.w, a.h), 0]);
    }

    fn clear_clip(&mut self) {
        self.canvas.set_clip_rect(None);
        self.mark([TAG_CLIP, 0, 0, 1]);
    }

    /// A cursor rect, trimmed to `pane`. The cursor legitimately sits one cell
    /// *past* the last column — end of line in insert mode, see [`cursor_pos`] —
    /// and a whole cell there overhangs the neighbouring pane. The clip rect
    /// would hide it; trimming means we never ask SDL to draw it at all.
    /// Draw a terminal's cell grid.
    ///
    /// Deliberately not the text path. A terminal carries a colour per cell and
    /// its own cursor, and both are gone by the time the grid has been
    /// flattened into the buffer's rope — the rope is what the buffer switcher
    /// and `buffer-string` read, this is what the eye reads.
    ///
    /// Rows past the bottom of the pane are dropped rather than scrolled: the
    /// grid is sized to the pane by `Term::sync`, so an overflowing row means a
    /// resize is one frame behind, not that there is anything more to show.
    fn draw_terminal(&mut self, screen: &Screen, doc: Area, bg: Color, cursor: Color) {
        for row in 0..screen.rows {
            let y = doc.y + row as i32 * self.line_h;
            if y + self.line_h > doc.y + doc.h {
                break;
            }
            for col in 0..screen.cols {
                let x = doc.x + col as i32 * self.cell_w;
                if x + self.cell_w > doc.x + doc.w {
                    break;
                }
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                let cell_bg = Color::RGB(cell.bg[0], cell.bg[1], cell.bg[2]);
                // Only when it differs from the pane's own background: an 80×24
                // grid is 1920 fills a frame, nearly all of them the same colour
                // the pane was already cleared to.
                if cell_bg != bg {
                    self.fill(x, y, self.cell_w, self.line_h, cell_bg);
                }
                self.draw_char(cell.c, x, y, Color::RGB(cell.fg[0], cell.fg[1], cell.fg[2]));
                if cell.underline {
                    self.fill(x, y + self.line_h - 1, self.cell_w, 1, cell_bg);
                }
            }
        }

        // The block cursor last, so it sits over the character it is on and
        // that character is redrawn in the background colour to stay legible.
        if let Some((row, col)) = screen.cursor {
            let (x, y) = (
                doc.x + col as i32 * self.cell_w,
                doc.y + row as i32 * self.line_h,
            );
            if x + self.cell_w <= doc.x + doc.w && y + self.line_h <= doc.y + doc.h {
                self.fill(x, y, self.cell_w, self.line_h, cursor);
                if let Some(cell) = screen.cell(row, col) {
                    self.draw_char(cell.c, x, y, bg);
                }
            }
        }
    }

    /// Take the keyboard. macOS does not reliably send `FocusGained` for a
    /// window the application opened itself, so a new frame has to ask.
    pub fn focus(&mut self) {
        self.canvas.window_mut().raise();
    }

    /// Advance and line height in pixels — what the app divides a pane by to
    /// tell the shell how many columns and rows it has.
    pub fn cell_size(&self) -> (i32, i32) {
        (self.cell_w, self.line_h)
    }

    /// `h` rather than [`Renderer::line_h`] because a line an overlay has scaled
    /// owns several rows, and a cursor one row tall on a heading looks like a
    /// cursor on the line above it.
    fn draw_cursor(&mut self, x: i32, y: i32, w: i32, h: i32, pane: Area, color: Color) {
        self.fill(x, y, w.min(pane.x + pane.w - x), h, color);
    }

    /// Blit an overlay image into the row whose top-left is `(x, y)`.
    ///
    /// Uploaded once and kept — the id is a hash of the job that produced the
    /// pixels, so previewing the same equation again is a cache hit rather than
    /// a second texture. Straight (unpremultiplied) alpha, which is what
    /// [`BlendMode::Blend`] expects and what `zemacs-latex` promises.
    ///
    /// Vertical placement is the whole point of `Image::depth`, and there is now
    /// one rule for every size: **the image's baseline lands on the baseline of
    /// the last row it needs**. `$x_1$` is shorter than the ascent, needs one
    /// row, and so hangs its `depth` pixels below the text baseline exactly as
    /// the surrounding descenders do. A display equation needs several, and
    /// claims them *upward* from that baseline — which is safe only because
    /// [`Renderer::draw_document`] has grown the line it starts on to hold them,
    /// so upward means into rows the fragment owns rather than over the text
    /// above. Before rows could vary in height this had to hang from the top of
    /// its single row and hope the fragment's own blank source lines caught it.
    ///
    /// ponytail: no *horizontal* reflow. Nothing to the right of an image moves,
    /// so an image wider than the cells its substitution reserved paints over
    /// the text after it — which is only ever a rounding cell, since the
    /// reservation is `ceil(width / cell_w)`. The clip rect keeps it inside the
    /// pane. Upgrade path is a per-line table of cell advances.
    fn draw_image(&mut self, editor: &Editor, id: ImageId, x: i32, y: i32) {
        let Some(image) = editor.image(id) else {
            return;
        };
        let (w, h, depth) = (image.width, image.height, image.depth as i32);
        let rows = image_rows(h, image.depth, self.line_h, self.ascent) as i32;
        let iy = y + (rows - 1) * self.line_h + self.ascent + depth - h as i32;
        self.blit_image(id, image, x, iy, w, h);
    }

    /// Upload if this is the first sighting, then blit into `(x, y, w, h)`.
    ///
    /// The whole of the texture cache's use, shared by the cell grid above and
    /// by [`Renderer::draw_scene`] below. Where the box comes from is the
    /// caller's business — a grid works it out from `line_h` and `ascent`, a
    /// scene was handed it by the layout engine — but the cache, the failure
    /// contract and the digest fold are one thing and belong in one place.
    fn blit_image(&mut self, id: ImageId, image: &Image, x: i32, y: i32, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return; // SDL rejects an empty destination and there is nothing to see
        }
        // Split borrows: the cache needs `&mut images` while uploading needs the
        // creator, and blitting needs `&mut canvas`.
        let Renderer {
            images,
            textures,
            canvas,
            ..
        } = self;
        let slot = images
            .entry(id)
            .or_insert_with(|| image_texture(textures, image));
        let Some(tex) = slot else { return };
        let _ = canvas.copy(tex, None, Rect::new(x, y, w, h));
        self.mark([id, pack(x, y), pack(w as i32, h as i32), 0]);
        self.draws += 1;
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
        if w <= 0 || h <= 0 {
            return;
        }
        self.canvas.set_draw_color(color);
        let _ = self.canvas.fill_rect(Rect::new(x, y, w as u32, h as u32));
        self.mark([TAG_FILL, pack(x, y), pack(w, h), bits(color)]);
        self.draws += 1;
    }

    /// Returns the x just past the last cell, so callers can chain / place a bar.
    fn draw_str(&mut self, s: &str, x: i32, y: i32, color: Color) -> i32 {
        self.draw_weighted(s, x, y, color, false)
    }

    /// `draw_str`, optionally in the bold face. Returns the x just past the
    /// last cell. Bold is *metrically* the same — the cell width never changes,
    /// so a bold run cannot shift the columns after it.
    ///
    /// A wide character advances two cells and a combining mark none, exactly as
    /// in the document: a buffer named `日本語.txt` has to measure the same on the
    /// modeline as it does in the switcher, or one of them wraps early.
    fn draw_weighted(&mut self, s: &str, x: i32, y: i32, color: Color, bold: bool) -> i32 {
        let mut x = x;
        for c in s.chars() {
            if bold {
                self.draw_bold_char(c, x, y, color);
            } else {
                self.draw_char(c, x, y, color);
            }
            x += char_cells(c) as i32 * self.cell_w;
        }
        x
    }

    fn draw_bold_char(&mut self, c: char, x: i32, y: i32, color: Color) {
        if c == ' ' || c == '\t' {
            return;
        }
        let Renderer {
            bold_glyphs,
            bold,
            textures,
            canvas,
            ..
        } = self;
        let slot = bold_glyphs
            .entry(c)
            .or_insert_with(|| glyph_texture(textures, bold, c));
        if let Some(tex) = slot {
            tex.set_color_mod(color.r, color.g, color.b);
            let q = tex.query();
            let _ = canvas.copy(tex, None, Rect::new(x, y, q.width, q.height));
            // The glyph's size is a function of the face and the point size, and
            // the point size is already in the frame seed, so the character and
            // the weight identify the blit.
            self.mark([TAG_BOLD ^ u64::from(c), pack(x, y), 0, bits(color)]);
            self.draws += 1;
        }
    }

    /// `draw_str`, but `x` is where the text should *end*. Same glyph path.
    fn draw_right(&mut self, s: &str, x: i32, y: i32, color: Color) {
        let w = str_cells(s) as i32 * self.cell_w;
        self.draw_str(s, x - w, y, color);
    }

    fn draw_char(&mut self, c: char, x: i32, y: i32, color: Color) {
        if c == ' ' || c == '\t' {
            return;
        }
        // Split borrows: the cache needs `&mut glyphs` while rasterising needs
        // `&font`, and blitting needs `&mut canvas`.
        let Renderer {
            glyphs,
            font,
            textures,
            canvas,
            ..
        } = self;
        let slot = glyphs
            .entry(c)
            .or_insert_with(|| glyph_texture(textures, font, c));
        if let Some(tex) = slot {
            tex.set_color_mod(color.r, color.g, color.b);
            let q = tex.query();
            let _ = canvas.copy(tex, None, Rect::new(x, y, q.width, q.height));
            self.mark([u64::from(c), pack(x, y), 0, bits(color)]);
            self.draws += 1;
        }
    }

    // --- typeset text -----------------------------------------------------

    /// One glyph in whatever face `(pct, bold, italic)` names.
    ///
    /// A three-way split rather than one lookup, and it is worth the extra arm:
    /// body-weight and bold body text is nearly every character the editor ever
    /// draws, and those two keep the direct field access and the flat
    /// `HashMap<char, _>` they have always had. Only the rest — anything an
    /// overlay's `scale` asked for, and italic at any size — pays a second hash
    /// to find its face first. The fast path is unchanged, which also means the
    /// glyph cache with the interesting lifetime is unchanged.
    fn draw_glyph(&mut self, c: char, x: i32, y: i32, color: Color, cut: Cut) {
        match (cut.pct, cut.bold, cut.italic) {
            (100, false, false) => self.draw_char(c, x, y, color),
            (100, true, false) => self.draw_bold_char(c, x, y, color),
            _ => self.draw_styled_char(c, x, y, color, cut),
        }
    }

    /// [`Renderer::draw_weighted`] for a typeset run: draws `s` at `pct` of body
    /// size and answers the x just past it.
    ///
    /// Advances by the *arithmetic* cell width rather than by the face's own,
    /// for the same reason bold does not measure itself: layout is a grid, and a
    /// run that measured itself would put the column its neighbour starts in out
    /// of the renderer's hands. A face opened at 150% of the point size has an
    /// advance within a pixel of 150% of the cell, so the visible effect is
    /// slightly loose or tight tracking on a scaled line — never a wrong column.
    fn draw_run(&mut self, s: &str, x: i32, y: i32, color: Color, cut: Cut) -> i32 {
        let cw = scaled(self.cell_w, cut.pct);
        let mut x = x;
        for c in s.chars() {
            self.draw_glyph(c, x, y, color, cut);
            x += char_cells(c) as i32 * cw;
        }
        x
    }

    /// A glyph from the on-demand face map. See [`Renderer::faces`] for the
    /// bound; this is where both halves of it are enforced.
    fn draw_styled_char(&mut self, c: char, x: i32, y: i32, color: Color, cut: Cut) {
        if c == ' ' || c == '\t' {
            return;
        }
        let key = face_key(self.point_size, cut);
        let Renderer {
            faces,
            ttf,
            font_path,
            textures,
            canvas,
            ..
        } = self;
        let Some(face) = cached_face(faces, ttf, font_path, key) else {
            return;
        };
        // Emptied rather than evicted one by one: there is no access order to
        // evict by without keeping one, and a face that has drawn this many
        // distinct characters is a document whose repertoire is the whole cache
        // anyway. Costs a re-rasterisation of what is on screen, once.
        if face.glyphs.len() >= MAX_FACE_GLYPHS && !face.glyphs.contains_key(&c) {
            face.glyphs.clear();
        }
        let slot = face
            .glyphs
            .entry(c)
            .or_insert_with(|| glyph_texture(textures, &face.font, c));
        if let Some(tex) = slot {
            tex.set_color_mod(color.r, color.g, color.b);
            let q = tex.query();
            let _ = canvas.copy(tex, None, Rect::new(x, y, q.width, q.height));
            // The size is *not* implied by the frame seed the way the body
            // face's is, so it goes in: two frames differing only in a heading's
            // scale would otherwise hash the same and never be presented.
            self.mark([
                TAG_STYLED ^ u64::from(c),
                pack(x, y),
                pack(i32::from(key.point_size), i32::from(key.style)),
                bits(color),
            ]);
            self.draws += 1;
        }
    }
}

/// A *cut* of the typeface: one size, one weight, one slant — the three things
/// that together decide which font handle rasterises a glyph.
///
/// One value rather than three arguments because it is one decision. The draw
/// loop resolves it per cell (the size from the line, the weight and slant from
/// the overlays covering that cell) and hands it straight down; nothing in
/// between has any business taking the three apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cut {
    /// Percent of the body size, already snapped to a [`SCALE_STEPS`] entry.
    pct: u16,
    bold: bool,
    italic: bool,
}

impl Cut {
    /// The body face at `pct` — what chrome and a prefix are drawn in.
    fn plain(pct: u16) -> Self {
        Self {
            pct,
            bold: false,
            italic: false,
        }
    }
}

/// One font opened at one size and style, with the glyphs it has rasterised.
///
/// Bundled rather than two parallel maps because they die together: a texture
/// cached from one `Font` is meaningless against another, so anything that drops
/// the font must drop the glyphs in the same move.
struct Face {
    font: Font<'static, 'static>,
    glyphs: HashMap<char, Option<Texture<'static>>>,
}

/// What identifies a [`Face`]: an absolute point size — already through the
/// display's scale factor and the overlay's percentage — and the two style bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct FaceKey {
    point_size: u16,
    /// Bit 0 bold, bit 1 italic.
    style: u8,
}

/// The face a [`Cut`] names, given the point size the body is open at.
///
/// The one place a percentage becomes a point size, and it has to stay the one
/// place: a scene *measures* through this and then *draws* through it again, and
/// a second copy of the arithmetic that rounded differently would lay text out
/// in one face and paint it in another.
///
/// The clamp is SDL_ttf's own range and not policy — a zero or negative point
/// size is a font it will not open, so the value that reaches [`open_face`] must
/// already be one it can.
fn face_key(body_point_size: u16, cut: Cut) -> FaceKey {
    FaceKey {
        point_size: scaled(i32::from(body_point_size), cut.pct).clamp(4, 400) as u16,
        style: u8::from(cut.bold) | (u8::from(cut.italic) << 1),
    }
}

/// The face `key` names, opening it on first sighting. `None` is a face that
/// would not open — see [`open_face`], which caches that failure.
///
/// Both halves of the bound documented on [`Renderer::faces`] are enforced here
/// and nowhere else, so a second caller (a scene's face pre-pass) cannot grow
/// the cache past what a glyph draw would have.
fn cached_face<'a>(
    faces: &'a mut HashMap<FaceKey, Option<Face>>,
    ttf: &'static Sdl2TtfContext,
    path: &std::path::Path,
    key: FaceKey,
) -> Option<&'a mut Face> {
    // The key space is closed, so this can only fire if `SCALE_STEPS` grew
    // without `MAX_FACES` growing with it. Clearing rather than refusing to
    // draw keeps that a performance bug instead of a rendering one.
    if faces.len() >= MAX_FACES && !faces.contains_key(&key) {
        faces.clear();
    }
    faces
        .entry(key)
        .or_insert_with(|| open_face(ttf, path, key))
        .as_mut()
}

/// Open faces [`Renderer::faces`] will hold. `SCALE_STEPS.len() * 4`, which is
/// the whole key space, so reaching it means the steps grew and this did not.
const MAX_FACES: usize = SCALE_STEPS.len() * 4;

/// Glyph textures one [`Face`] will cache. At two hundred and fifty-six
/// characters and a few kilobytes each this is a few megabytes across every
/// face that could exist, and a config using `scale` for headings opens two or
/// three of them holding a few dozen glyphs apiece.
const MAX_FACE_GLYPHS: usize = 256;

/// A font at one size and style, or `None` if it will not open — cached as a
/// failure so a missing file is not re-`stat`ed sixty times a second.
fn open_face(
    ttf: &'static Sdl2TtfContext,
    path: &std::path::Path,
    key: FaceKey,
) -> Option<Face> {
    let mut font = ttf.load_font(path, key.point_size).ok()?;
    set_hinting(&mut font);
    // Synthetic, both of them: FreeType smears the outline for bold and shears
    // it for italic rather than loading designed faces, which is what keeps the
    // advance the grid's and not the font's. A designed italic would need a
    // second file, a second search path, and a per-family table — and would
    // still be drawn on the same monospace grid.
    let mut style = sdl2::ttf::FontStyle::NORMAL;
    if key.style & 1 != 0 {
        style |= sdl2::ttf::FontStyle::BOLD;
    }
    if key.style & 2 != 0 {
        style |= sdl2::ttf::FontStyle::ITALIC;
    }
    font.set_style(style);
    Some(Face {
        font,
        glyphs: HashMap::new(),
    })
}

/// FNV-1a's constants, and tags that keep one kind of draw call from colliding
/// with another. The character tags are the code points themselves — every
/// scalar value is below `TAG_BOLD`, so nothing overlaps.
const FNV_SEED: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const TAG_BOLD: u64 = 1 << 24;
const TAG_FRAME: u64 = 1 << 25;
const TAG_CLEAR: u64 = 1 << 26;
const TAG_FILL: u64 = 1 << 27;
const TAG_CLIP: u64 = 1 << 28;
const TAG_STYLED: u64 = 1 << 29;
const TAG_SCENE: u64 = 1 << 30;

/// Two coordinates in one word, so a draw call is four folds rather than six.
#[inline]
fn pack(a: i32, b: i32) -> u64 {
    (u64::from(a as u32) << 32) | u64::from(b as u32)
}

#[inline]
fn bits(c: Color) -> u64 {
    u64::from(u32::from_be_bytes([c.r, c.g, c.b, c.a]))
}

/// `None` when the font has no glyph for `c` — cached so we don't retry it.
/// How much to darken glyph stems. 1.0 is off; higher is heavier.
///
/// FreeType hands back linear coverage, and blending that straight onto a dark
/// background is what makes light-on-dark text look thin and washed out next to
/// the same font in a native macOS application. CoreText applies a gamma to the
/// coverage before blending — this is that, and it is the single biggest reason
/// text here did not look like text in Emacs.
const STEM_GAMMA: f32 = 1.45;

fn glyph_texture(
    textures: &'static TextureCreator<WindowContext>,
    font: &Font,
    c: char,
) -> Option<Texture<'static>> {
    // Render as a one-char *string* rather than via `render_char`: the string
    // path applies the glyph's left bearing for us, so blitting at the cell
    // origin lines up.
    let mut buf = [0u8; 4];
    let surface = font.render(c.encode_utf8(&mut buf)).blended(Color::WHITE).ok()?;
    // A known layout to walk: `blended` picks its own, and the alpha byte is
    // not in the same place in all of them.
    let mut surface = surface.convert_format(PixelFormatEnum::ARGB8888).ok()?;
    darken_stems(&mut surface);

    let mut tex = textures.create_texture_from_surface(&surface).ok()?;
    tex.set_blend_mode(BlendMode::Blend);
    Some(tex)
}

/// Upload an overlay bitmap. `None` records a failure so it is not retried on
/// every frame — the same contract [`glyph_texture`] has for a missing glyph.
///
/// `ABGR8888` is SDL's spelling of *byte order* `R G B A` on a little-endian
/// machine (it is what `SDL_PIXELFORMAT_RGBA32` expands to there), which is the
/// layout `zemacs_latex::Preview` documents. No `darken_stems`: dvipng already
/// antialiased against the colour it was given, and a gamma meant for a font
/// rasteriser's linear coverage would only smear an equation.
fn image_texture(
    textures: &'static TextureCreator<WindowContext>,
    image: &zemacs_core::Image,
) -> Option<Texture<'static>> {
    // `Surface::from_data` borrows, and it wants `&mut` even to read — the copy
    // is one frame's worth of pixels once per image, never per frame.
    let mut pixels = image.rgba.clone();
    let surface = Surface::from_data(
        &mut pixels,
        image.width,
        image.height,
        image.width * 4,
        PixelFormatEnum::ABGR8888,
    )
    .ok()?;
    let mut tex = textures.create_texture_from_surface(&surface).ok()?;
    tex.set_blend_mode(BlendMode::Blend);
    Some(tex)
}

/// Apply [`STEM_GAMMA`] to a glyph's coverage.
///
/// Done once per glyph, on the way into the cache, so this costs nothing per
/// frame. Fully opaque and fully transparent pixels are already correct and are
/// left alone; it is the partial coverage along a stem's edge that is too light.
fn darken_stems(surface: &mut Surface) {
    // A 256-entry table beats a `powf` per pixel, and a glyph is thousands of
    // pixels.
    static CURVE: OnceLock<[u8; 256]> = OnceLock::new();
    let curve = CURVE.get_or_init(|| {
        let mut table = [0u8; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let linear = i as f32 / 255.0;
            *slot = (linear.powf(1.0 / STEM_GAMMA) * 255.0).round() as u8;
        }
        table
    });
    let _ = surface.with_lock_mut(|pixels: &mut [u8]| {
        // ARGB8888 is little-endian in memory, so the alpha byte is the last of
        // each four.
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = curve[pixel[3] as usize];
        }
    });
}

/// macOS does not hint at all — CoreText positions glyphs on the real outline
/// and lets the resolution carry it. FreeType's default is full hinting, which
/// snaps stems to the pixel grid and is why the same font looks subtly wrong
/// here next to a native application. Light hints vertically only, which keeps
/// the baseline crisp without distorting letterforms sideways.
/// The same file, emboldened.
///
/// SDL_ttf's `BOLD` is synthetic — FreeType smears the outline rather than
/// loading a designed bold — which is exactly what is wanted here: the advance
/// is unchanged, so a bold run occupies the same cells as a plain one and the
/// modeline's columns do not move when the mode name changes.
fn open_bold(
    ttf: &'static Sdl2TtfContext,
    path: &std::path::Path,
    point_size: u16,
) -> anyhow::Result<Font<'static, 'static>> {
    let mut bold = ttf
        .load_font(path, point_size)
        .map_err(|e| anyhow::anyhow!("cannot open font {}: {e}", path.display()))?;
    set_hinting(&mut bold);
    bold.set_style(sdl2::ttf::FontStyle::BOLD);
    Ok(bold)
}

fn set_hinting(font: &mut Font) {
    font.set_hinting(Hinting::Light);
}

fn metrics(font: &Font) -> (i32, i32) {
    let cell_w = font
        .find_glyph_metrics('M')
        .map(|m| m.advance)
        .or_else(|| font.size_of_char('M').ok().map(|(w, _)| w as i32))
        .unwrap_or(8)
        .max(1);
    (cell_w, font.recommended_line_spacing().max(1))
}

/// Retina windows report a drawable larger than the window; render at that
/// scale so text is sharp instead of upscaled.
fn dpi_scale(canvas: &WindowCanvas) -> f32 {
    let win = canvas.window();
    let (logical, _) = win.size();
    let (drawable, _) = win.drawable_size();
    if logical == 0 {
        1.0
    } else {
        (drawable as f32 / logical as f32).max(1.0)
    }
}

fn scale_point_size(font_size: f32, scale: f32) -> u16 {
    (font_size * scale).round().clamp(4.0, 400.0) as u16
}

fn find_font() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("ZEMACS_FONT") {
        let p = PathBuf::from(p);
        anyhow::ensure!(p.is_file(), "$ZEMACS_FONT is not a file: {}", p.display());
        return Ok(p);
    }
    FONT_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no monospace font found. Tried:\n  {}\nSet $ZEMACS_FONT to a .ttf/.ttc to override.",
                FONT_CANDIDATES.join("\n  ")
            )
        })
}

// --- pure layout helpers (unit-tested; no window required) -----------------

fn rgb(c: [f32; 3]) -> Color {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color::RGB(b(c[0]), b(c[1]), b(c[2]))
}

/// `rgb` with an alpha channel, for the scrim behind a popup. The canvas is in
/// `BlendMode::Blend`, so this is the only translucent thing in the renderer.
fn rgba(c: [f32; 3], a: u8) -> Color {
    let Color { r, g, b, .. } = rgb(c);
    Color::RGBA(r, g, b, a)
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// A pixel rectangle. Not `sdl2::rect::Rect`: that one stores its size
/// unsigned, and every bit of layout below wants to subtract freely and clamp
/// once at the end. [`Renderer::fill`] drops non-positive rects anyway.
///
/// Structurally identical to [`zemacs_core::Rect`], which is the layout's
/// currency; [`area_of`] and [`area_rect`] convert. Keeping the local type
/// means the popup and bevel maths below stay untouched by the window tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Area {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

fn area_of(r: zemacs_core::Rect) -> Area {
    Area {
        x: r.x,
        y: r.y,
        w: r.w,
        h: r.h,
    }
}

fn area_rect(a: Area) -> zemacs_core::Rect {
    zemacs_core::Rect::new(a.x, a.y, a.w, a.h)
}

/// A logical (mouse) coordinate in drawable pixels. See
/// [`Renderer::to_pixels`]; split out so it is testable without a window.
fn scale_point(scale: f32, x: i32, y: i32) -> (i32, i32) {
    let at = |v: i32| (v as f32 * scale).round() as i32;
    (at(x), at(y))
}

/// The text rectangle inside a pane: everything above its modeline, inset by
/// [`PAD`] and, when `measure` is positive, narrowed to that many pixels and
/// centred in what is left.
///
/// Returned as the *text* rectangle rather than the whole remainder so the row
/// count and the drawing loop cannot disagree about the inset.
///
/// **The measure is applied here and nowhere else**, and that is the whole
/// design of it. `visible_cols`, the wrap points, the block cursor, the
/// relative gutter and the click-to-offset map in [`offset_at`] all take their
/// geometry from this rectangle, so narrowing it moves every one of them
/// together. Centring by shifting the glyphs at draw time instead would have
/// left the mouse and the wrap points reading the pane's full width — which is
/// the bug `olivetti-mode` spent years having.
///
/// The modeline is deliberately *not* narrowed: it belongs to the window rather
/// than to the page, and a status bar floating in the middle of a pane reads as
/// a second document rather than as a frame around the first.
fn doc_rect(pane: Area, status_h: i32, measure: i32) -> Area {
    let modeline = modeline_rect(pane, status_h);
    let avail = (pane.w - 2 * PAD).max(0);
    // A measure wider than the pane is not an error and must not become a
    // negative inset — it simply means the pane is the narrower constraint.
    let w = match measure {
        m if m > 0 => m.min(avail),
        _ => avail,
    };
    Area {
        x: pane.x + PAD + (avail - w) / 2,
        y: pane.y + PAD,
        w,
        h: (pane.h.max(0) - modeline.h - PAD).max(0),
    }
}

/// The measure in pixels, from the setting's columns and the font's cell.
///
/// Columns rather than pixels is the setting's whole point — see
/// [`Settings::text_width`] — so this is the one place the two units meet, and
/// every caller of [`doc_rect`] that draws a *document* goes through it.
fn measure_px(set: &Settings, cell_w: i32) -> i32 {
    set.text_width as i32 * cell_w.max(1)
}

/// Colour runs for one candidate row, as `(start, end, kind)` in **char**
/// offsets into the row as drawn. Empty means "nothing to say about this one",
/// and the row is drawn flat.
///
/// The interesting case is `consult-line`, and it is interesting because it
/// costs nothing: those candidates are lines of a buffer that is *already*
/// highlighted, so a row is one of that buffer's lines shifted right by the
/// width of the number in front of it. No parser runs, no grammar is loaded,
/// and the colours in the popup are by construction the same ones the text has
/// behind it — which is the point. Re-highlighting each visible row per
/// keystroke would have been a tree-sitter parse per row per frame, and it
/// would have got single lines torn out of context wrong besides.
///
/// A grep hit comes from a file nobody has opened, so there are no spans to
/// borrow and this colours the structure instead — path, line number, text —
/// which is what `consult-ripgrep` itself shows and most of what the eye is
/// using to scan the list anyway.
///
/// ponytail: a grep hit's *code* stays the row colour. Highlighting it would
/// mean this crate depending on tree-sitter, which the layering deliberately
/// avoids (see the note on [`Span`] in core). The upgrade path, if it ever
/// matters, is for the app — which already links the highlighter — to attach
/// spans to the candidates it pushes.
fn candidate_runs(
    editor: &Editor,
    p: &zemacs_core::Prompt,
    item: usize,
    shown: &str,
) -> Vec<(usize, usize, HlKind)> {
    match p.kind {
        zemacs_core::PromptKind::Line => {
            let buf = &editor.buffer;
            if item >= buf.len_lines() {
                return Vec::new();
            }
            let (start, end) = (buf.line_start(item), buf.line_end(item));
            // The number in front is chrome, not content — dimmed, the same
            // way the document's own gutter is.
            let mut runs = vec![(0, p.prefix, HlKind::Comment)];
            runs.extend(
                buf.highlights
                    .iter()
                    .filter(|s| s.end > start && s.start < end)
                    .map(|s| {
                        (
                            s.start.max(start) - start + p.prefix,
                            s.end.min(end) - start + p.prefix,
                            s.kind,
                        )
                    }),
            );
            runs
        }
        // `path:line:text`, split from the left twice — the same two colons
        // the app splits on to open the hit, so what reads as a path is what
        // will be opened as one.
        zemacs_core::PromptKind::Grep => {
            let mut colons = shown
                .chars()
                .enumerate()
                .filter(|&(_, c)| c == ':')
                .map(|(i, _)| i);
            match (colons.next(), colons.next()) {
                (Some(a), Some(b)) => vec![
                    (0, a, HlKind::Function),
                    (a, a + 1, HlKind::Punctuation),
                    (a + 1, b, HlKind::Number),
                    (b, b + 1, HlKind::Punctuation),
                ],
                _ => Vec::new(),
            }
        }
        // The mode after the name is chrome, the same way the `Line` prompt's
        // numbers are — dimmed, so the eye reads a column of buffer names and
        // only glances right. The gap is the last run of spaces, which is where
        // the padding core added ends; a truncated row simply loses the tail and
        // falls back to one colour.
        zemacs_core::PromptKind::Buffer => match shown.rfind("  ") {
            Some(i) => vec![(
                shown[..i].chars().count(),
                shown.chars().count(),
                HlKind::Comment,
            )],
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The buffer offset a pixel in `pane` points at.
///
/// The pure half of [`Renderer::click_target`], split out so it can be tested
/// against a real buffer without opening a window — the wrapped, folded and
/// wide-character cases are exactly the ones worth a test and exactly the ones
/// a hand-run of the app is worst at checking.
///
/// The loop is the drawing loop's twin and has to stay one: rows are spent on
/// lines in the same order, a folded line is skipped without spending a row,
/// and a wrapped line spends the same number the draw pass gives it. Anything
/// else and the click lands on a character other than the one under the
/// pointer.
#[allow(clippy::too_many_arguments)]
fn offset_at(
    buf: &Buffer,
    win: &Window,
    set: &Settings,
    pane: Area,
    status_h: i32,
    line_h: i32,
    cell_w: i32,
    x: i32,
    y: i32,
) -> usize {
    let doc = doc_rect(pane, status_h, measure_px(set, cell_w));
    let gutter = gutter_w(buf, set, cell_w);
    let text_w = doc.w - gutter;
    let wrap = set.line_overflow == LineOverflow::Wrap;
    let rows = doc_lines(pane, status_h, line_h);

    // Clamped rather than rejected: a click in the gutter means column zero of
    // that row, and one past the right edge means end of line — both are what
    // every other editor does with the same gesture.
    //
    // A *pixel* offset rather than a column, because which column that is now
    // depends on the line: a heading's cells are wider than the body's and its
    // prefix has already eaten some of the front. Divided per line below.
    let want_row = ((y - doc.y).max(0) / line_h.max(1)) as usize;
    let want_x = (x - doc.x - gutter).max(0);

    let (mut row, mut line) = (0usize, win.scroll);
    while row < rows && line < buf.len_lines() {
        let start = buf.line_start(line);
        if fold_hiding(buf.overlays(), start).is_some() {
            line += 1; // spans a line, occupies no row — as in the draw loop
            continue;
        }
        let b = line_box(
            &line_style(buf.overlays(), start, start + buf.line_len(line)),
            text_w,
            cell_w,
        );
        let cells = line_cells(buf, line, set.tab_width);
        let visual = if wrap {
            wrap_row_count(cells.len(), b.cols)
        } else {
            1
        };
        let height = visual * b.tall;
        if want_row < row + height {
            // Clamped at `cols` on the right, and it is the margin outside a
            // centred measure that made it matter: without it, a pointer out in
            // the white space answered whatever character the *untruncated* line
            // has that far along — a character nothing on screen is showing.
            // `cols` and not `cols - 1` so that "one cell past the last
            // character", where the cursor may legitimately sit at end of line,
            // still lands there.
            let want_col = (((want_x - b.indent).max(0) / b.cw.max(1)) as usize).min(b.cols);
            // The cell the pointer is over, then the *source* character that
            // cell was expanded from: a tab is one character across several
            // cells and 漢 is one character across two, so this last step is
            // the whole reason clicking a wide glyph lands on it rather than
            // beside it.
            let cell = ((want_row - row) / b.tall.max(1)) * b.cols + want_col;
            let src = match cells.get(cell) {
                Some(&(_, i)) => i,
                // Past the end of the line: the position after the last
                // character, which is where the cursor may sit and no further.
                None => cells.last().map_or(0, |&(_, i)| i + 1),
            };
            return start + src;
        }
        row += height;
        line += 1;
    }
    // Below the last line. Emacs puts point at the end of the buffer, and so
    // does a click in the empty space under a short file.
    buf.len_chars()
}

/// Document lines that fit in a pane — the window's `viewport_lines`.
///
/// Zero for a pane too short to show one: the modeline is still drawn, the text
/// just has nowhere to go. Core clamps this with `.max(1)` wherever it divides
/// by it, so an honest zero is safe and a lie about one row is not.
fn doc_lines(pane: Area, status_h: i32, line_h: i32) -> usize {
    // No measure: it only ever insets horizontally, and this is a question
    // about height. Passing one would be an invitation to believe otherwise.
    (doc_rect(pane, status_h, 0).h / line_h.max(1)).max(0) as usize
}

/// Whole columns that fit in `w` pixels. Long lines are cut here rather than
/// drawn and clipped, so a 10k-character line costs one pane's worth of blits.
fn visible_cols(w: i32, cell_w: i32) -> usize {
    (w.max(0) / cell_w.max(1)) as usize
}

/// (line, column) of an arbitrary char offset. `Buffer::cursor_line_col` reads
/// the buffer's own cursor, which is only the *focused* window's.
fn line_col(buf: &Buffer, cursor: usize) -> (usize, usize) {
    let c = cursor.min(buf.len_chars());
    let line = buf.text.char_to_line(c);
    (line, c - buf.line_start(line))
}

/// What a pane's modeline says.
///
/// The active pane gets the editor's status line — mode, position, messages,
/// and the prompt while one is open. The others get only their own buffer,
/// because every other field in that line describes the focused window and
/// would be the same lie repeated in every pane.
// --- modeline appearance ---------------------------------------------------
//
// Every knob Emacs exposes on `mode-line` lives behind one of the functions
// below, so wiring a real setting to it later is a one-line change to the
// body. None of them are customisable *today*: the only paths from Lisp into
// the renderer are `Settings` (font size, fg/bg, line numbers, tab width,
// completion style) and `EditorCommand::SetSyntaxColor`, whose face names come
// from `HlKind::from_name` — and there is no modeline face in `HlKind`. So
// these read what exists and derive the rest.

/// Width of the modeline's 3D border in pixels — Emacs's `:box :line-width`.
/// Positive raises the strip, negative sinks it (see [`bevel`]), 0 is flat.
///
/// Set from Lisp with `(set-modeline-relief n)`.
fn relief(settings: &Settings) -> i32 {
    settings.modeline_relief
}

/// Vertical breathing room around the modeline's text, on top of the glyph
/// height and the box. `(set-modeline-pad n)`.
fn modeline_pad(settings: &Settings) -> i32 {
    settings.modeline_pad
}

/// The strip's own background. Derived from the theme rather than fixed, so it
/// stays a *lighter shade of the buffer* on a light theme instead of turning
/// into a grey bar — and so the bevel shades below have something to push off.
///
/// The derived shade is the *fallback*: `(set-syntax-color "modeline" r g b)`
/// overrides it, and leaving it unset keeps the strip tracking the theme.
fn modeline_bg(editor: &Editor, is_active: bool) -> [f32; 3] {
    let (kind, t) = if is_active {
        (HlKind::Modeline, 0.14)
    } else {
        (HlKind::ModelineInactive, 0.06)
    };
    let derived = mix(editor.settings.background, editor.settings.foreground, t);
    editor.theme.color(kind, derived)
}

/// Text colour on the strip. Inactive windows get a dimmer label, which is most
/// of what separates them at a glance. `"modeline-text"` overrides it.
fn modeline_fg(editor: &Editor, is_active: bool) -> [f32; 3] {
    let derived = mix(
        editor.settings.background,
        editor.settings.foreground,
        if is_active { 0.92 } else { 0.55 },
    );
    match is_active {
        true => editor.theme.color(HlKind::ModelineText, derived),
        false => derived,
    }
}

/// The lit edge: the strip's background pushed toward white.
///
/// Toward *white* rather than by a fixed amount, because a light theme's
/// modeline is already near the top of the range and adding a constant would
/// clip both edges to the same colour, flattening the bevel.
/// ponytail: hardcoded; wants a settable highlight shade.
fn highlight_shade(base: [f32; 3]) -> [f32; 3] {
    mix(base, [1.0; 3], 0.40)
}

/// The shadowed edge — [`highlight_shade`] toward black, and deliberately the
/// stronger of the two: a shadow that is weaker than its highlight reads as
/// glow rather than depth. ponytail: hardcoded; wants a settable shadow shade.
fn shadow_shade(base: [f32; 3]) -> [f32; 3] {
    mix(base, [0.0; 3], 0.50)
}

/// The bar between two panes. Pushed further toward the foreground than either
/// modeline shade (0.14 active, 0.06 idle) so it reads as a seam rather than as
/// more chrome, and derived from the theme for the same reason [`modeline_bg`]
/// is — on a light theme a fixed grey would be the darkest thing on screen.
///
/// ponytail: not settable. Ceiling: a theme that wants a coloured seam. Upgrade
/// path: a `HlKind::Divider` face, which is a one-line change here plus an
/// entry in `HlKind::ALL` — and that lives in core.
fn divider_shade(settings: &Settings) -> [f32; 3] {
    mix(settings.background, settings.foreground, 0.22)
}

/// Total height of a modeline: one text line, its padding, and the box on both
/// edges. The relief is *added* rather than eaten out of the text row, matching
/// Emacs, where a wider `:box` makes the mode line taller.
fn modeline_h(line_h: i32, settings: &Settings) -> i32 {
    line_h + modeline_pad(settings) + 2 * relief(settings).abs()
}

/// The modeline strip inside `area`: full width, hugging the bottom edge.
///
/// Clamped to `area`, so a window too short for a modeline gets a shorter one
/// (possibly empty) rather than a rectangle with a negative height or a `y`
/// hanging off the top of its own window.
fn modeline_rect(area: Area, h: i32) -> Area {
    let avail = area.h.max(0);
    let h = h.clamp(0, avail);
    Area {
        x: area.x,
        y: area.y + avail - h,
        w: area.w.max(0),
        h,
    }
}

/// Bevel width actually used for `rect` — the requested relief, clamped so the
/// facing edges of a very short or narrow strip cannot overlap and paint the
/// whole thing shadow-coloured.
fn bevel_width(rect: Area, relief: i32) -> i32 {
    relief.abs().min(rect.w.max(0) / 2).min(rect.h.max(0) / 2)
}

/// The four edges of `rect`'s 3D border as `(edge, is_highlight)`, for a relief
/// of `relief` pixels. Empty when the relief is 0 or the rect is too small to
/// hold one.
///
/// Positive relief lights the top and left and shadows the bottom and right —
/// a surface tilted toward a light source above and to the left, i.e. raised.
/// Negative swaps them, which is the same surface tilted away: pressed in.
/// Emacs's `:box` `:line-width` has exactly this sign convention.
fn bevel(rect: Area, relief: i32) -> Vec<(Area, bool)> {
    let n = bevel_width(rect, relief);
    if n == 0 {
        return Vec::new();
    }
    let lit = relief > 0;
    vec![
        (Area { h: n, ..rect }, lit),                                  // top
        (Area { y: rect.y + rect.h - n, h: n, ..rect }, !lit),         // bottom
        (Area { w: n, ..rect }, lit),                                  // left
        (Area { x: rect.x + rect.w - n, w: n, ..rect }, !lit),         // right
    ]
}

/// Where a completion popup goes, in pixels, plus the number of candidate rows
/// it was actually sized for — which is the requested count only when the
/// window was tall enough for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Popup {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    rows: usize,
}

/// Everything in a bottom panel that is not a candidate row: padding above and
/// below, and the input line.
fn bottom_chrome_h(line_h: i32) -> i32 {
    2 * PADV + line_h
}

/// Everything in a centered box that is not a candidate row: the title line,
/// the input line, two 1px rules and the padding around all of it.
fn center_chrome_h(line_h: i32) -> i32 {
    6 * PADV + 2 * line_h + 2
}

/// A full-width panel whose bottom edge is the top of the status strip and
/// which grows upward — `consult`/`vertico`.
///
/// One document line is deliberately kept visible above it, so the panel never
/// eats the whole window; when even that does not fit, `rows` clamps to 0 and
/// the panel degenerates to its input line rather than producing a negative
/// height.
fn bottom_popup(w: i32, h: i32, status_h: i32, line_h: i32, want: usize) -> Popup {
    let avail = (h - status_h).max(line_h);
    let chrome = bottom_chrome_h(line_h);
    let room = ((avail - line_h - chrome) / line_h).max(0) as usize;
    let rows = want.min(room);
    let box_h = (chrome + rows as i32 * line_h).min(avail);
    Popup {
        x: 0,
        y: avail - box_h,
        w,
        h: box_h,
        rows,
    }
}

/// A floating box, horizontally centered and sitting a third of the free space
/// down rather than half — `telescope.nvim`. Same clamping story as
/// [`bottom_popup`], with two document lines kept visible instead of one
/// because a box that touches both edges is not floating.
fn center_popup(w: i32, h: i32, status_h: i32, line_h: i32, cell_w: i32, want: usize) -> Popup {
    let box_w = (w * CENTER_PCT / 100)
        .clamp(CENTER_MIN_COLS * cell_w, CENTER_MAX_COLS * cell_w)
        .min(w - 2 * PAD)
        .max(cell_w);
    let avail = (h - status_h).max(line_h);
    let chrome = center_chrome_h(line_h);
    let room = ((avail - 2 * line_h - chrome) / line_h).max(0) as usize;
    let rows = want.min(room);
    let box_h = (chrome + rows as i32 * line_h).min(avail);
    Popup {
        x: ((w - box_w) / 2).max(0),
        y: (avail - box_h) / 3,
        w: box_w,
        h: box_h,
        rows,
    }
}

/// Cells between one which-key column and the next.
const WHICH_KEY_GUTTER: usize = 2;

/// Rows a which-key panel wants for `n` entries of at most `widest` cells, in a
/// panel `cols` cells across and with room for `max_rows`.
///
/// As *few* rows as the width allows, filled column-major when drawn. Entries
/// are short — `f +file` — so a which-key that is taller than it is wide has
/// stopped being a hint and become a menu, which is the thing the status line
/// was already failing to be.
fn which_key_rows(widest: usize, n: usize, cols: usize, max_rows: usize) -> usize {
    let columns = (cols / (widest + WHICH_KEY_GUTTER).max(1)).max(1);
    n.div_ceil(columns).clamp(1, max_rows.max(1))
}

/// `s` cut to `cols` characters, ellipsised when anything was lost.
///
/// Counts *characters*: candidates are file paths and Lisp symbol names, so a
/// byte-wise cut would both mis-measure a non-ASCII row and panic on a split
/// codepoint.
fn truncate(s: &str, cols: usize) -> String {
    if str_cells(s) <= cols {
        return s.to_string();
    }
    match cols {
        0 => String::new(),
        1 => "…".to_string(),
        // ponytail: truncates the tail, so two long paths sharing a prefix look
        // identical. Ceiling: deep directory trees. Upgrade path: elide the
        // middle, keeping the basename.
        //
        // Cells, not characters: a wide character that would only half fit is
        // dropped rather than cut, since half a glyph is what pushes everything
        // after it one column right.
        n => {
            let mut out = String::new();
            let mut used = 0;
            for c in s.chars() {
                let w = char_cells(c);
                if used + w > n - 1 {
                    break;
                }
                used += w;
                out.push(c);
            }
            out.push('…');
            out
        }
    }
}

/// A prompt label as a box title: `"Find file: "` -> `"Find file"`.
fn title_of(label: &str) -> &str {
    label.trim().trim_end_matches(':').trim_end()
}

/// Column at which to start drawing `s` so it lands centered in `cols` columns.
/// Counts *cells* — the dashboard banner is box-drawing art, so a byte count
/// would push it a third of the way off screen and a character count would
/// mis-centre any row with an emoji in it.
fn center_col(s: &str, cols: usize) -> usize {
    cols.saturating_sub(str_cells(s)) / 2
}

/// Box-drawing / block-element art versus prose. Lets the renderer tint the
/// dashboard banner without `Dashboard` having to describe its own layout.
fn is_banner_line(s: &str) -> bool {
    !s.trim().is_empty()
        && s.chars()
            .all(|c| c == ' ' || ('\u{2500}'..='\u{259f}').contains(&c))
}


// --- line overflow ---------------------------------------------------------
//
// Everything below counts *display cells*, never source chars and never bytes:
// a tab is several cells and a `→` is one, so wrapping on anything else puts
// the break in the wrong place (or, for bytes, mid-codepoint).

/// The cell range `[start, end)` shown on each display row of a line of `len`
/// cells, in order. Truncation takes the first of these and nothing else.
///
/// ponytail: the break is at exactly `cols`, so a wide character whose first
/// cell is the last column is cut in half — its glyph is clipped at the pane
/// edge and its continuation blank starts the next row. Emacs moves the whole
/// character down. Ceiling: a CJK paragraph in a narrow pane, where every other
/// row ends on a half glyph. Upgrade path: this takes the cell list rather than
/// a length, and backs the break off by one when it lands on a continuation
/// cell — which is also what a per-line table of advances would give it.
fn wrap_rows(len: usize, cols: usize) -> impl Iterator<Item = (usize, usize)> {
    (0..wrap_row_count(len, cols)).map(move |r| {
        let s = (r * cols).min(len);
        (s, (s + cols).min(len))
    })
}

/// Column of the truncation marker for a line of `len` cells in `cols`
/// columns, or `None` when the whole line is visible.
///
/// A line that exactly fills the pane is *not* marked: the marker claims there
/// is more text, and it costs a column to say so, so saying it falsely is worse
/// than staying quiet.
fn truncation_marker(len: usize, cols: usize) -> Option<usize> {
    (cols > 0 && len > cols).then(|| cols - 1)
}

/// The part of display-cell range `sel` that lands on the row starting at cell
/// `rs`, as columns within that row — `None` when it misses the row entirely.
///
/// Clipped to `cols`, which is what stops a selection running off the pane: the
/// range may extend one cell past the line's last character (the newline a
/// linewise selection swallows) and a wrapped row's range is unbounded to the
/// right until it is cut here.
fn row_span(sel: (usize, usize), rs: usize, cols: usize) -> Option<(usize, usize)> {
    let (a, b) = (sel.0.max(rs), sel.1.min(rs + cols));
    (b > a).then(|| (a - rs, b - rs))
}

/// (row, column) of a cursor on display cell `vc` of a line occupying `rows`
/// rows of `cols` columns.
///
/// The cursor may sit one cell *past* the last character — insert mode at end
/// of line — and on a line ending exactly at the pane's edge that cell is the
/// first column of a row this line does not own. Emacs opens a continuation row
/// for it; we park it on the trailing margin of the last row the line does own,
/// one cell left of where Emacs draws it and, more importantly, inside the
/// pane. Truncated lines (`rows == 1`) land there too: with no horizontal
/// scrolling there is nowhere else to say "the cursor is off to the right".
fn cursor_pos(vc: usize, cols: usize, rows: usize) -> (usize, usize) {
    if cols == 0 {
        return (0, 0);
    }
    match vc / cols {
        r if r < rows => (r, vc % cols),
        _ => (rows.saturating_sub(1), cols),
    }
}

/// Buffer lines that fit in `rows` display rows, given each line's row count in
/// `heights` counting down from the first visible line — the window's
/// `viewport_lines`.
///
/// Rows left over once the buffer ends count as one line each. Core clamps
/// `scroll` against this number, so a count that shrank at the end of the file
/// would let the view scroll past it; topping up also makes truncation (every
/// height 1) report the pane's full row capacity, exactly as before wrapping
/// existed. A single line taller than the whole pane still reports 1, which is
/// what keeps scrolling able to step over it.
///
/// A height of **0** is a folded line, and is the reason this adds `h` rather
/// than `h.max(1)`: such a line spans a buffer line and occupies no row, so it
/// has to be counted as a line — or `scroll` could never step over a fold — and
/// must not be charged a row.
fn lines_in_rows(heights: impl IntoIterator<Item = usize>, rows: usize) -> usize {
    let (mut used, mut lines) = (0usize, 0usize);
    for h in heights {
        if used >= rows {
            break;
        }
        used += h;
        lines += 1;
    }
    lines + rows.saturating_sub(used)
}

/// [`lines_in_rows`] for a real buffer. Only the visible lines are expanded —
/// the iterator is lazy and the count stops as soon as the pane is full — so a
/// 10k-line file costs one screenful, same as drawing it.
fn visible_lines(
    buf: &Buffer,
    scroll: usize,
    rows: usize,
    text_w: i32,
    cell_w: i32,
    set: &Settings,
) -> usize {
    let wrap = set.line_overflow == LineOverflow::Wrap;
    // Truncation with nothing folded and nothing typeset is one row per line,
    // and the general path below would agree with it more slowly. A fold, a
    // scale and a prefix each break that identity — the first by removing rows,
    // the second by claiming extra ones, the third by narrowing the line into an
    // earlier wrap — which is why this is not simply the overflow mode.
    if !wrap && !buf.overlays().iter().any(reshapes_lines) {
        return rows;
    }
    let heights =
        (scroll..buf.len_lines()).map(|l| line_rows(buf, l, text_w, cell_w, wrap, set.tab_width));
    lines_in_rows(heights, rows)
}

/// Whether an overlay can make a line occupy a different number of rows than its
/// text alone would. The one predicate [`visible_lines`] skips its whole slow
/// path on, so anything added to [`LineBox`] belongs here too.
fn reshapes_lines(o: &Overlay) -> bool {
    o.fold || o.scale.is_some() || o.line_prefix.is_some()
}

/// Display rows buffer line `l` occupies: none at all when a fold is hiding it,
/// one when the pane truncates body-size text, and its wrapped height times its
/// type size otherwise.
///
/// The counting twin of the drawing loop, and the reason folding is a *line*
/// mechanism rather than another overlay payload: everything else an overlay
/// carries replaces cells with cells, and this is a height of zero. A scale is
/// the same mechanism pointing the other way — a height of two or three — which
/// is why both are resolved here and not in two places.
fn line_rows(
    buf: &Buffer,
    l: usize,
    text_w: i32,
    cell_w: i32,
    wrap: bool,
    tab_width: usize,
) -> usize {
    let start = buf.line_start(l);
    if fold_hiding(buf.overlays(), start).is_some() {
        return 0;
    }
    let b = line_box(
        &line_style(buf.overlays(), start, start + buf.line_len(l)),
        text_w,
        cell_w,
    );
    let visual = match wrap {
        true => wrap_row_count(line_cells(buf, l, tab_width).len(), b.cols),
        false => 1,
    };
    visual * b.tall
}

/// Visual rows of a line the draw loop actually walks, given the `fits` display
/// rows it was granted, the `tall` display rows one visual row of it costs, and
/// the `text_rows` its own text occupies.
///
/// Two clamps, and the second is the one worth a function. Rows that *begin*
/// inside the pane is the first: the last of them may overhang the bottom edge
/// and is clipped there, the same trade a tall image already makes.
///
/// The second is that a line never gets more visual rows than its text has, and
/// it is load-bearing rather than tidy. `fits` comes from a `max` with the rows
/// an *image* on the line claimed, so a tall fragment on a **truncated** line
/// used to buy row 1 and row 2 of a line the pane had already decided to cut at
/// row 0. The loop then sliced `cells[rs .. rs + shown]` on those rows with a
/// `shown` the truncation marker had pinned at `cols - 1` — past the end of any
/// line only a little wider than the pane, and a panic rather than a smear.
///
/// It is also the only reading under which the three row counts agree:
/// [`line_rows`] and [`offset_at`] both spend one visual row on a truncated line
/// whatever is drawn on it, so a click and a scroll clamp already believed this.
fn drawn_rows(fits: usize, tall: usize, text_rows: usize) -> usize {
    fits.div_ceil(tall.max(1)).min(text_rows.max(1))
}

/// The cells of buffer line `l`. The renderer's one entry into
/// [`zemacs_core::display`], and the same call `Editor::line_cells` makes — `j`
/// and the glyph it lands on have to be laid out by the same function.
fn line_cells(buf: &Buffer, l: usize, tab_width: usize) -> Vec<(char, usize)> {
    let start = buf.line_start(l);
    expand_line(&buf.slice_string(start, start + buf.line_len(l)), tab_width)
}


/// What the gutter says beside buffer line `line`.
///
/// Absolute is the buffer line number; relative is the distance in **buffer
/// lines** from the cursor's, and the cursor's own line keeps its absolute
/// number — vim's `number relativenumber` pair, and Emacs'
/// `display-line-numbers-current-absolute`.
///
/// It used to count *visual rows*, on the argument that `j` and `k` move by row
/// here, so `3j` should land on the row labelled 3. That argument is correct and
/// the result was still wrong, because it made the gutter unreadable at exactly
/// the moment it had the most to say: a wrapped line got a number on every one
/// of its rows, so one line of prose showed three descending numbers and looked
/// like three lines. Worse, a wrapped line *containing the cursor* showed its
/// absolute number on one row and a relative number on the others — the same
/// line labelled two different things.
///
/// One number per line, then, and the continuation rows stay blank in both
/// modes. What it costs is real and small: with wrapping on, `3j` may land
/// somewhere other than the line labelled 3, because the count is of lines and
/// the motion is of rows. Wrapping is off in every code buffer, and counted
/// jumps are not how anyone reads prose.
///
/// ponytail: Emacs makes this a choice — `display-line-numbers-type` is `t`,
/// `'relative` or `'visual`, and the old behaviour was `'visual`. If someone
/// wants it back, that is the setting to add, not a second numbering rule.
fn gutter_number(line: usize, cur_line: usize, relative: bool) -> usize {
    match relative && line != cur_line {
        true => line.abs_diff(cur_line),
        false => line + 1,
    }
}

/// Digits reserved for the line-number gutter. Sized from the whole file so the
/// text column does not shift as you scroll into four-digit territory.
fn gutter_digits(buf: &Buffer) -> usize {
    buf.len_lines().to_string().len().max(3)
}

/// Whether `buf` shows a line-number gutter at all.
///
/// The buffer decides, falling back to the editor. Per buffer because the
/// gutter is drawn per *pane*: an org file and a source file side by side want
/// different answers, and a single editor-wide flag can only give them the same
/// one — whichever mode was entered last.
///
/// The two kinds are not a policy anyone configures. A dashboard is a menu and a
/// terminal's rows are a grid the shell owns; neither has buffer lines to
/// number, so neither is a decision Lisp should have to remember to make.
///
/// **Its own function because two loops ask it**, and they used to ask it
/// differently: [`gutter_w`] reserved the columns from *this* answer while the
/// draw loop decided whether to write a number from `settings.line_numbers`
/// alone. On the config we ship — `(set-line-numbers t)` plus
/// `(set-no-gutter-modes '("org-mode" …))` — those two disagree on every org
/// buffer, which reserved no columns and then printed the numbers into the first
/// three of the text.
fn gutter_on(buf: &Buffer, set: &Settings) -> bool {
    match buf.kind {
        BufferKind::Dashboard | BufferKind::Terminal => false,
        _ => buf.line_numbers.unwrap_or(set.line_numbers),
    }
}

/// Width of the line-number gutter in pixels — zero when they are off. Both the
/// row count and the drawing loop need it, and they must not disagree about how
/// many columns the text has left.
fn gutter_w(buf: &Buffer, set: &Settings, cell_w: i32) -> i32 {
    match gutter_on(buf, set) {
        true => (gutter_digits(buf) as i32 + 1) * cell_w,
        false => 0,
    }
}

// --- overlays --------------------------------------------------------------

/// One overlay's claim on a line, in line-relative source char offsets.
type OverlayRun<'a> = (usize, usize, &'a Overlay);

/// The overlays touching `[start, end)`, clipped to it and rebased to
/// line-relative char offsets, in creation order.
///
/// A linear scan of the whole list per line. ponytail: the same ceiling
/// `zemacs_core::overlay` names — right for the tens of overlays a config makes
/// by hand, wrong for one per hit of a search, and the fix is on that side.
fn overlays_for_line<'a>(overlays: &'a [Overlay], start: usize, end: usize) -> Vec<OverlayRun<'a>> {
    overlays
        .iter()
        .filter(|o| o.end > start && o.start < end)
        .map(|o| (o.start.max(start) - start, o.end.min(end) - start, o))
        .collect()
}

/// The foreground and background in force at line-relative source char `src`.
///
/// Later beats earlier, and *per attribute*: an overlay that sets only a
/// background leaves an earlier one's foreground alone, which is what makes a
/// highlight and a face stack rather than fight.
fn overlay_face(runs: &[OverlayRun], src: usize) -> (Option<HlKind>, Option<HlKind>) {
    let (mut fg, mut bg) = (None, None);
    for &(s, e, o) in runs {
        if s <= src && src < e {
            fg = o.face.or(fg);
            bg = o.background.or(bg);
        }
    }
    (fg, bg)
}

/// Whether an overlay's run is set bold and italic at line-relative source char
/// `src`.
///
/// [`overlay_face`]'s sibling, and separate from it on purpose: colour and
/// weight resolve by the same rule but are consumed at different moments — a
/// colour picks the `set_color_mod`, a weight picks the *face*, and only the
/// second can make the renderer open a font. Same tri-state, so `Some(false)`
/// from a later overlay takes an earlier one's bold off and `None` leaves it.
fn overlay_emphasis(runs: &[OverlayRun], src: usize) -> (bool, bool) {
    let (mut bold, mut italic) = (None, None);
    for &(s, e, o) in runs {
        if s <= src && src < e {
            bold = o.bold.or(bold);
            italic = o.italic.or(italic);
        }
    }
    (bold.unwrap_or(false), italic.unwrap_or(false))
}

/// The type sizes the renderer will ever open a face at, as percentages of the
/// body size. A `scale` from Lisp is snapped to the nearest of them.
///
/// **This list is the glyph cache's bound**, and that is why it is a list rather
/// than a range: a face is a `Font` plus a texture per character it has drawn,
/// so an editor that honoured `1.37×` literally would open a face and grow a
/// cache per distinct number any config ever passed. Four steps times four
/// styles is sixteen possible faces, and the two the editor draws almost
/// everything in are not even in this map (see [`Renderer::faces`]).
///
/// Snapping rather than clamping-and-rounding because the steps are not evenly
/// spaced: they are the sizes a document actually uses — body, a subheading, a
/// heading, a title — and a config asking for 1.4 wants the 1.5 rather than a
/// face of its own.
const SCALE_STEPS: [u16; 4] = [100, 125, 150, 200];

/// The step nearest `pct`. Out of range clamps to the ends, which is the honest
/// answer for `0.5×` (there is no smaller step: shrinking text is a separate
/// problem, since a cell narrower than the body's would make a *short* line and
/// nothing wants that) and for `4×` alike.
fn scale_step(pct: u16) -> u16 {
    *SCALE_STEPS
        .iter()
        .min_by_key(|s| s.abs_diff(pct))
        .expect("SCALE_STEPS is not empty")
}

/// A body-size measurement at `pct` of body size. One pixel minimum, since a
/// zero-wide cell is a division by zero two functions later.
fn scaled(v: i32, pct: u16) -> i32 {
    (v * pct as i32 / 100).max(1)
}

/// Everything the overlays touching a line say about the **line** rather than
/// about its cells: how big its type is, what colour the band behind it is, and
/// what sits in front of it.
///
/// Resolved once per line and then consulted by four different loops — the draw
/// pass, the row count, the cursor's row, and the click map — which is the only
/// way those four can agree about where a line ends.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct LineStyle<'a> {
    /// Percent of body size, already snapped to a [`SCALE_STEPS`] entry. 0 means
    /// nothing claimed a size, and reads as 100 everywhere below.
    scale: u16,
    background: Option<HlKind>,
    /// The prefix and the colour to draw it in — one overlay carries both, so a
    /// quote bar is `(overlay-put ov 'line-prefix "▎ ")` beside that overlay's
    /// own `face` rather than a second colour property nothing else would use.
    prefix: Option<(&'a str, Option<HlKind>)>,
}

/// The line attributes in force on `[start, end)`.
///
/// Later beats earlier per attribute, as everywhere else here — *except* the
/// scale, which is a `max`. An overlay cannot shrink a line under another
/// overlay's type: the row height has to hold every run on the line, and a
/// last-wins rule would let a later 1× overlay clip a 2× one's glyphs into the
/// line below.
fn line_style<'a>(overlays: &'a [Overlay], start: usize, end: usize) -> LineStyle<'a> {
    let mut style = LineStyle::default();
    for o in overlays.iter().filter(|o| o.end > start && o.start < end) {
        if let Some(s) = o.scale {
            style.scale = style.scale.max(scale_step(s));
        }
        style.background = o.line_background.or(style.background);
        if let Some(p) = &o.line_prefix {
            style.prefix = Some((p.as_str(), o.face));
        }
    }
    style
}

/// One buffer line's grid, once its overlays have had their say.
///
/// The four numbers every loop that touches a line needs, and the reason they
/// travel together: a scaled line has a wider cell *and* a taller row *and*
/// therefore fewer columns, and a prefix takes columns off the front. Working
/// any one of them out without the others is how the cursor ends up drawn in a
/// column the text is not in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LineBox {
    /// Cell width for this line, in pixels.
    cw: i32,
    /// Display rows one *visual* row of this line occupies. This is the whole of
    /// "a scaled line is taller": [`image_rows`] already established that a line
    /// may own several rows, and a scaled one claims them by the same rule, so
    /// `doc_lines`, `scroll` and the gutter keep counting whole rows.
    tall: usize,
    /// Pixels this line's text is pushed right by its prefix.
    indent: i32,
    /// Columns of text left after the prefix.
    cols: usize,
}

/// [`LineBox`] for a line styled `style`, given `text_w` pixels between the
/// gutter and the pane's right edge.
///
/// `tall` is `ceil(scale)` and nothing cleverer: a run set at 1.5× has a 1.5×
/// em box, so it needs two of the body's rows and gets exactly two. The type
/// sits at the *bottom* of that block — see the draw loop — which puts the slack
/// above the line, where a heading wants its air.
fn line_box(style: &LineStyle, text_w: i32, cell_w: i32) -> LineBox {
    let pct = style.scale.max(100);
    let cw = scaled(cell_w, pct);
    let indent = match style.prefix {
        Some((p, _)) => str_cells(p) as i32 * cw,
        None => 0,
    };
    LineBox {
        cw,
        tall: usize::from(pct).div_ceil(100),
        indent,
        cols: visible_cols(text_w - indent, cw),
    }
}

/// Cells a bitmap `width` pixels across reserves. At least one, so an image
/// narrower than a cell still hides the character it replaced.
fn image_cells(width: u32, cell_w: i32) -> usize {
    (width as usize).div_ceil(cell_w.max(1) as usize).max(1)
}

/// Display rows a bitmap needs, under the rule that its baseline lands on the
/// baseline of the *last* of them.
///
/// The vertical twin of [`image_cells`], and the only reason a row's height is
/// not a constant. One row already offers `ascent` pixels above the baseline —
/// which is every inline fragment, since `$x_1$` is set at the text's own size —
/// and anything taller than that buys whole further rows above it. `depth` is
/// the part that hangs *below* the baseline and is therefore free: it lands in
/// the last row's descender space, exactly where a `g` puts its tail.
fn image_rows(height: u32, depth: u32, line_h: i32, ascent: i32) -> usize {
    let above = (height as i32 - depth as i32).max(0) - ascent;
    1 + (above.max(0) as usize).div_ceil(line_h.max(1) as usize)
}

/// Replace the cells of each `(start, end, text)` — line-relative *source* char
/// offsets — with `text`'s characters, all attributed to `start`.
///
/// Attributing them to `start` is what keeps everything else working unchanged:
/// [`visual_col`] still finds a column for a cursor inside the hidden range, the
/// highlight cursor still walks monotonically, and wrapping counts the cells
/// that are actually drawn.
///
/// `subs` must be sorted by `start`. Overlapping substitutions are not
/// composable and are not composed — the one that starts first wins and the rest
/// are dropped. In practice they never overlap: LaTeX fragments are disjoint by
/// construction, and so are bullets.
fn substitute(cells: &[(char, usize)], subs: &[(usize, usize, String)]) -> Vec<(char, usize)> {
    let mut out = Vec::with_capacity(cells.len());
    let (mut i, mut si) = (0usize, 0usize);
    while i < cells.len() {
        let src = cells[i].1;
        while si < subs.len() && subs[si].1 <= src {
            si += 1;
        }
        match subs.get(si) {
            Some((s, e, text)) if *s <= src => {
                out.extend(text.chars().map(|c| (c, *s)));
                while i < cells.len() && cells[i].1 < *e {
                    i += 1;
                }
                let e = *e;
                si += 1;
                while si < subs.len() && subs[si].0 < e {
                    si += 1; // started inside the one just applied
                }
            }
            _ => {
                out.push(cells[i]);
                i += 1;
            }
        }
    }
    out
}

/// Highlight runs covering `[start, end)`, clipped to it and rebased to
/// line-relative char offsets.
///
/// `from` is a monotonic cursor into the (sorted) span list — pass the returned
/// value to the next line so the whole frame walks `highlights` once. It only
/// advances past spans that end before this line, so a span straddling a line
/// boundary is still seen by the next call.
fn spans_for_line(
    spans: &[Span],
    from: usize,
    start: usize,
    end: usize,
) -> (usize, Vec<(usize, usize, HlKind)>) {
    let mut next = from;
    while next < spans.len() && spans[next].end <= start {
        next += 1;
    }
    let mut out = Vec::new();
    let mut j = next;
    while j < spans.len() && spans[j].start < end {
        let (s, e) = (spans[j].start.max(start), spans[j].end.min(end));
        if s < e {
            out.push((s - start, e - start, spans[j].kind));
        }
        j += 1;
    }
    (next, out)
}

// --- scenes ----------------------------------------------------------------
//
// A scene is the other thing a window can show: a tree laid out in pixels by
// `crates/gui` rather than a grid of cells counted here. See `docs/gui.org`.
//
// **A scene adds no second font stack.** Everything below measures and paints
// through the `Face` cache the overlays already opened — same `SCALE_STEPS`,
// same `MAX_FACES`, same `MAX_FACE_GLYPHS`, same digest. The only thing that is
// new is that a scene asks the *font* how wide a string is instead of
// multiplying a cell width, which is the whole point of it: the grid's answer is
// arithmetic and a scene's has to be the truth, because there is no column for a
// wrong answer to be corrected against.

/// Everything that would move a box if it changed, in one number.
///
/// The scene itself — every node, every run, every byte of text, and the scroll
/// offset, since layout is what applies it — plus the viewport it is being laid
/// out in and the point size the body is open at, because a resize and a
/// font-size change both relayout a document that did not itself change.
///
/// A hash rather than a comparison against a kept copy, for the space: a
/// curriculum's arena is the document, and holding two of them to notice that
/// one equals the other costs more than measuring it again would have.
fn scene_key(scene: &Scene, viewport: zemacs_core::Rect, point_size: u16) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    scene.hash(&mut h);
    (viewport.x, viewport.y, viewport.w, viewport.h, point_size).hash(&mut h);
    h.finish()
}

/// The [`Cut`] a scene [`Style`] asks for.
///
/// The snap is [`scale_step`], exactly as an overlay's `scale` is snapped, and
/// it is what keeps the face cache's key space closed: a scene is written by
/// hand in Lisp and would otherwise open a face per distinct percentage anybody
/// ever typed. `size: 0` is a legal `u16` and no type size at all, and snaps to
/// the body — [`scale_step`] answers the nearest step, and nothing is nearer to
/// nothing than 100.
fn scene_cut(style: Style) -> Cut {
    Cut {
        pct: scale_step(style.size),
        bold: style.bold,
        italic: style.italic,
    }
}

/// Advance of `text` in `font`, or the grid's own arithmetic when there is no
/// face to ask.
///
/// `None` is a face [`open_face`] refused — a font file that will not open at
/// that point size — and the fallback is deliberately the *body* metric scaled,
/// not zero: a paragraph measured at zero wraps every word onto one line and
/// stacks the whole document in one place, which is a great deal harder to
/// recognise as a missing font than text that is merely spaced like the grid.
fn advance_in(font: Option<&Font>, cell_w: i32, text: &str, pct: u16) -> i32 {
    match font.and_then(|f| f.size_of(text).ok()) {
        Some((w, _)) => w as i32,
        // Cells rather than characters, so a wide character still measures two
        // of them — the same rule `draw_weighted` follows on the grid.
        None => str_cells(text) as i32 * scaled(cell_w, pct),
    }
}

/// Line height and ascent for `font`, or the body's own scaled, on the same
/// terms as [`advance_in`].
///
/// The ascent is the number that puts a 200% word, an inline equation and the
/// body text around them on one baseline, and it is also where a glyph texture
/// is blitted *from*: SDL_ttf renders a string with its baseline this far down,
/// so `baseline - ascent` is the top-left the blit wants. Getting it from the
/// same call layout got its metrics from is what stops the two disagreeing.
fn line_in(font: Option<&Font>, line_h: i32, ascent: i32, pct: u16) -> (i32, i32) {
    match font {
        Some(f) => (f.recommended_line_spacing().max(1), f.ascent().max(0)),
        None => (scaled(line_h, pct), scaled(ascent, pct)),
    }
}

/// The two questions `crates/gui` asks a font, answered by the faces this
/// renderer already has open.
///
/// # The `&self` in `advance`
///
/// `Measure` takes `&self` and the face cache is a `&mut` structure, and the two
/// are reconciled by a **pre-pass**: [`Renderer::layout_scene`] opens every face
/// the scene's runs name *before* handing itself to the layout engine, so
/// measuring is a pure read of a map that is already populated. The alternatives
/// were a `RefCell` around `faces` — which would put a runtime borrow on
/// `draw_styled_char`, the hottest typeset path there is, to serve a call that
/// happens once per relayout — and a separate struct borrowing the fonts, which
/// would have to be `pub` and would therefore put `sdl2::ttf::Font` in this
/// crate's public API. The pre-pass costs one walk of the arena and changes
/// nothing about how a glyph is drawn.
///
/// And it degrades rather than fails: a style whose face is not in the map is
/// measured on the body metric rather than at zero, so a caller who skipped the
/// pre-pass gets a document spaced like the grid instead of a document collapsed
/// into a single point.
impl Measure for Renderer {
    fn advance(&self, text: &str, style: Style) -> i32 {
        let cut = scene_cut(style);
        let font = self.face_font(face_key(self.point_size, cut));
        advance_in(font, self.cell_w, text, cut.pct)
    }

    fn line(&self, style: Style) -> (i32, i32) {
        let cut = scene_cut(style);
        let font = self.face_font(face_key(self.point_size, cut));
        line_in(font, self.line_h, self.ascent, cut.pct)
    }
}

impl Renderer {
    /// The font handle behind `key`, without opening anything.
    ///
    /// The same three-way split [`Renderer::draw_glyph`] makes, for the same
    /// reason: the body's plain and bold faces are not in the `faces` map, they
    /// are fields, and a lookup that missed them would answer `None` for the two
    /// faces nearly every character is set in.
    fn face_font(&self, key: FaceKey) -> Option<&Font<'static, 'static>> {
        if key.point_size == self.point_size {
            match key.style {
                0 => return Some(&self.font),
                1 => return Some(&self.bold),
                _ => {}
            }
        }
        self.faces.get(&key)?.as_ref().map(|f| &f.font)
    }

    /// Open every face `scene` will be measured in, so that [`Measure`] can
    /// answer from `&self`.
    ///
    /// Bounded by construction rather than by a cap of its own: [`scene_cut`]
    /// snaps to a [`SCALE_STEPS`] entry, so the keys a whole document can ask
    /// for are the same closed set of at most [`MAX_FACES`] that overlays could
    /// already ask for, and [`cached_face`] is the same door they go through.
    /// A scene therefore cannot widen the cache, only fill it.
    fn open_scene_faces(&mut self, scene: &Scene) {
        // Gathered first and opened after, because the gathering reads `self`
        // (for the body point size and for what is already open) and the
        // opening writes it. At most `MAX_FACES` entries, so the linear
        // `contains` is cheaper than a set.
        let mut want: Vec<FaceKey> = Vec::new();
        for id in 0..scene.len() {
            let Some(Node::Text { runs, .. }) = scene.node(id) else {
                continue;
            };
            for r in runs {
                let Run::Text { style, .. } = r else { continue };
                let key = face_key(self.point_size, scene_cut(*style));
                if self.face_font(key).is_none() && !want.contains(&key) {
                    want.push(key);
                }
            }
        }
        let ttf = self.ttf;
        // Cloned once for the whole pre-pass, not per face: `cached_face` wants
        // the path while it holds `&mut self.faces`, and those are two fields of
        // one struct.
        let path = self.font_path.clone();
        for key in want {
            cached_face(&mut self.faces, ttf, &path, key);
        }
    }

    /// The text rectangle a scene in frame `frame_index`'s current pane is
    /// drawn in, or `None` when that frame or pane is not there.
    ///
    /// The one place the viewport is decided, so that the paint, the wheel and
    /// the hit test cannot disagree about it. They would otherwise disagree by
    /// a padding and a modeline, which is a click landing in the paragraph
    /// above the one under the pointer.
    ///
    /// **No measure.** `doc_rect` takes `settings.text_width` for a document
    /// and zero here, for the reason the dashboard takes zero: a scene sets its
    /// own measure — that is what a `Block`'s `pad`, `width` and `align` are —
    /// and narrowing the pane underneath it would be the setting reaching past
    /// the document it is about, then the document centring itself inside the
    /// result.
    fn scene_viewport(&self, editor: &Editor, frame_index: usize) -> Option<zemacs_core::Rect> {
        let frame = editor.frames.get(frame_index)?;
        let pane = frame
            .panes(self.content_area())
            .into_iter()
            .find(|p| p.window == frame.current)?;
        let status_h = modeline_h(self.line_h, &editor.settings);
        Some(area_rect(doc_rect(area_of(pane.rect), status_h, 0)))
    }

    /// The focused pane's scene, laid out where it will be painted, with that
    /// viewport — `None` for a buffer drawn as a grid of cells, which is nearly
    /// all of them.
    ///
    /// For the app: the wheel needs the content height to clamp a scroll
    /// against, and a click needs the frames to hit test. Both focus the pane
    /// they are acting on first, which is what makes `editor.buffer` the buffer
    /// being scrolled or clicked; doing it any other way would leave the wheel
    /// moving one document's page while the keyboard was in another.
    pub fn scene_layout(
        &mut self,
        editor: &Editor,
        frame_index: usize,
    ) -> Option<(&Layout, zemacs_core::Rect)> {
        let scene = editor.buffer.scene.as_ref()?;
        let viewport = self.scene_viewport(editor, frame_index)?;
        let fresh = self.take_scene_layout(scene, viewport);
        Some((&self.scenes.insert(fresh).1, viewport))
    }

    /// The cached layout for `scene` in `viewport`, laid out again if what is
    /// in the cache was measured for something else — **taken out of the cache
    /// rather than borrowed from it**, because [`Renderer::draw_scene`] needs
    /// `&mut self` and a layout borrowed *from* self cannot be passed *to* it.
    /// Every caller puts it back.
    ///
    /// # How it decides the layout is stale
    ///
    /// By fingerprinting the scene's contents — see [`scene_key`] — rather than
    /// by a revision somebody has to remember to bump. Core says out loud that
    /// installing a scene deliberately does *not* move `Editor::revision`,
    /// because a scene is typeset rather than edited and the syntax spans that
    /// built its runs must survive; and a curriculum re-renders its whole page
    /// when one problem's state changes, so "same node count, same root, same
    /// buffer" is precisely the case that has to come out *different*. A
    /// content hash is the only test that cannot be fooled by that, and it is
    /// the same argument the frame digest makes one screen over: a fingerprint
    /// of what is actually there beats a list of fields to keep in step.
    ///
    /// The cost is one FNV walk of the arena per frame per scene pane, against
    /// a relayout that is a font call per word of the whole document — hashing
    /// a page is cheaper than measuring a paragraph of it.
    fn take_scene_layout(&mut self, scene: &Scene, viewport: zemacs_core::Rect) -> (u64, Layout) {
        let key = scene_key(scene, viewport, self.point_size);
        match self.scenes.take() {
            Some(hit) if hit.0 == key => hit,
            _ => (key, self.layout_scene(scene, viewport)),
        }
    }

    /// One pane's scene, laid out and painted where [`Renderer::draw_document`]
    /// would have painted its text.
    ///
    /// The colour resolver is where a `FaceId` stops being an integer: it is an
    /// index into the same `face-list` an `HlKind` names, so a scene follows a
    /// theme change for free and `crates/gui` still holds no dependency on
    /// core. `None` is the pane's own foreground — the only reading available,
    /// since there is no face that spells "default" — and it reaches here only
    /// from a run's style, because `draw_scene` never asks about a background
    /// or a fill the document left unset.
    fn draw_pane_scene(&mut self, editor: &Editor, scene: &Scene, doc: Area) {
        let viewport = area_rect(doc);
        let laid = self.take_scene_layout(scene, viewport);

        let fg = editor.settings.foreground;
        let theme = &editor.theme;
        // A number naming no face falls back to the pane's foreground rather
        // than to a face picked for it, on the same terms `HlKind::from_face_id`
        // answers `None`: the number arrived from arithmetic in Lisp. On a run
        // that is the right answer outright; on a block's background it paints
        // the foreground colour, which is wrong and *loudly* wrong, which is the
        // point — a colour quietly substituted is a bug you find by squinting.
        let colour = |face: Option<FaceId>| match face.and_then(HlKind::from_face_id) {
            Some(kind) => rgb(theme.color(kind, fg)),
            None => rgb(fg),
        };
        let image = |id: ImageId| editor.image(id);
        self.draw_scene(scene, &laid.1, viewport, &colour, &image);

        self.scenes = Some(laid);
    }

    /// Lay `scene` out inside `viewport`, measured in the real faces.
    ///
    /// The blessed entry point, because the pre-pass has to happen first and
    /// this is the only place that ordering is written down. Layout is not
    /// incremental — a scene relays out whole when it changes, never per frame —
    /// so this is reached through [`Renderer::take_scene_layout`], which is what
    /// decides whether it needs reaching at all.
    pub fn layout_scene(&mut self, scene: &Scene, viewport: zemacs_core::Rect) -> Layout {
        self.open_scene_faces(scene);
        let viewport = zemacs_gui::Rect {
            x: viewport.x,
            y: viewport.y,
            w: viewport.w,
            h: viewport.h,
        };
        zemacs_gui::layout(scene, viewport, &*self)
    }

    /// Paint a laid-out scene into `area`.
    ///
    /// `layout` is the answer [`Renderer::layout_scene`] gave for this scene,
    /// and `area` is the viewport it was given — a [`zemacs_gui::Frame`]'s rect
    /// is absolute in that viewport, with the scene's scroll already taken out,
    /// so painting it against a different one moves the whole document. A layout
    /// belonging to a *different scene* is not a panic either way: every id is
    /// looked up and a miss is skipped.
    ///
    /// `colour` resolves a [`FaceId`] — an index into the same `face-list` that
    /// names an `HlKind` — to a real colour, and `None` asks for the pane's own
    /// foreground, which is what a run with no face of its own is set in. It is
    /// a parameter rather than a `&Theme` so that everything below this line is
    /// paintable against `crates/gui` alone: the scene model deliberately holds
    /// an integer instead of a `HlKind`, and resolving it here would put the
    /// dependency back that the integer was chosen to avoid.
    ///
    /// `image` resolves an [`ImageId`] to its pixels, for the first sighting
    /// only — after that the texture cache answers. Same shape and same reason:
    /// the bitmap belongs to whoever produced it, and the renderer holds the
    /// upload rather than the source.
    ///
    /// Everything is clipped to `area`, and the clip in force on the way in is
    /// put back on the way out, so this can be called from inside a pane's own
    /// clip without stealing it.
    pub fn draw_scene<'i>(
        &mut self,
        scene: &Scene,
        layout: &Layout,
        area: zemacs_core::Rect,
        colour: &dyn Fn(Option<FaceId>) -> Color,
        image: &dyn Fn(ImageId) -> Option<&'i Image>,
    ) {
        let area = area_of(area);
        if area.w <= 0 || area.h <= 0 {
            return; // SDL dislikes an empty clip, and there is nowhere to draw
        }
        // Reading the clip is not a draw call and is deliberately not folded
        // into the digest: it changes no pixel, and the `set_clip` /
        // `clear_clip` that restore it are marked by those primitives.
        let restore = self.canvas.clip_rect();
        self.set_clip(area);

        // The scroll offset is already inside every rect below — the layout
        // engine applies it once, by starting the root above the viewport — so
        // a wheel notch normally moves the fills and the glyphs and the digest
        // moves with them. It is folded in anyway because "normally" is not
        // "always": the cull below drops every frame outside the pane, so a
        // scene scrolled past its own end draws *nothing*, and two different
        // offsets that both draw nothing would hash the same and the window
        // would never be put up again. The frame count comes along for the case
        // where the scene changed into one that happens to paint the same.
        self.mark([
            TAG_SCENE,
            u64::from(scene.scroll as u32),
            layout.frames.len() as u64,
            pack(area.w, area.h),
        ]);

        for f in &layout.frames {
            let r = f.rect;
            // Vertically outside the pane, which for a scrolled document is most
            // of it. Culled here rather than left to the clip because a
            // curriculum is thousands of frames and every one of them would
            // otherwise be a command SDL builds, queues and throws away. Two
            // scenes that are both entirely off-screen draw nothing and hash the
            // same, which is correct: they show the same picture.
            if r.h <= 0 || r.y + r.h <= area.y || r.y >= area.y + area.h {
                continue;
            }
            let Some(node) = scene.node(f.node) else {
                continue;
            };
            match node {
                Node::Block(b) => {
                    if let Some(face) = b.background {
                        self.fill(r.x, r.y, r.w, r.h, colour(Some(face)));
                    }
                    if let Some(face) = b.border {
                        self.stroke(r.x, r.y, r.w, r.h, colour(Some(face)));
                    }
                }
                Node::Rect { fill, .. } => {
                    if let Some(face) = fill {
                        self.fill(r.x, r.y, r.w, r.h, colour(Some(*face)));
                    }
                }
                // A figure. `depth` is inert at block level — there is no
                // baseline in a column of boxes to hang from — so the box is
                // exactly the rect layout gave it.
                Node::Image { image: id, .. } => {
                    if let Some(pixels) = image(*id) {
                        let (w, h) = (r.w.max(0) as u32, r.h.max(0) as u32);
                        self.blit_image(*id, pixels, r.x, r.y, w, h);
                    }
                }
                Node::Text { runs, .. } => self.draw_text_frame(runs, f, colour, image),
            }
        }

        match restore {
            sdl2::render::ClippingRect::Some(p) => self.set_clip(Area {
                x: p.x(),
                y: p.y(),
                w: p.width() as i32,
                h: p.height() as i32,
            }),
            // A clip admitting nothing, which is not the same as no clip. It
            // cannot arise from anything in this file — the pane loop skips a
            // pane dragged to nothing rather than clipping it away — so this
            // arm exists to keep the match total and honest.
            sdl2::render::ClippingRect::Zero => self.set_clip(Area {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            }),
            sdl2::render::ClippingRect::None => self.clear_clip(),
        }
    }

    /// One paragraph: every piece of every line, at the offset and on the
    /// baseline the layout engine recorded.
    fn draw_text_frame<'i>(
        &mut self,
        runs: &[Run],
        f: &SceneFrame,
        colour: &dyn Fn(Option<FaceId>) -> Color,
        image: &dyn Fn(ImageId) -> Option<&'i Image>,
    ) {
        for line in &f.lines {
            let baseline = f.rect.y + line.y + line.baseline;
            for p in &line.pieces {
                let left = f.rect.x + p.x;
                // A piece naming a run that is not there is arithmetic somebody
                // got wrong, and the same rule applies as to a dangling node id:
                // draw the rest of the document rather than take the frame down.
                let Some(run) = runs.get(p.run) else {
                    debug_assert!(false, "piece names run {} of {}", p.run, runs.len());
                    continue;
                };
                match run {
                    // On the baseline, hanging `depth` below it — which is the
                    // whole of why `depth` exists, and the difference between
                    // `$x_1$` sitting in the sentence and floating above it.
                    Run::Image {
                        image: id,
                        width,
                        height,
                        depth,
                        ..
                    } => {
                        let above = height.saturating_sub(*depth);
                        let top = baseline - i32::try_from(above).unwrap_or(i32::MAX);
                        if let Some(pixels) = image(*id) {
                            self.blit_image(*id, pixels, left, top, *width, *height);
                        }
                    }
                    Run::Text { text, style, .. } => {
                        // Byte offsets into UTF-8 that another crate computed.
                        // They *are* character boundaries — `wrap` finds its
                        // segments with `char_indices` and breaks a long word by
                        // `len_utf8` — and this is the assertion of that rather
                        // than the assumption: `get` answers `None` on a
                        // boundary that is not one, so a release build drops a
                        // piece where a `&text[..]` would panic mid-frame.
                        let Some(s) = text.get(p.start..p.end) else {
                            debug_assert!(
                                false,
                                "piece {}..{} is not on a character boundary of {text:?}",
                                p.start, p.end
                            );
                            continue;
                        };
                        let cut = scene_cut(*style);
                        // The blit is from the top-left of the glyph's own box,
                        // and the box's baseline is `ascent` down from there.
                        let top = baseline - self.line(*style).1;
                        let c = colour(style.face);
                        let mut x = left;
                        for ch in s.chars() {
                            // ponytail: one `advance` per character, so the pen
                            // steps by exactly what the layout engine measured
                            // and a piece cannot end somewhere other than where
                            // its `width` said. That is a font call per visible
                            // character per frame — cheap against the blit next
                            // to it, and it assumes advances add, which the
                            // wrapper already assumes for the same fonts.
                            // Ceiling: a kerned proportional face, where the
                            // drawn text would be a pixel or two loose against
                            // its own measure. Upgrade path is one shaped run
                            // per piece, which needs a shaper this editor does
                            // not have.
                            let mut buf = [0u8; 4];
                            let w = self.advance(ch.encode_utf8(&mut buf), *style);
                            self.draw_glyph(ch, x, top, c, cut);
                            x += w;
                        }
                    }
                }
            }
        }
    }

    /// A hairline around `(x, y, w, h)`, as four fills.
    ///
    /// Inside the rect rather than around it, so a border on a block flush with
    /// the pane's edge is visible rather than clipped away. It takes no part in
    /// layout — see `Block::border` — so a child pushed in only by `pad` may run
    /// under it, which is the ceiling that field already writes down.
    fn stroke(&mut self, x: i32, y: i32, w: i32, h: i32, c: Color) {
        if w <= 0 || h <= 0 {
            return;
        }
        self.fill(x, y, w, 1, c);
        self.fill(x, y + h - 1, w, 1, c);
        self.fill(x, y, 1, h, c);
        self.fill(x + w - 1, y, 1, h, c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: usize, end: usize, kind: HlKind) -> Span {
        Span { start, end, kind }
    }

    #[test]
    fn centering_counts_chars_not_bytes() {
        // 3 chars, 9 bytes. A byte-length assumption gives 1, which is wrong.
        assert_eq!(center_col("███", 11), 4);
        assert_eq!(center_col("abc", 11), 4);
        assert_eq!(center_col("╔═╗", 3), 0);
        // Wider than the window: clamp to the left edge rather than underflow.
        assert_eq!(center_col("████████", 4), 0);
    }

    #[test]
    fn banner_lines_are_box_drawing_only() {
        assert!(is_banner_line(" ███████╗███████╗"));
        assert!(is_banner_line(" ╚══════╝"));
        assert!(!is_banner_line("        a Common Lisp machine that edits text"));
        assert!(!is_banner_line("▸ [f]  Find file")); // ▸ is U+25B8, outside the range
        assert!(!is_banner_line(""));
        assert!(!is_banner_line("   "));
    }

    #[test]
    fn span_crossing_a_line_boundary_is_clipped_and_reused() {
        // One comment span covering "b\nc" in "ab\ncd", i.e. chars 1..4.
        let spans = [span(1, 4, HlKind::Comment)];
        let (next, runs) = spans_for_line(&spans, 0, 0, 2); // line "ab"
        assert_eq!(runs, vec![(1, 2, HlKind::Comment)]);
        // Must NOT be consumed: it still covers the next line.
        assert_eq!(next, 0);
        let (next, runs) = spans_for_line(&spans, next, 3, 5); // line "cd"
        assert_eq!(runs, vec![(0, 1, HlKind::Comment)]);
        assert_eq!(next, 0);
        // A line past its end finally retires it.
        let (next, runs) = spans_for_line(&spans, next, 6, 8);
        assert!(runs.is_empty());
        assert_eq!(next, 1);
    }

    #[test]
    fn span_walk_is_monotonic_across_lines() {
        let spans = [
            span(0, 2, HlKind::Keyword),
            span(3, 5, HlKind::String),
            span(6, 9, HlKind::Number),
        ];
        let (next, runs) = spans_for_line(&spans, 0, 0, 5);
        assert_eq!(
            runs,
            vec![(0, 2, HlKind::Keyword), (3, 5, HlKind::String)]
        );
        assert_eq!(next, 0);
        let (next, runs) = spans_for_line(&spans, next, 6, 9);
        assert_eq!(runs, vec![(0, 3, HlKind::Number)]);
        assert_eq!(next, 2); // the first two are behind us
        assert!(spans_for_line(&spans, next, 20, 25).1.is_empty());
    }

    #[test]
    fn empty_span_list_is_fine() {
        let (next, runs) = spans_for_line(&[], 0, 0, 10);
        assert_eq!(next, 0);
        assert!(runs.is_empty());
    }

    // --- wide characters --------------------------------------------------
    //
    // What a cell *is* belongs to `zemacs_core::display` and is tested there,
    // because `j` and `k` read the same functions. These are the renderer's own
    // consequences of it.

    /// Emoji are the case that used to walk the cursor off the end of a line:
    /// four of them are eight cells, not four, so the cursor on the third lands
    /// on the second row of a four-column pane.
    #[test]
    fn the_cursor_follows_a_wide_character_across_a_wrap() {
        let cells = expand_line("😀😀😀😀", 4);
        assert_eq!(cells.len(), 8);
        assert_eq!(wrap_row_count(cells.len(), 4), 2);
        assert_eq!(cursor_pos(visual_col(&cells, 3), 4, 2), (1, 2));
    }

    /// Chrome measures in cells too — a CJK candidate that fit "by character
    /// count" would run off the end of its box.
    #[test]
    fn chrome_measures_wide_text_in_cells() {
        assert_eq!(truncate("日本語", 6), "日本語");
        assert_eq!(truncate("日本語", 5), "日本…");
        // Cut at 4: 日本 is 4 cells, so only 日 plus the ellipsis fits in 3.
        assert_eq!(truncate("日本語", 4), "日…");
        // A wide character that would only half fit is dropped, never halved.
        assert_eq!(truncate("a漢b", 3), "a…");
        assert_eq!(center_col("日本", 10), 3);
    }

    // --- overlays ---------------------------------------------------------

    fn overlay(id: u64, start: usize, end: usize) -> Overlay {
        Overlay {
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

    /// What a row would read as, so a substitution can be asserted on the text.
    fn text_of(cells: &[(char, usize)]) -> String {
        cells.iter().map(|&(c, _)| c).collect()
    }

    #[test]
    fn a_display_string_replaces_the_cells_it_covers() {
        let cells = expand_line("** heading", 4);
        // org-modern: the two stars become one glyph, and the space after them
        // is deliberately outside the range so the text does not move.
        let subs = vec![(0, 2, "◉".to_string())];
        let out = substitute(&cells, &subs);
        assert_eq!(text_of(&out), "◉ heading");
        // Every cell of the substitution is attributed to its start, so a
        // cursor anywhere inside the hidden range lands on the first of them...
        assert_eq!(visual_col(&out, 0), 0);
        assert_eq!(visual_col(&out, 1), 0);
        // ...and everything after it keeps a column of its own.
        assert_eq!(visual_col(&out, 2), 1);
        assert_eq!(visual_col(&out, 3), 2);
    }

    /// An image reserves blanks rather than drawing anything here: the blit
    /// happens over them, and reserving is what stops the text after an equation
    /// being painted on.
    #[test]
    fn a_substitution_can_be_wider_or_narrower_than_what_it_hides() {
        let cells = expand_line("see $x^2$ here", 4);
        let wide = substitute(&cells, &[(4, 9, " ".repeat(10))]);
        assert_eq!(text_of(&wide), format!("see{}here", " ".repeat(12)));
        let narrow = substitute(&cells, &[(4, 9, "x".to_string())]);
        assert_eq!(text_of(&narrow), "see x here");
        // The column the blit goes in, and the one the text after it resumes at.
        assert_eq!(visual_col(&narrow, 4), 4);
        assert_eq!(visual_col(&narrow, 9), 5);
    }

    #[test]
    fn an_empty_substitution_hides_the_text_entirely() {
        let cells = expand_line("secret", 4);
        assert_eq!(text_of(&substitute(&cells, &[(0, 6, String::new())])), "");
    }

    #[test]
    fn substitutions_are_applied_in_order_and_overlaps_are_dropped() {
        let cells = expand_line("abcdefgh", 4);
        let out = substitute(&cells, &[(1, 3, "X".into()), (5, 7, "Y".into())]);
        assert_eq!(text_of(&out), "aXdeYh");
        // The second starts inside the first: first start wins, and the loser is
        // ignored rather than half-applied.
        let out = substitute(&cells, &[(1, 5, "X".into()), (3, 7, "Y".into())]);
        assert_eq!(text_of(&out), "aXfgh");
    }

    #[test]
    fn tabs_inside_a_substitution_disappear_with_it() {
        let cells = expand_line("a\tb", 4);
        assert_eq!(text_of(&cells), "a   b");
        assert_eq!(text_of(&substitute(&cells, &[(1, 2, "-".into())])), "a-b");
    }

    #[test]
    fn overlay_runs_are_clipped_and_rebased_onto_the_line() {
        // A buffer where line 2 is chars [6, 10).
        let overlays = vec![overlay(1, 0, 20), overlay(2, 7, 8), overlay(3, 10, 12)];
        let runs = overlays_for_line(&overlays, 6, 10);
        assert_eq!(
            runs.iter().map(|&(s, e, o)| (o.id, s, e)).collect::<Vec<_>>(),
            // The third only touches the newline and the line after it.
            vec![(1, 0, 4), (2, 1, 2)]
        );
    }

    #[test]
    fn the_most_recent_overlay_wins_per_attribute() {
        let mut first = overlay(1, 0, 4);
        first.face = Some(HlKind::Keyword);
        first.background = Some(HlKind::Modeline);
        let mut second = overlay(2, 2, 4);
        second.background = Some(HlKind::Comment); // no face of its own
        let overlays = vec![first, second];
        let runs = overlays_for_line(&overlays, 0, 4);
        // Only the first covers cell 0.
        assert_eq!(
            overlay_face(&runs, 0),
            (Some(HlKind::Keyword), Some(HlKind::Modeline))
        );
        // Both cover cell 2: the later background wins, and the earlier
        // foreground survives because the later one never claimed it.
        assert_eq!(
            overlay_face(&runs, 2),
            (Some(HlKind::Keyword), Some(HlKind::Comment))
        );
        assert_eq!(overlay_face(&[], 0), (None, None));
    }

    #[test]
    fn an_image_reserves_at_least_one_cell() {
        assert_eq!(image_cells(0, 10), 1);
        assert_eq!(image_cells(1, 10), 1);
        assert_eq!(image_cells(10, 10), 1);
        assert_eq!(image_cells(11, 10), 2);
        // A degenerate cell width must not divide by zero.
        assert_eq!(image_cells(11, 0), 11);
    }

    /// Rows of variable height, as the one integer they turned out to be.
    ///
    /// `line_h = 20`, `ascent = 15`: a row offers 15 pixels above the baseline
    /// and 5 below, which is roughly what a 14pt monospace face reports.
    #[test]
    fn an_image_claims_rows_above_its_baseline_and_none_below() {
        let rows = |h, depth| image_rows(h, depth, 20, 15);
        // Everything that fits under the ascent is one row — every inline
        // fragment, since `$x_1$` is typeset at the text's own size.
        assert_eq!(rows(0, 0), 1);
        assert_eq!(rows(15, 0), 1);
        // ...including one that is *taller than the line* but hangs the excess
        // below the baseline, which is exactly what `depth` is for. This is the
        // case the old `h <= line_h` test got wrong.
        assert_eq!(rows(19, 4), 1);
        // Past the ascent, whole rows, and they are claimed upward.
        assert_eq!(rows(16, 0), 2);
        assert_eq!(rows(35, 0), 2);
        assert_eq!(rows(36, 0), 3);
        // Depth is free: it lands in the last row's descender space.
        assert_eq!(rows(40, 5), 2);
        // Degenerate inputs must neither divide by zero nor go below one row.
        assert_eq!(rows(0, 99), 1);
        assert_eq!(image_rows(100, 0, 0, 15), 86);
    }

    /// The placement rule, spelled the way [`Renderer::draw_image`] computes it,
    /// so the two cannot drift: the image's baseline is the last row's baseline.
    #[test]
    fn a_tall_image_grows_into_its_own_rows_and_never_over_the_line_above() {
        let (line_h, ascent) = (20, 15);
        let iy = |h: i32, depth: i32| {
            let rows = image_rows(h as u32, depth as u32, line_h, ascent) as i32;
            (rows - 1) * line_h + ascent + depth - h
        };
        // An inline fragment sits on the baseline and hangs `depth` below it —
        // 15 - 12 = 3 pixels down from the top of its one row.
        assert_eq!(iy(12, 0), 3);
        assert_eq!(iy(19, 4), 0);
        // A display equation never starts above the top of the block it was
        // given, which is the whole point: `>= 0` is "does not paint over the
        // line above", and `< line_h` is "does not leave a blank row under it".
        for h in 1..400 {
            let top = iy(h, 0);
            assert!((0..line_h).contains(&top), "{h}px starts at {top}");
        }
    }

    /// A folded line occupies no row, which is the one thing an overlay could
    /// not say before: every other payload swaps cells for cells, and this makes
    /// rows stop existing.
    ///
    /// Asserted through `visible_lines` rather than through `line_rows`, because
    /// the number that matters is the one core clamps `scroll` against — a fold
    /// that hid rows without also being counted as the buffer lines it spans
    /// would make the bottom of the file unreachable.
    #[test]
    fn a_folded_line_occupies_no_row_but_is_still_a_line() {
        let text = "a\nb\nc\nd\ne\nf\n";
        let set = Settings::default(); // truncating, one row per drawn line
        let plain = Buffer::from_str(text);
        // Four rows of pane, nothing folded: four lines.
        assert_eq!(visible_lines(&plain, 0, 4, 20, 1, &set), 4);

        // Fold "b" and "c" under "a" — chars 0..5 of "a\nb\nc\n…".
        let mut ed = Editor::new();
        ed.buffer = Buffer::from_str(text);
        let id = ed.make_overlay(0, 5);
        ed.apply(zemacs_core::EditorCommand::Overlay(
            zemacs_core::OverlayEdit::Fold(id, true),
        ));
        // Six buffer lines now fit in four rows: a, (b), (c), d, e, f — the two
        // hidden ones cost nothing and are still counted, so `scroll` can step
        // past them.
        assert_eq!(visible_lines(&ed.buffer, 0, 4, 20, 1, &set), 6);
        // The contrast, in a two-row pane: two lines without the fold, four
        // with it, because two of the four are free.
        assert_eq!(visible_lines(&plain, 0, 2, 20, 1, &set), 2);
        assert_eq!(visible_lines(&ed.buffer, 0, 2, 20, 1, &set), 4);
        // The head line is drawn, the two under it are not, the rest are.
        let folded = |l: usize| fold_hiding(ed.buffer.overlays(), ed.buffer.line_start(l));
        assert_eq!((folded(0), folded(1), folded(2), folded(3)),
                   (None, Some(0), Some(0), None));
        // ...and it says so, on the line that is still drawn.
        assert!(fold_starts_in(ed.buffer.overlays(), 0, 1));
        assert!(!fold_starts_in(ed.buffer.overlays(), 2, 3));
    }

    // --- typesetting ------------------------------------------------------

    /// The bound on the face cache, asserted where it is actually enforced.
    /// Every other number in this file is layout; this one is memory.
    #[test]
    fn a_scale_is_snapped_to_one_of_the_few_sizes_a_face_opens_at() {
        assert_eq!(scale_step(100), 100);
        assert_eq!(scale_step(150), 150);
        // Nearest, not next up: 1.4 is a 1.5 and 1.1 is body size. A config
        // asking for a size between two steps gets the closer one rather than a
        // font of its own.
        assert_eq!(scale_step(140), 150);
        assert_eq!(scale_step(110), 100);
        assert_eq!(scale_step(113), 125);
        // Out of range clamps rather than opening a face nobody budgeted for.
        assert_eq!(scale_step(1), 100);
        assert_eq!(scale_step(4000), 200);
        // ...and the declared bound covers the whole key space, which is the
        // claim `Renderer::faces` makes in prose.
        assert_eq!(MAX_FACES, SCALE_STEPS.len() * 4);
        assert!(SCALE_STEPS.contains(&100), "body size must be a step");
    }

    /// A scaled line's grid: wider cells, fewer columns, more rows — and every
    /// one of them a whole number, which is the entire design. Nothing in the
    /// renderer ever holds a fractional row height.
    #[test]
    fn a_scaled_line_is_wider_and_taller_by_whole_cells() {
        // A hundred pixels of text width and a ten-pixel cell.
        let at = |pct| {
            line_box(
                &LineStyle {
                    scale: pct,
                    ..Default::default()
                },
                100,
                10,
            )
        };
        // Body size, and the case that matters most: identical to what this was
        // before any of it existed. `0` is "nothing claimed a size".
        let body = LineBox { cw: 10, tall: 1, indent: 0, cols: 10 };
        assert_eq!(at(0), body);
        assert_eq!(at(100), body);
        // 1.5×: fifteen-pixel cells, so six fit where ten did, and the line
        // claims two rows because its em box is one and a half of them.
        assert_eq!(at(150), LineBox { cw: 15, tall: 2, indent: 0, cols: 6 });
        // 2× is *still* two rows — `ceil`, so nothing between 1 and 2 costs a
        // third — and 2.5× is three.
        assert_eq!(at(200), LineBox { cw: 20, tall: 2, indent: 0, cols: 5 });
        assert_eq!(at(250), LineBox { cw: 25, tall: 3, indent: 0, cols: 4 });
        // A degenerate cell must not make a zero-wide one two divisions later.
        assert_eq!(line_box(&LineStyle::default(), 100, 0).cw, 1);
    }

    /// A prefix takes its own width off the front, and the columns left over are
    /// the ones every loop downstream counts in — which is what makes a wrapped
    /// quotation line up under itself instead of under the pane's edge.
    #[test]
    fn a_line_prefix_takes_columns_off_the_front() {
        let quote = |scale| LineStyle {
            scale,
            prefix: Some(("| ", None)),
            ..Default::default()
        };
        assert_eq!(
            line_box(&quote(0), 100, 10),
            LineBox { cw: 10, tall: 1, indent: 20, cols: 8 }
        );
        // Measured in the line's *own* cells, so an indented heading is indented
        // by heading-sized ones and its bar lines up with its type.
        assert_eq!(
            line_box(&quote(150), 100, 10),
            LineBox { cw: 15, tall: 2, indent: 30, cols: 4 }
        );
    }

    /// The line attributes, resolved the way the draw loop resolves them.
    #[test]
    fn line_attributes_take_the_widest_scale_and_the_latest_of_the_rest() {
        let mut small = overlay(1, 0, 4);
        small.scale = Some(125);
        small.line_background = Some(HlKind::Code);
        let mut big = overlay(2, 2, 4);
        big.scale = Some(150);
        let mut last = overlay(3, 0, 4);
        last.scale = Some(100);
        last.line_background = Some(HlKind::Comment);
        let all = [small, big, last];
        let style = line_style(&all, 0, 4);
        // The widest wins rather than the latest, and it is the one place this
        // file departs from "most recent wins": the row block has to hold every
        // run on the line, so a later body-size overlay must not be able to clip
        // an earlier heading's glyphs into the line underneath.
        assert_eq!(style.scale, 150);
        // Everything else is last-wins, exactly as a face is.
        assert_eq!(style.background, Some(HlKind::Comment));
        // An overlay that only *touches* the line still styles it: that is what
        // lets org scale a heading with an overlay over its stars.
        let mut stars = overlay(4, 0, 1);
        stars.scale = Some(200);
        let one = [stars];
        assert_eq!(line_style(&one, 0, 40).scale, 200);
        assert_eq!(line_style(&[], 0, 4), LineStyle::default());
    }

    /// Weight and slant stack per attribute the way a face does — and `normal`
    /// is a claim, which is the whole reason they are tri-state.
    #[test]
    fn weight_and_slant_stack_per_attribute_like_a_face() {
        let mut first = overlay(1, 0, 4);
        first.bold = Some(true);
        first.italic = Some(true);
        let mut second = overlay(2, 2, 4);
        second.bold = Some(false); // upright, deliberately
        let both = [first, second];
        let runs = overlays_for_line(&both, 0, 4);
        assert_eq!(overlay_emphasis(&runs, 0), (true, true));
        // The later overlay takes the bold off and leaves the italic alone,
        // which is the difference between `Some(false)` and `None`.
        assert_eq!(overlay_emphasis(&runs, 2), (false, true));
        assert_eq!(overlay_emphasis(&[], 0), (false, false));
    }

    /// The counting side of a scaled line. `visible_lines` and `line_rows` have
    /// to spend the same rows on it that the draw loop does, or `scroll` clamps
    /// against a screen nobody is looking at.
    ///
    /// The same thing `a_folded_line_occupies_no_row_but_is_still_a_line`
    /// asserts, pointing the other way: a fold is a height of zero and a scale
    /// is a height of two.
    #[test]
    fn a_scaled_line_claims_whole_extra_rows() {
        let text = "one\ntwo\nthree\nfour\n";
        let set = Settings::default(); // truncating, one row per body line
        let plain = Buffer::from_str(text);
        let mut ed = Editor::new();
        ed.buffer = Buffer::from_str(text);
        // Over "one" alone — a heading's stars, in the shape org-modern uses.
        let id = ed.make_overlay(0, 3);
        ed.apply(zemacs_core::EditorCommand::Overlay(
            zemacs_core::OverlayEdit::Scale(id, Some(150)),
        ));

        // A hundred pixels of text at a ten-pixel cell. Four rows of pane: the
        // heading eats two of them, so three buffer lines are on screen where
        // four were.
        assert_eq!(visible_lines(&plain, 0, 4, 100, 10, &set), 4);
        assert_eq!(visible_lines(&ed.buffer, 0, 4, 100, 10, &set), 3);
        // ...and everything below it has moved down by the row it took: the
        // rows spent before line 1 are 1 without the scale and 2 with it.
        let spent = |b: &Buffer| line_rows(b, 0, 100, 10, false, set.tab_width);
        assert_eq!(spent(&plain), 1);
        assert_eq!(spent(&ed.buffer), 2);
        // Per line, not per buffer: only the line the overlay touches grew.
        assert_eq!(line_rows(&ed.buffer, 0, 100, 10, false, 4), 2);
        assert_eq!(line_rows(&ed.buffer, 1, 100, 10, false, 4), 1);
        // A scale of exactly body size is not a scale, so the fast path in
        // `visible_lines` is still allowed to skip the whole walk.
        ed.apply(zemacs_core::EditorCommand::Overlay(
            zemacs_core::OverlayEdit::Scale(id, Some(100)),
        ));
        assert!(!ed.buffer.overlays().iter().any(reshapes_lines));
        assert_eq!(visible_lines(&ed.buffer, 0, 4, 100, 10, &set), 4);
    }

    /// A click on a typeset line reads *that line's* grid. The draw loop and
    /// [`offset_at`] are twins, and a scale is the newest way for them to come
    /// apart: a heading's cells are wider and its rows are taller, so a pointer
    /// halfway across one is not halfway along its text.
    #[test]
    fn a_click_on_a_scaled_line_reads_its_own_grid() {
        let set = Settings {
            line_numbers: false, // no gutter, so a column is a column
            ..Settings::default()
        };
        let pane = Area { x: 0, y: 0, w: 40 * CW + 2 * PAD, h: 9 * LH + 2 * PAD };
        let mut ed = Editor::new();
        ed.buffer = Buffer::from_str("HEADING\nbody text\n");
        let id = ed.make_overlay(0, 7);
        ed.apply(zemacs_core::EditorCommand::Overlay(
            zemacs_core::OverlayEdit::Scale(id, Some(200)),
        ));
        let buf = &ed.buffer;
        let win = Window {
            id: 0,
            buffer: buf.id,
            cursor: 0,
            scroll: 0,
            viewport_lines: 8,
            wrap_cols: 0,
        };
        let doc = doc_rect(pane, STATUS, 0);
        // A pixel, rather than a column: which column it is, is the question.
        let hit = |row: i32, px: i32| {
            offset_at(buf, &win, &set, pane, STATUS, LH, CW,
                      doc.x + px, doc.y + row * LH + LH / 2)
        };

        // The heading's cells are twice as wide, so the character under a
        // pointer two body-cells along is the *first* one, not the third.
        assert_eq!(hit(0, CW + CW / 2), 0, "still 'H' at 2x");
        assert_eq!(hit(0, 2 * CW + CW / 2), 1, "'E'");
        // The heading owns both of the first two rows, so row 1 is still it...
        assert_eq!(hit(1, CW / 2), 0);
        // ...and the body line only begins on the third.
        assert_eq!(hit(2, CW / 2), buf.line_start(1));
        assert_eq!(hit(2, CW + CW / 2), buf.line_start(1) + 1, "body is 1x again");
    }

    /// The which-key panel is a *grid*, and this is all there is to it: how
    /// short can it be and still fit across the window.
    #[test]
    fn a_which_key_panel_is_as_short_as_the_window_is_wide() {
        // Eight `f +file`-sized entries: 9 cells each, 11 with the gutter.
        let rows = |cols, room| which_key_rows(9, 8, cols, room);
        assert_eq!(rows(120, 10), 1); // ten columns fit: one row
        assert_eq!(rows(44, 10), 2); // four fit: two rows
        assert_eq!(rows(30, 10), 4); // two fit
        // Narrower than one column still draws one per row rather than none.
        assert_eq!(rows(4, 10), 8);
        assert_eq!(rows(0, 10), 8);
        // ...and never taller than the room above the status line. The tail is
        // dropped; Lisp has already said how many it left out.
        assert_eq!(rows(30, 3), 3);
        assert_eq!(rows(30, 0), 1);
        // A degenerate width must not divide by zero.
        assert_eq!(which_key_rows(0, 3, 0, 10), 3);
    }

    // --- line overflow ----------------------------------------------------

    /// The glyphs a row shows, so a wrap can be asserted on what you'd read.
    fn row_text(cells: &[(char, usize)], (s, e): (usize, usize)) -> String {
        cells[s..e].iter().map(|&(c, _)| c).collect()
    }

    /// `visible_lines` for `text` wrapped in a `cols`-wide, `rows`-tall pane —
    /// the path the renderer actually takes, so a wrap measured in the wrong
    /// unit shows up here rather than only in the helper it was measured with.
    ///
    /// A cell one pixel wide throughout the tests below, which is what makes a
    /// pixel width and a column count the same number and keeps every assertion
    /// readable. A scaled line is the one place that matters — see
    /// `a_scaled_line_is_wider_and_taller_by_whole_cells`, which uses a real
    /// cell so the arithmetic has somewhere to go wrong.
    fn wrapped_lines(text: &str, rows: usize, cols: usize) -> usize {
        let set = Settings {
            line_overflow: LineOverflow::Wrap,
            ..Settings::default()
        };
        visible_lines(&Buffer::from_str(text), 0, rows, cols as i32, 1, &set)
    }

    #[test]
    fn a_line_wraps_into_whole_rows_of_the_panes_width() {
        let rows = |n: usize, w: usize| wrap_rows(n, w).collect::<Vec<_>>();
        // Narrower than the pane: one row, and it is the whole line.
        assert_eq!(rows(3, 4), vec![(0, 3)]);
        // Exactly the pane: still one row. A second, empty one would look like
        // a blank line in the file.
        assert_eq!(rows(4, 4), vec![(0, 4)]);
        // One over: the overflow gets a row of its own.
        assert_eq!(rows(5, 4), vec![(0, 4), (4, 5)]);
        // Three panefuls: three full rows, no fourth.
        assert_eq!(rows(12, 4), vec![(0, 4), (4, 8), (8, 12)]);
        assert_eq!(rows(13, 4), vec![(0, 4), (4, 8), (8, 12), (12, 13)]);
        // Empty line: one row to hold the cursor and the gutter number.
        assert_eq!(rows(0, 4), vec![(0, 0)]);
        for (n, w) in [(3, 4), (4, 4), (5, 4), (12, 4), (0, 4), (7, 3)] {
            assert_eq!(rows(n, w).len(), wrap_row_count(n, w), "{n}/{w}");
            // Every row is inside the line and they tile it end to end.
            assert_eq!(rows(n, w).last().unwrap().1, n, "{n}/{w}");
            assert!(rows(n, w).windows(2).all(|p| p[0].1 == p[1].0), "{n}/{w}");
            assert!(rows(n, w).iter().all(|&(s, e)| e - s <= w), "{n}/{w}");
        }
    }

    #[test]
    fn a_degenerate_pane_width_terminates() {
        // Zero columns: a pane narrower than its own gutter. Dividing by it is
        // the infinite loop; one empty row is the answer that lets the caller
        // move on to the next line.
        assert_eq!(wrap_row_count(500, 0), 1);
        assert_eq!(wrap_rows(500, 0).collect::<Vec<_>>(), vec![(0, 0)]);
        assert_eq!(wrap_rows(0, 0).collect::<Vec<_>>(), vec![(0, 0)]);
        assert_eq!(truncation_marker(500, 0), None); // nowhere to put it
        assert_eq!(cursor_pos(7, 0, 1), (0, 0));
        // One column: one row per character, and it does terminate.
        assert_eq!(wrap_row_count(5, 1), 5);
        assert_eq!(
            wrap_rows(5, 1).collect::<Vec<_>>(),
            vec![(0, 1), (1, 2), (2, 3), (3, 4), (4, 5)]
        );
        assert_eq!(truncation_marker(5, 1), Some(0));
    }

    #[test]
    fn wrapping_measures_display_cells_not_source_chars() {
        // "a\tb" is 3 source chars but 5 cells: 'a', three spaces to the tab
        // stop, 'b'. Wrapping the chars would fit it on one 4-column row.
        let cells = expand_line("a\tb", 4);
        assert_eq!(cells.len(), 5);
        let rows = wrap_rows(cells.len(), 4).collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        assert_eq!(row_text(&cells, rows[0]), "a   ");
        assert_eq!(row_text(&cells, rows[1]), "b");
        // ...and the cell that starts row 1 still points at its source char.
        assert_eq!(cells[rows[1].0].1, 2);
        // Same at the call site, which is what a source-char implementation
        // would actually get wrong: "a\tb" costs two of the pane's twenty rows
        // where the three-character "abc" costs one. (Both files also have the
        // empty line their trailing newline leaves behind.)
        assert_eq!(wrapped_lines("a\tb\n", 20, 4), 2 + 17);
        assert_eq!(wrapped_lines("abc\n", 20, 4), 2 + 18);
    }

    #[test]
    fn wrapping_measures_characters_not_bytes() {
        // 5 chars, 10 bytes. A byte count wraps this into 4 rows and slices a
        // codepoint in half doing it.
        let cells = expand_line("ααααα", 4);
        assert_eq!(cells.len(), 5);
        assert_eq!(wrap_row_count(cells.len(), 3), 2);
        let rows = wrap_rows(cells.len(), 3).collect::<Vec<_>>();
        assert_eq!(row_text(&cells, rows[0]), "ααα");
        assert_eq!(row_text(&cells, rows[1]), "αα");
        // Box-drawing art from the dashboard banner, same story.
        let cells = expand_line("███████╗", 4);
        assert_eq!(cells.len(), 8);
        assert_eq!(wrap_row_count(cells.len(), 8), 1);
        assert_eq!(wrap_row_count(cells.len(), 4), 2);
        // At the call site: ten Greek letters are ten cells and twenty bytes,
        // so a byte-measured pane wraps them into five rows instead of three
        // and reports 16 lines where the answer is 18.
        assert_eq!(wrapped_lines("αααααααααα\n", 20, 4), 2 + 16);
        assert_eq!(wrapped_lines("aaaaaaaaaa\n", 20, 4), 2 + 16);
        // Three cells is one row; three bytes would be two.
        assert_eq!(wrapped_lines("ααα\n", 20, 4), 2 + 18);
    }

    #[test]
    fn the_marker_replaces_the_last_column_only_when_text_is_hidden() {
        assert_eq!(truncation_marker(9, 10), None); // fits with room to spare
        assert_eq!(truncation_marker(10, 10), None); // fits exactly: no lie
        assert_eq!(truncation_marker(11, 10), Some(9)); // last column, not the 11th
        assert_eq!(truncation_marker(500, 10), Some(9));
        assert_eq!(truncation_marker(0, 10), None);
        // It lands *inside* the pane: the whole point is that it is not itself
        // the overflow it warns about.
        let cols = 30;
        let mc = truncation_marker(500, cols).unwrap();
        assert!(mc < cols);
        let pane = Area { x: 0, y: 0, w: 2 * PAD + cols as i32 * CW, h: 600 };
        let doc = doc_rect(pane, mh(), 0);
        assert!(doc.x + (mc as i32 + 1) * CW <= pane.x + pane.w);
        // And it takes the place of a character rather than following one: the
        // glyphs drawn plus the marker are exactly one paneful.
        assert_eq!(mc + 1, cols);
    }

    #[test]
    fn a_wrapped_offset_round_trips_to_its_row_and_column() {
        let cells = expand_line("abcdefghij", 4);
        let rows = wrap_rows(cells.len(), 4).collect::<Vec<_>>();
        for vc in 0..cells.len() {
            let (r, c) = cursor_pos(vc, 4, rows.len());
            assert!(c < 4, "{vc} -> {r},{c}");
            assert_eq!(rows[r].0 + c, vc, "{vc} -> {r},{c}");
            assert!(vc >= rows[r].0 && vc < rows[r].1, "{vc} -> {r},{c}");
        }
        // Same for a tab-expanded line, where cells and chars disagree.
        let cells = expand_line("\tfn main() {", 4);
        let rows = wrap_rows(cells.len(), 5).collect::<Vec<_>>();
        for src in [0usize, 1, 5] {
            let vc = visual_col(&cells, src);
            let (r, c) = cursor_pos(vc, 5, rows.len());
            assert_eq!(rows[r].0 + c, vc);
        }
    }

    #[test]
    fn the_cursor_past_the_last_column_is_parked_in_the_margin() {
        // End of a line that exactly fills its last row: cell 8 belongs to a
        // row this line does not own, so it comes back to the trailing margin
        // of row 1 rather than landing on the next buffer line.
        assert_eq!(cursor_pos(8, 4, 2), (1, 4));
        assert_eq!(cursor_pos(7, 4, 2), (1, 3)); // the last character itself
        // Truncated: the cursor is somewhere off to the right, and the margin
        // is the only place left to say so. Never past it.
        for vc in [30, 31, 500] {
            assert_eq!(cursor_pos(vc, 30, 1), (0, 30));
        }
        assert_eq!(cursor_pos(0, 30, 1), (0, 0));
        // A margin cursor is still inside its pane once trimmed — this is the
        // arithmetic `draw_cursor` protects.
        let cols = 30i32;
        let pane = Area { x: 0, y: 0, w: 2 * PAD + cols * CW, h: 600 };
        let doc = doc_rect(pane, mh(), 0);
        let x = doc.x + cols * CW;
        assert!(x < pane.x + pane.w, "the margin is inside the pane");
        assert!(CW.min(pane.x + pane.w - x) > 0, "and wide enough to see");
    }

    #[test]
    fn a_selection_is_clipped_to_each_row_it_crosses() {
        // Cells 2..9 of a line wrapped at 4: part of row 0, all of row 1, part
        // of row 2.
        assert_eq!(row_span((2, 9), 0, 4), Some((2, 4)));
        assert_eq!(row_span((2, 9), 4, 4), Some((0, 4)));
        assert_eq!(row_span((2, 9), 8, 4), Some((0, 1)));
        assert_eq!(row_span((2, 9), 12, 4), None); // past the selection
        assert_eq!(row_span((2, 9), 0, 0), None); // zero-column pane
        // The trailing newline cell of a linewise selection: shown when the row
        // has a column spare, dropped when it would land past the pane edge.
        assert_eq!(row_span((0, 5), 0, 8), Some((0, 5)));
        assert_eq!(row_span((0, 5), 0, 4), Some((0, 4)));
        // Never wider than the pane, whatever it is handed.
        for &(s, e) in &[(0usize, 500usize), (0, 5), (7, 9), (100, 200)] {
            if let Some((a, b)) = row_span((s, e), 0, 30) {
                assert!(b <= 30 && a < b, "{s}..{e} -> {a}..{b}");
            }
        }
    }

    #[test]
    fn viewport_lines_counts_buffer_lines_not_rows() {
        const ROWS: usize = 40;
        // Truncation: one row each, so the pane's full capacity, file length
        // notwithstanding — unchanged from before wrapping existed.
        assert_eq!(lines_in_rows(std::iter::repeat_n(1, 100), ROWS), ROWS);
        // A short file leaves rows empty; they count one line each, or `scroll`
        // would be clamped further down every frame near the end of the file.
        assert_eq!(lines_in_rows([1, 1, 1], ROWS), ROWS);
        // Wrapped: a line worth several rows crowds the others out.
        assert_eq!(lines_in_rows([10, 10, 10, 10, 1, 1], ROWS), 4);
        assert_eq!(lines_in_rows([2, 2, 2, 2], ROWS), 4 + 32);
        // One line taller than the whole pane still counts as one, so scrolling
        // can step over it instead of getting stuck on it.
        assert_eq!(lines_in_rows([100, 1, 1], ROWS), 1);
        assert_eq!(lines_in_rows([100], ROWS), 1);
        // A pane with no room for text advertises no lines: core reads that as
        // zero and clamps, where a lie about one row would scroll by it.
        assert_eq!(lines_in_rows([1, 1], 0), 0);
        assert_eq!(lines_in_rows(std::iter::empty(), 0), 0);
        // Past the end of the buffer: still the pane's capacity, never zero.
        assert_eq!(lines_in_rows(std::iter::empty(), ROWS), ROWS);
    }

    #[test]
    fn a_wrapped_buffer_reports_fewer_lines_than_a_truncated_one() {
        // Four lines: 100 characters, "short", "short", and the empty line the
        // trailing newline leaves behind.
        let buf = Buffer::from_str(&format!("{}\nshort\nshort\n", "x".repeat(100)));
        let mut set = Settings { line_numbers: false, ..Settings::default() };
        // Truncated, every line is one row, so the answer is the pane's height
        // whatever the buffer looks like.
        assert_eq!(visible_lines(&buf, 0, 20, 10, 1, &set), 20);
        set.line_overflow = LineOverflow::Wrap;
        // 100 cells in a 10-column pane is 10 rows for the first line alone,
        // then three one-row lines, then seven rows of nothing.
        assert_eq!(visible_lines(&buf, 0, 20, 10, 1, &set), 4 + 7);
        // Scrolled past the long line, the wrapped and truncated counts agree
        // again — nothing on screen is wider than the pane.
        assert_eq!(visible_lines(&buf, 1, 20, 10, 1, &set), 20);
        // Tall enough to matter: the long line alone fills a 5-row pane, and
        // the count must not drop to zero or scrolling stops dead.
        assert_eq!(visible_lines(&buf, 0, 5, 10, 1, &set), 1);
        assert_eq!(visible_lines(&buf, 0, 0, 10, 1, &set), 0);
        // A pane with no columns must not hang: every line is one row.
        assert_eq!(visible_lines(&buf, 0, 5, 0, 1, &set), 4 + 1);
    }

    // --- the gutter -------------------------------------------------------

    /// One number per *buffer line*, counted in buffer lines.
    ///
    /// This is the assertion that changed when the gutter stopped counting
    /// screen rows. The old rule numbered every visual row, so a line wrapped
    /// across three of them showed three descending numbers and read as three
    /// lines — and a wrapped line holding the cursor showed its absolute number
    /// on one row and a relative number on the others. See `gutter_number` for
    /// what that bought and why it was not worth it.
    #[test]
    fn relative_line_numbers_count_buffer_lines() {
        // Cursor on line 4. Distances are in lines, whatever any of them are
        // doing on screen.
        let label = |line| gutter_number(line, 4, true);
        assert_eq!((label(1), label(2), label(3)), (3, 2, 1));
        assert_eq!((label(5), label(6)), (1, 2), "and below it too");
        // The cursor's own line keeps the absolute buffer line number, 1-based.
        assert_eq!(gutter_number(4, 4, true), 5);
        // With relative off, every line is absolute.
        assert_eq!(gutter_number(1, 4, false), 2);
        assert_eq!(gutter_number(4, 4, false), 5);
    }

    /// ...and a wrapped line is numbered once, on the row it starts on.
    ///
    /// Asserted through the draw loop's own condition rather than through
    /// `gutter_number`, because "how many numbers does a wrapped line get" is a
    /// question about the loop, and it is the half of this the user actually
    /// sees. `r` is the visual row within the line.
    #[test]
    fn a_wrapped_line_gets_one_number_on_its_first_row() {
        let on = Settings::default();
        let buf = Buffer::from_str("a long line that wraps\n");
        // The draw loop's own condition. `gutter_on` and not `set.line_numbers`,
        // because the buffer gets the last word — see
        // `the_gutter_is_drawn_exactly_when_columns_were_reserved_for_it`.
        let numbered = |r: usize, set: &Settings| gutter_on(&buf, set) && r == 0;
        assert!(numbered(0, &on));
        assert!(!numbered(1, &on), "a continuation row's gutter stays blank");
        assert!(!numbered(2, &on));

        // ...including in relative mode, which is the case that was wrong: this
        // used to be `r == 0 || relative`, so every row got one.
        let rel = Settings { relative_line_numbers: true, ..on };
        assert!(numbered(0, &rel));
        assert!(!numbered(1, &rel));

        let off = Settings { line_numbers: false, ..on };
        assert!(!numbered(0, &off));
    }

    /// A click lands on the character under the pointer, through every layout
    /// step between the two: the gutter, the pane's scroll, a tab spread over
    /// several cells, a wide glyph spread over two, and a wrapped line spread
    /// over several rows.
    #[test]
    fn a_click_lands_on_the_character_under_it() {
        // Big enough for eight rows of text plus a modeline.
        let pane = Area { x: 0, y: 0, w: 40 * CW, h: 9 * LH + 2 * PAD };
        let set = Settings::default();
        let buf = Buffer::from_str("alpha\n\tbeta\n漢字 kanji\nomega");
        let win = Window {
            id: 0,
            buffer: buf.id,
            cursor: 0,
            scroll: 0,
            viewport_lines: 8,
            wrap_cols: 0,
        };
        let doc = doc_rect(pane, STATUS, 0);
        let gutter = gutter_w(&buf, &set, CW);
        // The centre of the cell at (row, col), which is where a pointer is.
        let hit = |row: i32, col: i32| {
            let x = doc.x + gutter + col * CW + CW / 2;
            let y = doc.y + row * LH + LH / 2;
            offset_at(&buf, &win, &set, pane, STATUS, LH, CW, x, y)
        };

        assert_eq!(hit(0, 0), 0, "first character of the file");
        assert_eq!(hit(0, 3), 3, "'h' of alpha");
        // Past the end of a five-character line: one past the last character,
        // never the newline and never the line below.
        assert_eq!(hit(0, 30), 5);
        // A click in the gutter is column zero of that row, not a negative one.
        assert_eq!(
            offset_at(&buf, &win, &set, pane, STATUS, LH, CW, doc.x, doc.y + LH / 2),
            0
        );

        // Line 1 is "\tbeta": the tab is one character occupying `tab_width`
        // cells, so every cell across it answers the tab itself, and 'b' is at
        // the cell after them.
        let line1 = buf.line_start(1);
        assert_eq!(hit(1, 0), line1);
        assert_eq!(hit(1, set.tab_width as i32 - 1), line1, "still the tab");
        assert_eq!(hit(1, set.tab_width as i32), line1 + 1, "'b' of beta");

        // Line 2 is "漢字 kanji": two cells per kanji, and clicking either half
        // of one lands on the character rather than beside it.
        let line2 = buf.line_start(2);
        assert_eq!(hit(2, 0), line2);
        assert_eq!(hit(2, 1), line2, "the right half of 漢 is still 漢");
        assert_eq!(hit(2, 2), line2 + 1, "字");
        assert_eq!(hit(2, 4), line2 + 2, "the space");

        // Scrolling moves what the top row shows, and nothing else.
        let scrolled = Window { scroll: 2, ..win };
        assert_eq!(
            offset_at(&buf, &scrolled, &set, pane, STATUS, LH, CW,
                      doc.x + gutter + CW / 2, doc.y + LH / 2),
            line2
        );

        // Below the last line: the end of the buffer, as Emacs does.
        assert_eq!(hit(7, 0), buf.len_chars());
    }

    /// Wrapping is the case the arithmetic is easiest to get wrong: one buffer
    /// line owns several pane rows, so the rows spent before the next line are
    /// not the lines above it.
    #[test]
    fn a_click_on_a_wrapped_line_counts_rows_not_lines() {
        let set = Settings {
            line_overflow: LineOverflow::Wrap,
            line_numbers: false, // no gutter, so a column is a column
            ..Settings::default()
        };
        // Ten columns of text; "0123456789abcdefghij" is exactly two rows.
        let pane = Area { x: 0, y: 0, w: 10 * CW + 2 * PAD, h: 9 * LH + 2 * PAD };
        let buf = Buffer::from_str("0123456789abcdefghij\ntail");
        let win = Window {
            id: 0,
            buffer: buf.id,
            cursor: 0,
            scroll: 0,
            viewport_lines: 8,
            wrap_cols: 10,
        };
        let doc = doc_rect(pane, STATUS, 0);
        let hit = |row: i32, col: i32| {
            offset_at(&buf, &win, &set, pane, STATUS, LH, CW,
                      doc.x + col * CW + CW / 2, doc.y + row * LH + LH / 2)
        };

        assert_eq!(hit(0, 0), 0);
        assert_eq!(hit(0, 9), 9, "last cell of the first row");
        assert_eq!(hit(1, 0), 10, "'a' — the continuation row, same buffer line");
        assert_eq!(hit(1, 9), 19);
        // Only *now* does the second buffer line begin, on the third pane row.
        assert_eq!(hit(2, 0), buf.line_start(1));
    }

    /// A centred measure is only worth having if the mouse agrees with it, so
    /// this is the same click test one pane-width to the right: with
    /// `text_width` set, column zero is no longer at the pane's left edge, and
    /// every step between a pixel and a character has to have moved with it.
    ///
    /// The failure this exists to catch is the tempting one — centring by
    /// shifting the glyphs at draw time and leaving the layout alone — which
    /// looks perfect until you click, and then puts point a dozen characters to
    /// the left of the pointer.
    #[test]
    fn a_click_lands_on_the_character_under_it_in_a_centred_measure() {
        // A 60-column pane holding a 20-column measure: 20 columns of margin on
        // each side, which is a large enough offset that an un-inset hit test
        // could not accidentally pass.
        let set = Settings {
            line_numbers: false, // no gutter, so a column is a column
            text_width: 20,
            ..Settings::default()
        };
        let pane = Area { x: 0, y: 0, w: 60 * CW + 2 * PAD, h: 9 * LH + 2 * PAD };
        //                            0         1         2         3
        //                            0123456789012345678901234567890123
        let buf = Buffer::from_str("alpha beta gamma delta epsilon zeta\nsecond line");
        let win = Window {
            id: 0,
            buffer: buf.id,
            cursor: 0,
            scroll: 0,
            viewport_lines: 8,
            wrap_cols: 20,
        };

        let doc = doc_rect(pane, STATUS, measure_px(&set, CW));
        assert_eq!(doc.w, 20 * CW, "the measure is the text column's width");
        assert_eq!(doc.x, PAD + 20 * CW, "...and it is centred in the pane");

        let hit = |col: i32| {
            offset_at(&buf, &win, &set, pane, STATUS, LH, CW,
                      doc.x + col * CW + CW / 2, doc.y + LH / 2)
        };
        assert_eq!(hit(0), 0, "the first character, twenty columns in");
        assert_eq!(hit(6), 6, "'b' of beta");
        // Left of the measure is column zero of that row, exactly as a click in
        // the gutter is: the margin is chrome, not text you can point into.
        assert_eq!(
            offset_at(&buf, &win, &set, pane, STATUS, LH, CW, pane.x, doc.y + LH / 2),
            0
        );
        // Right of it, the margin is not text: the pointer is clamped to one
        // column past the measure rather than answering the 40th character of a
        // line that is only twenty columns wide on screen. Column 19 carries
        // the truncation marker, so character 20 is exactly what that marker
        // stands for — the first of the tail it is hiding.
        assert_eq!(
            offset_at(&buf, &win, &set, pane, STATUS, LH, CW,
                      pane.x + pane.w - 1, doc.y + LH / 2),
            20
        );

        // Turning the measure off moves the *same pixel* twenty columns along
        // the line, which is the proof that the two layouts really do differ
        // and that `doc_rect` is the only thing that made them.
        let full = Settings { text_width: 0, ..set };
        assert_eq!(
            offset_at(&buf, &win, &full, pane, STATUS, LH, CW,
                      doc.x + CW / 2, doc.y + LH / 2),
            20
        );
    }

    /// `consult-line` rows wear the buffer's own colours, taken from the spans
    /// it already has and shifted right by the number in front of them. Nothing
    /// is re-parsed, so this is also the check that the shift is right: an
    /// off-by-`prefix` here would colour the wrong token in the popup.
    #[test]
    fn a_consult_line_row_borrows_the_buffer_s_own_highlight() {
        let mut ed = Editor::new();
        //                             0123456789
        ed.buffer = Buffer::from_str("one\nlet x = 1\nthree");
        // "let" on line 1: chars 4..7 of the buffer.
        ed.buffer.highlights = vec![span(4, 7, HlKind::Keyword), span(12, 13, HlKind::Number)];
        ed.open_prompt(zemacs_core::PromptKind::Line);
        let p = ed.prompt.as_ref().unwrap();

        // Three lines, so "3" plus two spaces.
        assert_eq!(p.prefix, 3);
        let row = p.items[1].clone();
        assert_eq!(row, "2  let x = 1");

        let runs = candidate_runs(&ed, p, 1, &row);
        assert_eq!(
            runs,
            vec![
                (0, 3, HlKind::Comment),  // the number, dimmed like the gutter
                (3, 6, HlKind::Keyword),  // "let", shifted by the prefix
                (11, 12, HlKind::Number), // "1"
            ]
        );
        // ...and the run really does cover "let" in the row as drawn.
        assert_eq!(&row[3..6], "let");
        assert_eq!(&row[11..12], "1");

        // A row whose line is gone (the buffer changed under the prompt) is
        // drawn flat rather than reaching past the end of the rope.
        assert!(candidate_runs(&ed, p, 99, &row).is_empty());
    }

    /// A grep hit is `path:line:text` and has no spans to borrow — the file is
    /// not open. Its structure gets the colour instead, split on the same two
    /// colons the app splits on to open it.
    #[test]
    fn a_grep_row_colours_its_path_and_line_number() {
        let mut ed = Editor::new();
        ed.open_prompt(zemacs_core::PromptKind::Grep);
        let p = ed.prompt.as_ref().unwrap();

        let row = "src/main.rs:42:    let x = 1;";
        assert_eq!(
            candidate_runs(&ed, p, 0, row),
            vec![
                (0, 11, HlKind::Function),   // src/main.rs
                (11, 12, HlKind::Punctuation),
                (12, 14, HlKind::Number),    // 42
                (14, 15, HlKind::Punctuation),
            ]
        );
        assert_eq!(&row[0..11], "src/main.rs");
        assert_eq!(&row[12..14], "42");

        // A row that is not a hit at all — ripgrep printed something else, or
        // the list is still empty — is left alone rather than mis-split.
        assert!(candidate_runs(&ed, p, 0, "no colons here").is_empty());
    }

    #[test]
    fn the_gutter_is_sized_from_the_whole_file() {
        let set = Settings::default();
        let short = Buffer::from_str("a\nb\n");
        let long = Buffer::from_str(&"x\n".repeat(1500));
        // Three digits minimum, so a short file's text does not sit flush left.
        assert_eq!(gutter_digits(&short), 3);
        assert_eq!(gutter_digits(&long), 4);
        assert_eq!(gutter_w(&short, &set, CW), 4 * CW);
        assert_eq!(gutter_w(&long, &set, CW), 5 * CW);
        // Off: the text starts at the document edge and gets those columns back.
        let off = Settings { line_numbers: false, ..set };
        assert_eq!(gutter_w(&long, &off, CW), 0);
    }

    /// The gutter is a fact about the *buffer*, not about the editor — which is
    /// the whole point, because it is drawn once per pane. Two buffers on screen
    /// at once must be able to disagree, and before this they could not: the
    /// setting was global, so opening prose took the numbers off the source file
    /// beside it and the winner was whichever mode was entered last.
    #[test]
    fn each_buffer_answers_the_gutter_question_for_itself() {
        let set = Settings::default(); // editor-wide: on
        let code = Buffer::from_str("fn main() {}\n");
        let mut prose = Buffer::from_str("Once upon a time\n");
        prose.line_numbers = Some(false);

        assert_eq!(gutter_w(&code, &set, CW), 4 * CW, "code keeps its gutter");
        assert_eq!(gutter_w(&prose, &set, CW), 0, "prose beside it does not");

        // ...and the buffer can dissent the other way too, so turning numbers
        // off globally still leaves a buffer able to ask for them.
        let off = Settings { line_numbers: false, ..set };
        let mut wants = Buffer::from_str("1\n2\n");
        wants.line_numbers = Some(true);
        assert_eq!(gutter_w(&code, &off, CW), 0);
        assert_eq!(gutter_w(&wants, &off, CW), 4 * CW);

        // A dashboard and a terminal never have one, whatever anybody set:
        // there are no buffer lines to number. Not a policy Lisp has to
        // remember — the renderer knows what those two are.
        for kind in [BufferKind::Dashboard, BufferKind::Terminal] {
            let mut b = Buffer::from_str("x\n");
            b.kind = kind;
            b.line_numbers = Some(true);
            assert_eq!(gutter_w(&b, &set, CW), 0, "{kind:?} shows no gutter");
        }
    }

    /// The gutter's *ink* and the gutter's *columns* are one decision.
    ///
    /// They were two, and on the configuration we ship they disagreed on every
    /// org buffer: `init.lisp` says `(set-line-numbers t)` and
    /// `(set-no-gutter-modes '("org-mode" …))`, so `Buffer::line_numbers` is
    /// `Some(false)` and the width came out zero — while the draw loop asked
    /// `settings.line_numbers`, said yes, and wrote `format!("{n:>3}")` at
    /// `doc.x`, which with no gutter reserved is the first three characters of
    /// the line. Prose with its first three letters overprinted by a number.
    ///
    /// Asserted as the equivalence rather than as a pixel, because the equivalence
    /// is the invariant: whatever decides one has to decide the other.
    #[test]
    fn the_gutter_is_drawn_exactly_when_columns_were_reserved_for_it() {
        let set = Settings::default(); // editor-wide: on, as `init.lisp` sets it
        let mut org = Buffer::from_str("* A heading\nand its prose\n");
        org.major_mode = "org-mode".into();
        org.line_numbers = Some(false); // what `set-no-gutter-modes` parks here

        // The draw loop's condition, and the width, from one answer.
        for buf in [&org, &Buffer::from_str("fn main() {}\n")] {
            assert_eq!(
                gutter_on(buf, &set),
                gutter_w(buf, &set, CW) > 0,
                "ink and columns disagree for {:?}",
                buf.major_mode
            );
        }
        assert!(!gutter_on(&org, &set), "no gutter, so no number over the text");

        // ...and the other three corners of the same matrix, including a buffer
        // dissenting *into* a gutter the editor has turned off.
        let off = Settings { line_numbers: false, ..set };
        let mut wants = Buffer::from_str("1\n2\n");
        wants.line_numbers = Some(true);
        for (buf, s) in [(&wants, &off), (&wants, &set), (&org, &off)] {
            assert_eq!(gutter_on(buf, s), gutter_w(buf, s, CW) > 0);
        }
        assert!(gutter_on(&wants, &off));

        // A dashboard and a terminal have no lines to number, so neither half
        // fires however loudly anyone asked.
        for kind in [BufferKind::Dashboard, BufferKind::Terminal] {
            let mut b = Buffer::from_str("x\n");
            b.kind = kind;
            b.line_numbers = Some(true);
            assert!(!gutter_on(&b, &set), "{kind:?}");
            assert_eq!(gutter_w(&b, &set, CW), 0, "{kind:?}");
        }
    }

    /// A truncated line spends one visual row, whatever an image on it claims —
    /// and the slice the draw loop takes out of its cells stays inside them.
    ///
    /// The failing shape: truncation on, a line a little wider than the pane, and
    /// an overlay image tall enough to want three display rows. `need` is a `max`
    /// over the two claims, so the loop was granted three rows and walked
    /// `wrap_rows` three times — on a line it had already decided to cut at row
    /// zero. Row 1 then sliced `cells[cols .. cols + (cols - 1)]`, because the
    /// truncation marker pins `shown` at `cols - 1` on *every* row, and a line of
    /// `cols + 1` cells has nothing like that much left. An index panic, taking
    /// the frame and the editor with it.
    ///
    /// Asserted through [`drawn_rows`] because that is the function the loop
    /// calls; the arithmetic below it is the loop's, transcribed.
    #[test]
    fn a_truncated_line_stays_one_row_even_under_a_tall_image() {
        let cols = 30usize;
        // One cell past the pane: the narrowest line that both trips the marker
        // and is too short for a second full row.
        let cells = expand_line(&"x".repeat(cols + 1), 4);
        let marker = truncation_marker(cells.len(), cols);
        assert_eq!(marker, Some(cols - 1), "the line is wide enough to be cut");

        // Truncation: one visual row of text, and an image claiming three.
        let (text_rows, tall, image) = (1usize, 1usize, 3usize);
        let need = (text_rows * tall).max(image);
        let rows_left = 8usize;
        let shown_rows = drawn_rows(need.min(rows_left), tall, text_rows);
        assert_eq!(shown_rows, 1, "a truncated line is one row however tall");

        // ...and every row it does walk indexes inside the line. This is the
        // slice that panicked.
        for (rs, re) in wrap_rows(cells.len(), cols).take(shown_rows) {
            let shown = marker.unwrap_or(re - rs);
            assert!(
                rs + shown <= cells.len(),
                "cells[{rs}..{}] is past {}",
                rs + shown,
                cells.len()
            );
        }

        // The counting twins already believed this, which is why the draw loop
        // disagreeing with them was the bug rather than the feature: one row for
        // the line, whatever is on it.
        let mut buf = Buffer::from_str(&"x".repeat(cols + 1));
        buf.line_numbers = Some(false);
        assert_eq!(line_rows(&buf, 0, cols as i32 * CW, CW, false, 4), 1);

        // Wrapping is untouched: there the text really does own those rows, and
        // `take` was never the thing bounding them.
        let wrapped = wrap_row_count(cells.len(), cols);
        assert_eq!(wrapped, 2);
        assert_eq!(drawn_rows(wrapped * tall, tall, wrapped), 2);
        // A 2× line still claims two display rows per visual row, and the row
        // budget still cuts it at the bottom of the pane rather than the clamp.
        assert_eq!(drawn_rows(4, 2, wrapped), 2);
        assert_eq!(drawn_rows(1, 2, wrapped), 1, "half a scaled row still draws");
    }

    #[test]
    fn neither_mode_puts_a_cell_outside_its_pane() {
        // A 30-column left pane and a 500-character line, in both modes: every
        // column that gets drawn ends inside the pane, cursor and marker
        // included. This is the bug the whole feature exists for.
        let pane = Area { x: 0, y: 0, w: 2 * PAD + 30 * CW, h: 600 };
        let doc = doc_rect(pane, mh(), 0);
        let right = pane.x + pane.w;
        let cells = expand_line(&"x".repeat(500), 4);

        for &(wrap, cols) in &[(false, 30usize), (true, 30)] {
            let need = if wrap { wrap_row_count(cells.len(), cols) } else { 1 };
            let marker = (!wrap).then(|| truncation_marker(cells.len(), cols)).flatten();
            for (rs, re) in wrap_rows(cells.len(), cols).take(need) {
                let shown = marker.unwrap_or(re - rs);
                assert!(shown <= cols, "wrap={wrap}");
                assert!(doc.x + shown as i32 * CW <= right, "wrap={wrap}");
                if let Some(mc) = marker {
                    assert!(doc.x + (mc as i32 + 1) * CW <= right, "marker spills");
                }
                // The selection of the entire line, clipped to this row.
                let (a, b) = row_span((0, cells.len() + 1), rs, cols).unwrap();
                assert!(doc.x + b as i32 * CW <= right, "selection spills");
                assert!(b > a);
            }
            // The cursor at end of line — the position that used to be drawn at
            // column 500 and left to the clip rect to hide.
            let (_, cc) = cursor_pos(cells.len(), cols, need);
            let x = doc.x + cc as i32 * CW;
            assert!(x <= right, "cursor at {x} outside {pane:?}");
            // `draw_cursor` trims it; a whole cell here would overhang.
            assert!(x + CW.min(right - x) <= right, "trimmed cursor spills");
        }
    }

    #[test]
    fn color_conversion_rounds_and_clamps() {
        assert_eq!(rgb([0.0, 0.0, 0.0]), Color::RGB(0, 0, 0));
        assert_eq!(rgb([1.0, 1.0, 1.0]), Color::RGB(255, 255, 255));
        assert_eq!(rgb([-1.0, 2.0, 0.5]), Color::RGB(0, 255, 128));
    }

    #[test]
    fn mix_interpolates() {
        assert_eq!(mix([0.0; 3], [1.0; 3], 0.0), [0.0; 3]);
        assert_eq!(mix([0.0; 3], [1.0; 3], 1.0), [1.0; 3]);
        assert_eq!(mix([0.0; 3], [1.0; 3], 0.5), [0.5; 3]);
    }

    #[test]
    fn point_size_tracks_dpi() {
        assert_eq!(scale_point_size(18.0, 2.0), 36);
        assert_eq!(scale_point_size(18.0, 1.0), 18);
        assert_eq!(scale_point_size(0.0, 1.0), 4); // clamped, never zero
    }

    // A plausible 1100x760 window at 2x DPI, in the units the popup math uses.
    const LH: i32 = 22;
    const CW: i32 = 11;
    const STATUS: i32 = LH + PAD;

    #[test]
    fn bottom_panel_grows_up_from_the_status_strip() {
        let b = bottom_popup(1100, 760, STATUS, LH, 8);
        assert_eq!(b.rows, 8); // plenty of room, nothing clamped
        assert_eq!(b.x, 0);
        assert_eq!(b.w, 1100); // full bleed, like consult
        assert_eq!(b.h, bottom_chrome_h(LH) + 8 * LH);
        // Bottom edge is exactly the top of the status strip.
        assert_eq!(b.y + b.h, 760 - STATUS);
        // ...and it leaves document visible above.
        assert!(b.y >= LH);
    }

    #[test]
    fn bottom_panel_clamps_instead_of_underflowing_in_a_short_window() {
        // Room for the chrome and about two rows, not the ten asked for.
        let b = bottom_popup(600, STATUS + bottom_chrome_h(LH) + 3 * LH, STATUS, LH, 10);
        assert_eq!(b.rows, 2); // one line held back for the document
        assert_eq!(b.y + b.h, bottom_chrome_h(LH) + 3 * LH);

        // Absurdly short: no rows, still a real rectangle at a sane origin.
        for h in [0, 1, 20, STATUS, STATUS + 5] {
            let b = bottom_popup(600, h, STATUS, LH, 10);
            assert_eq!(b.rows, 0, "h={h}");
            assert!(b.y >= 0 && b.h > 0, "h={h} -> {b:?}");
            assert!(b.h <= (h - STATUS).max(LH), "h={h} -> {b:?}");
        }
    }

    #[test]
    fn bottom_panel_never_asks_for_more_rows_than_requested() {
        // A huge window still honours the caller's cap.
        assert_eq!(bottom_popup(1100, 4000, STATUS, LH, 3).rows, 3);
        assert_eq!(bottom_popup(1100, 4000, STATUS, LH, 0).rows, 0);
    }

    #[test]
    fn center_box_floats_above_the_middle() {
        let b = center_popup(1100, 760, STATUS, LH, CW, 8);
        assert_eq!(b.rows, 8);
        assert_eq!(b.w, 1100 * CENTER_PCT / 100);
        assert_eq!(b.x, (1100 - b.w) / 2); // horizontally centered
        assert_eq!(b.h, center_chrome_h(LH) + 8 * LH);
        // Its centre is above the centre of the document area, and it is fully
        // inside it.
        let avail = 760 - STATUS;
        assert!(b.y + b.h / 2 < avail / 2);
        assert!(b.y > 0 && b.y + b.h <= avail);
    }

    #[test]
    fn center_box_width_is_clamped_at_both_ends() {
        // Narrow window: 66% would be under the minimum, but it must not spill
        // outside the window either.
        let b = center_popup(300, 760, STATUS, LH, CW, 8);
        assert!(b.w <= 300 - 2 * PAD, "{b:?}");
        assert!(b.x >= 0 && b.x + b.w <= 300, "{b:?}");
        // Ultra-wide window: capped in columns, not left to sprawl.
        let b = center_popup(6000, 760, STATUS, LH, CW, 8);
        assert_eq!(b.w, CENTER_MAX_COLS * CW);
        // Degenerate width: still a positive rectangle on screen.
        let b = center_popup(4, 760, STATUS, LH, CW, 8);
        assert!(b.w > 0 && b.x >= 0, "{b:?}");
    }

    #[test]
    fn center_box_clamps_rows_in_a_short_window() {
        let h = STATUS + center_chrome_h(LH) + 4 * LH;
        let b = center_popup(1100, h, STATUS, LH, CW, 10);
        assert_eq!(b.rows, 2); // two document lines held back
        assert_eq!(b.h, center_chrome_h(LH) + 2 * LH);
        assert!(b.y >= 0 && b.y + b.h <= h - STATUS);

        for h in [0, 1, 30, STATUS, STATUS + LH] {
            let b = center_popup(1100, h, STATUS, LH, CW, 10);
            assert_eq!(b.rows, 0, "h={h}");
            assert!(b.y >= 0 && b.h > 0, "h={h} -> {b:?}");
        }
    }

    // --- modeline ---------------------------------------------------------

    /// Enough to compare two shades of the same hue; the bevel only ever pushes
    /// a colour toward white or black, so a channel sum orders them correctly.
    fn luma(c: [f32; 3]) -> f32 {
        c[0] + c[1] + c[2]
    }

    /// A modeline-shaped strip at the bottom of a 1100x760 window.
    const STRIP: Area = Area {
        x: 0,
        y: 726,
        w: 1100,
        h: 34,
    };

    #[test]
    fn positive_relief_lights_the_top_and_left() {
        let edges = bevel(STRIP, 2);
        assert_eq!(
            edges,
            vec![
                (Area { x: 0, y: 726, w: 1100, h: 2 }, true),   // top: lit
                (Area { x: 0, y: 758, w: 1100, h: 2 }, false),  // bottom: shadow
                (Area { x: 0, y: 726, w: 2, h: 34 }, true),     // left: lit
                (Area { x: 1098, y: 726, w: 2, h: 34 }, false), // right: shadow
            ]
        );
        // Every edge is inside the strip it decorates.
        for (e, _) in bevel(STRIP, 2) {
            assert!(e.x >= STRIP.x && e.x + e.w <= STRIP.x + STRIP.w, "{e:?}");
            assert!(e.y >= STRIP.y && e.y + e.h <= STRIP.y + STRIP.h, "{e:?}");
        }
    }

    #[test]
    fn negative_relief_swaps_highlight_and_shadow() {
        let raised = bevel(STRIP, 2);
        let sunken = bevel(STRIP, -2);
        // Same four rectangles...
        let rects = |v: &[(Area, bool)]| v.iter().map(|&(a, _)| a).collect::<Vec<_>>();
        assert_eq!(rects(&raised), rects(&sunken));
        // ...with the light source flipped, which is the entire difference
        // between a raised modeline and a pressed one.
        for (&(_, a), &(_, b)) in raised.iter().zip(&sunken) {
            assert_ne!(a, b);
        }
        assert!(!sunken[0].1 && sunken[1].1); // top shadowed, bottom lit
    }

    #[test]
    fn relief_zero_or_no_room_draws_no_bevel() {
        assert!(bevel(STRIP, 0).is_empty());
        // Degenerate strips: no bevel rather than facing edges overlapping into
        // one solid shadow-coloured bar, and never a negative rectangle.
        for h in [0, 1, 2, 3, 4] {
            for w in [0, 1, 2, 1100] {
                let r = Area { x: 0, y: 0, w, h };
                for relief in [-2, 0, 2] {
                    for (e, _) in bevel(r, relief) {
                        assert!(e.w > 0 && e.h > 0, "w={w} h={h} relief={relief} -> {e:?}");
                        assert!(e.w <= r.w && e.h <= r.h, "w={w} h={h} -> {e:?}");
                    }
                }
            }
        }
        // A 4px strip can hold a 2px box exactly, a 3px one only half of it.
        assert_eq!(bevel_width(Area { x: 0, y: 0, w: 100, h: 4 }, 2), 2);
        assert_eq!(bevel_width(Area { x: 0, y: 0, w: 100, h: 3 }, 2), 1);
    }

    #[test]
    fn bevel_shades_straddle_the_strip_background() {
        // Both directions matter: on a light theme a "lighten" that clipped to
        // white would make the two edges identical and kill the 3D read.
        for background in [[0.06, 0.06, 0.09], [0.98, 0.97, 0.94]] {
            let set = Settings {
                background,
                foreground: [1.0 - background[0]; 3],
                ..Settings::default()
            };
            // The colours are theme-aware now, so they need a whole Editor.
            let mut ed = Editor::new();
            ed.settings = set;

            let base = modeline_bg(&ed, true);
            assert!(luma(highlight_shade(base)) > luma(base), "{background:?}");
            assert!(luma(shadow_shade(base)) < luma(base), "{background:?}");
            // Distinct from the buffer behind it, or the strip has no edge at all.
            assert_ne!(luma(base), luma(background), "{background:?}");
            // The active strip is the brighter and higher-contrast of the two.
            let idle = modeline_bg(&ed, false);
            assert!((luma(base) - luma(background)).abs() > (luma(idle) - luma(background)).abs());
            assert_ne!(luma(modeline_fg(&ed, true)), luma(modeline_fg(&ed, false)));

            // A Lisp `(set-syntax-color "modeline" ...)` overrides the derived
            // shade; leaving it unset is what keeps the strip tracking the theme.
            ed.apply(zemacs_core::EditorCommand::SetSyntaxColor(
                "modeline".into(),
                [0.5, 0.0, 0.0],
            ));
            assert_eq!(modeline_bg(&ed, true), [0.5, 0.0, 0.0]);
            assert_eq!(modeline_bg(&ed, false), idle, "inactive is its own face");
        }
    }

    #[test]
    fn modeline_hugs_the_bottom_of_its_window() {
        let set = Settings::default();
        let mh = modeline_h(LH, &set);
        assert!(mh > LH, "the box and padding must add height, not eat the text row");

        let frame = Area { x: 0, y: 0, w: 1100, h: 760 };
        let r = modeline_rect(frame, mh);
        assert_eq!(r, Area { x: 0, y: 760 - mh, w: 1100, h: mh });
        assert_eq!(r.y + r.h, frame.h); // flush with the bottom edge
        // Tall enough for the full relief, by construction.
        assert_eq!(bevel_width(r, relief(&set)), relief(&set).abs());

        // A window that is not the frame — the multi-window case.
        let pane = Area { x: 550, y: 100, w: 550, h: 300 };
        let r = modeline_rect(pane, mh);
        assert_eq!(r, Area { x: 550, y: 400 - mh, w: 550, h: mh });
    }

    #[test]
    fn a_degenerate_window_yields_no_negative_modeline() {
        let mh = modeline_h(LH, &Settings::default());
        for h in [-10, 0, 1, 5, mh - 1, mh, mh + 1] {
            for w in [-4, 0, 1, 1100] {
                let area = Area { x: 3, y: 7, w, h };
                let r = modeline_rect(area, mh);
                assert!(r.w >= 0 && r.h >= 0, "w={w} h={h} -> {r:?}");
                assert!(r.h <= h.max(0), "w={w} h={h} -> {r:?}");
                assert!(r.y >= area.y, "w={w} h={h} -> {r:?}"); // never above its window
                assert_eq!(r.y + r.h, area.y + h.max(0), "w={w} h={h} -> {r:?}");
            }
        }
    }

    #[test]
    fn truncation_counts_chars_not_bytes() {
        // 5 chars, 10 bytes. `&s[..3]` would panic on a codepoint boundary and
        // `s.len() <= cols` would truncate a string that fits.
        assert_eq!(truncate("ααααα", 3), "αα…");
        assert_eq!(truncate("ααααα", 5), "ααααα");
        assert_eq!(truncate("ααααα", 9), "ααααα");
        // Paths and command names, the real payload.
        assert_eq!(truncate("~/Code/zemacs/README.org", 10), "~/Code/ze…");
        assert_eq!(truncate("find-file", 9), "find-file");
        // Degenerate widths must not underflow `n - 1`.
        assert_eq!(truncate("abc", 1), "…");
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn box_titles_drop_the_prompt_punctuation() {
        assert_eq!(title_of("M-x "), "M-x");
        assert_eq!(title_of("Find file: "), "Find file");
        assert_eq!(title_of("Buffer: "), "Buffer");
        assert_eq!(title_of(""), "");
    }

    // --- panes ------------------------------------------------------------

    /// The strip a pane reserves with the stock settings: 34px.
    fn mh() -> i32 {
        modeline_h(LH, &Settings::default())
    }

    #[test]
    fn mouse_coordinates_are_scaled_into_drawable_pixels() {
        assert_eq!(scale_point(1.0, 0, 0), (0, 0));
        assert_eq!(scale_point(1.0, 137, 42), (137, 42));
        // Retina: SDL reports the click in window points while every pane
        // rectangle is in drawable pixels. Identity here puts a divider grab in
        // the wrong half of the window.
        assert_eq!(scale_point(2.0, 137, 42), (274, 84));
        assert_eq!(scale_point(2.0, 1, 1), (2, 2));
        // Fractional scales round rather than truncate, so the pixel picked is
        // the one under the pointer.
        assert_eq!(scale_point(1.5, 3, 5), (5, 8));
    }

    #[test]
    fn a_panes_document_stops_above_its_modeline() {
        let pane = Area { x: 550, y: 100, w: 450, h: 300 };
        let doc = doc_rect(pane, mh(), 0);
        let strip = modeline_rect(pane, mh());
        assert!(doc.x >= pane.x && doc.y >= pane.y, "{doc:?} outside {pane:?}");
        assert!(doc.x + doc.w <= pane.x + pane.w, "{doc:?} outside {pane:?}");
        assert!(doc.y + doc.h <= strip.y, "{doc:?} runs into {strip:?}");
        // Every row it advertises fits in what is left.
        let rows = doc_lines(pane, mh(), LH);
        assert_eq!(rows, (doc.h / LH) as usize);
        assert!(doc.y + rows as i32 * LH <= strip.y);
    }

    #[test]
    fn a_pane_too_short_for_a_line_gets_no_lines() {
        for h in [-5, 0, 1, mh(), mh() + PAD, mh() + PAD + LH - 1] {
            let pane = Area { x: 0, y: 0, w: 400, h };
            assert_eq!(doc_lines(pane, mh(), LH), 0, "h={h}");
            let doc = doc_rect(pane, mh(), 0);
            assert!(doc.w >= 0 && doc.h >= 0, "h={h} -> {doc:?}");
        }
        // One pixel more than the chrome plus a line, and one row appears.
        let pane = Area { x: 0, y: 0, w: 400, h: mh() + PAD + LH };
        assert_eq!(doc_lines(pane, mh(), LH), 1);
        assert_eq!(doc_lines(pane, mh(), 0), doc_rect(pane, mh(), 0).h as usize); // no /0
    }

    #[test]
    fn every_pane_pays_for_its_own_modeline() {
        let area = zemacs_core::Rect::new(0, 0, 1000, 600);
        let lines = |p: &zemacs_core::frame::Pane| doc_lines(area_of(p.rect), mh(), LH);

        let f = zemacs_core::Frame::new(0);
        let whole = lines(&f.panes(area)[0]);
        assert!(whole > 0);

        let mut f = zemacs_core::Frame::new(0);
        f.split(zemacs_core::frame::Split::Rows);
        let panes = f.panes(area);
        let (top, bottom) = (lines(&panes[0]), lines(&panes[1]));
        assert!(top > 0 && bottom > 0);
        // Strictly fewer than one window over the same height: two modelines
        // and two divider halves came out of the middle.
        assert!(top + bottom < whole, "{top}+{bottom} vs {whole}");

        // Side by side changes nothing vertically.
        let mut f = zemacs_core::Frame::new(0);
        f.split(zemacs_core::frame::Split::Columns);
        for p in &f.panes(area) {
            assert_eq!(lines(p), whole);
        }
    }

    #[test]
    fn a_long_line_is_cut_at_the_panes_last_whole_column() {
        assert_eq!(visible_cols(30 * CW, CW), 30);
        assert_eq!(visible_cols(30 * CW + CW - 1, CW), 30); // no partial column
        assert_eq!(visible_cols(0, CW), 0);
        assert_eq!(visible_cols(-40, CW), 0); // pane narrower than its gutter
        assert_eq!(visible_cols(100, 0), 100); // never divides by zero

        // The real case: 500 characters in a 30-column left pane. The last cell
        // drawn ends inside the pane, so the right pane keeps its pixels.
        let pane = Area { x: 0, y: 0, w: 2 * PAD + 30 * CW, h: 600 };
        let doc = doc_rect(pane, mh(), 0);
        let cols = visible_cols(doc.w, CW);
        assert_eq!(cols, 30);
        let cells = expand_line(&"x".repeat(500), 4);
        assert!(cells.len() > cols);
        let last = doc.x + (cols as i32 - 1) * CW;
        assert!(last + CW <= pane.x + pane.w, "{last} spills out of {pane:?}");
    }

    #[test]
    fn an_inactive_pane_names_its_own_buffer() {
        let ed = Editor::new();
        let mut other = Buffer::from_str("");
        other.id = 9;
        other.modified = true;
        other.path = Some(PathBuf::from("/tmp/notes.org"));

        let text = |buf: &Buffer, active: bool| {
            let (left, right) = modeline::segments(&ed, buf, active);
            left.iter()
                .chain(right.iter())
                .map(|s| s.text.clone())
                .collect::<String>()
        };

        let idle = text(&other, false);
        assert!(idle.contains("notes.org") && idle.contains('●'), "{idle}");
        // Nothing about the focused window: the mode, the position and the
        // messages all describe a window this pane is not.
        assert!(!idle.contains(&ed.buffer.name()), "{idle}");
        assert!(!idle.contains("DASHBOARD"), "{idle}");
        // ...and the active pane is the one that names the mode.
        assert!(text(&ed.buffer, true).contains("DASHBOARD"));
    }

    #[test]
    fn the_divider_is_its_own_shade() {
        for background in [[0.06, 0.06, 0.09], [0.98, 0.97, 0.94]] {
            let mut ed = Editor::new();
            ed.settings = Settings {
                background,
                foreground: [1.0 - background[0]; 3],
                ..Settings::default()
            };
            let d = divider_shade(&ed.settings);
            // Distinct from the buffer it sits between and from both modeline
            // faces, or a split has no visible seam at all.
            assert_ne!(luma(d), luma(background), "{background:?}");
            assert_ne!(luma(d), luma(modeline_bg(&ed, true)), "{background:?}");
            assert_ne!(luma(d), luma(modeline_bg(&ed, false)), "{background:?}");
            // Further from the buffer than either strip, on a light theme too.
            let from_bg = |c: [f32; 3]| (luma(c) - luma(background)).abs();
            assert!(from_bg(d) > from_bg(modeline_bg(&ed, true)), "{background:?}");
        }
    }

    #[test]
    fn a_monospace_font_is_findable() {
        match find_font() {
            Ok(p) => assert!(p.is_file(), "{} does not exist", p.display()),
            // No font on this box is survivable, an unhelpful message is not.
            Err(e) => {
                let msg = e.to_string();
                assert!(msg.contains("ZEMACS_FONT") && msg.contains(FONT_CANDIDATES[0]));
            }
        }
    }

    /// A scaled face really opens, and really is bigger.
    ///
    /// Everything else about typesetting in this file is arithmetic and is
    /// tested as arithmetic. This is the one step that talks to FreeType, and it
    /// is the step whose failure mode is silent: `open_face` answering `None`,
    /// or the point size not moving, would draw a heading at body size and no
    /// amount of layout testing would notice.
    ///
    /// No window — `Sdl2TtfContext` needs no video subsystem — so this runs on a
    /// headless box like every other test here. The `Box::leak` is the same
    /// trade `Renderer::new` makes and is documented there.
    #[test]
    fn a_scaled_face_opens_and_is_bigger_than_the_body() {
        let Ok(path) = find_font() else {
            return; // no font on this box; the test above already said so
        };
        let ttf: &'static Sdl2TtfContext = Box::leak(Box::new(sdl2::ttf::init().unwrap()));
        let advance = |pct: u16, style: u8| {
            let key = FaceKey {
                point_size: scaled(18, pct).clamp(4, 400) as u16,
                style,
            };
            let face = open_face(ttf, &path, key).expect("the body font opens at every step");
            metrics(&face.font)
        };
        let (body_w, body_h) = advance(100, 0);
        for &pct in &SCALE_STEPS[1..] {
            let (w, h) = advance(pct, 0);
            // Bigger in both directions, and roughly by the factor asked for.
            // Roughly, because a point size is rounded to a whole number and a
            // rasteriser rounds again — which is exactly why the *grid* is
            // arithmetic and the face is only a rasteriser. See `draw_run`.
            assert!(w > body_w, "{pct}% is no wider than the body: {w} vs {body_w}");
            assert!(h > body_h, "{pct}% is no taller than the body: {h} vs {body_h}");
            let want = scaled(body_w, pct);
            assert!(
                (w - want).abs() <= 2,
                "{pct}% advance {w} is not within a pixel or two of {want}"
            );
        }
        // Bold and italic open at every size too — the `expect` inside is the
        // assertion, since a style FreeType refused would draw nothing at all.
        //
        // Their advance is deliberately *not* asserted equal to the plain one.
        // It is not: SDL_ttf's synthetic bold smears the outline and gains a
        // pixel on this font, and the italic shears it. That it does not matter
        // is the design — `draw_run` and `draw_weighted` step by the grid's cell
        // and never by the face's advance, so a bold run occupies the columns a
        // plain one would and overhangs its last one by a pixel, exactly as the
        // modeline's bold segments already do.
        for &pct in &SCALE_STEPS {
            for style in 1..4u8 {
                assert!(advance(pct, style).0 > 0, "{pct}%/{style} has no advance");
            }
        }
    }
}

#[cfg(test)]
mod truncate_invariant {
    use super::*;

    /// `draw_segments` subtracts `str_cells(truncate(s, left))` from `left`, so
    /// truncate must never answer something wider than it was asked for. A
    /// `usize` underflow here is a panic in a release-mode editor, not a wrong
    /// pixel — and the modeline is where it bites, because that is the one strip
    /// whose text is arbitrary and whose width runs out.
    #[test]
    fn truncate_never_exceeds_its_budget() {
        let cases = [
            "", "a", "ab", "abc", "…", "……", "漢", "漢字", "漢字漢字",
            "e\u{301}", "e\u{301}e\u{301}e\u{301}", "a\u{301}漢b",
            "very long modeline segment that will certainly not fit",
            "linear-algebra.org", "*tutor-lisp*", "\t\ta", "\u{0}\u{1}a",
            "🙂", "🙂🙂🙂", "a🙂漢e\u{301}…",
        ];
        for s in cases {
            for n in 0..12usize {
                let out = truncate(s, n);
                assert!(
                    str_cells(&out) <= n,
                    "truncate({s:?}, {n}) = {out:?} is {} cells, over budget",
                    str_cells(&out)
                );
            }
        }
    }
}

/// What can be tested about scenes with no window open.
///
/// Painting cannot: every primitive under `draw_scene` ends at a `WindowCanvas`,
/// and there is no canvas without a window. Measuring almost entirely can, which
/// is the half that matters — it is the arithmetic a document's whole layout
/// hangs off, and its failures are silent rather than loud.
#[cfg(test)]
mod scenes {
    use super::*;
    use zemacs_gui::{Align, Block, Length, Rect as GuiRect};

    fn style(size: u16, bold: bool, italic: bool) -> Style {
        Style {
            size,
            bold,
            italic,
            face: None,
        }
    }

    /// A percentage of the body becomes a point size, and only ever one of the
    /// handful the face cache is bounded by.
    #[test]
    fn a_style_percentage_becomes_a_point_size_off_the_body() {
        let key = |size| face_key(18, scene_cut(style(size, false, false))).point_size;
        assert_eq!(key(100), 18);
        assert_eq!(key(200), 36);
        assert_eq!(key(150), 27);
        // 18 × 125% is 22.5, and a point size is a whole number.
        assert_eq!(key(125), 22);
        // Snapped, not honoured: 140 is nearer 150 than 125, and a face of its
        // own is exactly what `SCALE_STEPS` exists to refuse. 137 lands the
        // other way, which is the point of asserting both — the steps are not
        // evenly spaced and "round it" is not the rule.
        assert_eq!(key(140), key(150));
        assert_eq!(key(137), key(125));
        // A size nobody set. `Style::default` says 100, but the field is a plain
        // `u16` and a builder in Lisp can leave it at nothing.
        assert_eq!(key(0), key(100));
        // Absurd sizes clamp to what SDL_ttf will open rather than wrapping the
        // `u16` on the way through.
        assert_eq!(key(u16::MAX), key(200));
    }

    #[test]
    fn weight_and_slant_are_the_two_bits_of_the_face_key() {
        let bits = |b, i| face_key(18, scene_cut(style(100, b, i))).style;
        assert_eq!(bits(false, false), 0);
        assert_eq!(bits(true, false), 1);
        assert_eq!(bits(false, true), 2);
        assert_eq!(bits(true, true), 3);
    }

    /// The bound on [`Renderer::faces`] is the one thing a scene could quietly
    /// break, since a scene's sizes are written by hand in Lisp rather than
    /// picked from a list. It cannot: every one of them goes through
    /// [`scene_cut`] first.
    #[test]
    fn no_scene_style_can_name_a_face_outside_the_cache_bound() {
        let mut keys = std::collections::HashSet::new();
        for size in [0u16, 1, 99, 100, 101, 112, 137, 175, 200, 1000, u16::MAX] {
            for bold in [false, true] {
                for italic in [false, true] {
                    let cut = scene_cut(style(size, bold, italic));
                    assert!(SCALE_STEPS.contains(&cut.pct), "{size}% escaped as {}", cut.pct);
                    keys.insert(face_key(18, cut));
                }
            }
        }
        assert!(keys.len() <= MAX_FACES, "{} faces, bound is {MAX_FACES}", keys.len());
    }

    /// A face the cache would not open degrades to the body metric.
    ///
    /// `None` is exactly what [`Renderer::face_font`] answers for a face
    /// [`open_face`] refused — a font file that will not load at that point
    /// size — and the failure this guards is the quiet one: a zero advance wraps
    /// every word of a paragraph onto one line and stacks a whole document at
    /// one point, which reads as a layout bug and not as a missing font.
    #[test]
    fn a_size_the_face_cache_refuses_measures_on_the_body_metric_instead_of_zero() {
        // Four characters at a 10-pixel cell, doubled.
        assert_eq!(advance_in(None, 10, "abcd", 200), 80);
        assert_eq!(advance_in(None, 10, "abcd", 100), 40);
        // A wide character is two cells here exactly as it is on the grid.
        assert_eq!(advance_in(None, 10, "漢", 100), 20);
        // Nothing to set is nothing wide, which is not the same failure.
        assert_eq!(advance_in(None, 10, "", 100), 0);
        // Never zero for text that exists, even where the arithmetic would
        // round to it — `scaled` has a one-pixel floor for this reason.
        assert!(advance_in(None, 0, "a", 100) > 0);
        assert_eq!(line_in(None, 20, 16, 150), (30, 24));
        assert_eq!(line_in(None, 20, 16, 100), (20, 16));
    }

    /// The real path, with a real font and no window — `Sdl2TtfContext` needs no
    /// video subsystem, which is what lets this run on a headless box. The
    /// `Box::leak` is the trade `Renderer::new` makes and documents.
    #[test]
    fn a_real_face_measures_a_string_wider_than_nothing_and_larger_sizes_wider_still() {
        let Ok(path) = find_font() else {
            return; // no font on this box; `a_monospace_font_is_findable` says so
        };
        let ttf: &'static Sdl2TtfContext = Box::leak(Box::new(sdl2::ttf::init().unwrap()));
        let open = |pct: u16| {
            open_face(ttf, &path, face_key(18, scene_cut(style(pct, false, false))))
                .expect("the body font opens at every step")
        };
        let (body, big) = (open(100), open(200));
        let (bw, bh) = (
            advance_in(Some(&body.font), 0, "measure", 100),
            line_in(Some(&body.font), 0, 0, 100),
        );
        assert!(bw > 0, "a real face measured 'measure' as nothing");
        assert!(bh.0 > 0 && bh.1 > 0, "a real face has no line box: {bh:?}");
        // The fallback's `cell_w` and `line_h` are passed as zero above on
        // purpose: if the real face were being ignored those calls would answer
        // the one-pixel floor, and the assertions above would have caught it.
        let (gw, gh) = (
            advance_in(Some(&big.font), 0, "measure", 200),
            line_in(Some(&big.font), 0, 0, 200),
        );
        assert!(gw > bw, "200% measured {gw}, no wider than the body's {bw}");
        assert!(gh.0 > bh.0, "200% is no taller than the body: {gh:?} vs {bh:?}");
        // An empty string is nothing wide in a real face too, which is what
        // lets a paragraph hold an empty run without reserving a pixel for it.
        assert_eq!(advance_in(Some(&body.font), 8, "", 100), 0);
    }

    /// Eight pixels a character, sixteen a line — the fake metric every wrapping
    /// test in `crates/gui` is written against.
    struct Eight;

    impl Measure for Eight {
        fn advance(&self, text: &str, _style: Style) -> i32 {
            text.chars().count() as i32 * 8
        }
        fn line(&self, _style: Style) -> (i32, i32) {
            (16, 12)
        }
    }

    /// `Piece::start` and `Piece::end` are byte offsets into a run's string, and
    /// `draw_text_frame` slices with them.
    ///
    /// The doc comment on `Piece` promises they are always on character
    /// boundaries, and the painter's `text.get(..)` is written not to trust it —
    /// but a promise nothing checks is a promise that quietly stops being true.
    /// This is the check, at the widths where it would break: one pixel, where
    /// every word overflows and `wrap` falls into its per-character path, and a
    /// few just wide enough to break in the middle of a multi-byte word.
    #[test]
    fn every_piece_of_a_wrapped_paragraph_starts_and_ends_on_a_character_boundary() {
        let runs = vec![
            Run::Text {
                text: "漢字 café — naïve 🙂🙂 x".into(),
                style: style(100, false, false),
                tag: None,
            },
            Run::Text {
                text: "e\u{301}e\u{301}e\u{301}".into(),
                style: style(200, true, false),
                tag: None,
            },
        ];
        let mut scene = Scene::default();
        let text = scene.push(Node::Text {
            runs: runs.clone(),
            align: Align::Start,
        });
        let root = scene.push(Node::Block(Block {
            children: vec![text],
            width: Length::Fill,
            ..Block::default()
        }));
        scene.set_root(root);

        for w in [1, 7, 8, 9, 16, 41, 400] {
            let laid = zemacs_gui::layout(
                &scene,
                GuiRect {
                    x: 0,
                    y: 0,
                    w,
                    h: 4000,
                },
                &Eight,
            );
            for f in &laid.frames {
                for line in &f.lines {
                    for p in &line.pieces {
                        let Some(Run::Text { text, .. }) = runs.get(p.run) else {
                            continue;
                        };
                        assert!(
                            text.get(p.start..p.end).is_some(),
                            "at width {w}, piece {}..{} does not slice {text:?}",
                            p.start,
                            p.end
                        );
                    }
                }
            }
        }
    }
}
