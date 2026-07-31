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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use sdl2::pixels::{Color, PixelFormatEnum};
use sdl2::rect::Rect;
use sdl2::render::{BlendMode, Texture, TextureCreator, WindowCanvas};
use sdl2::surface::Surface;
use sdl2::ttf::{Font, Hinting, Sdl2TtfContext};
use sdl2::video::WindowContext;
use zemacs_core::modeline;
use zemacs_core::{
    Buffer, BufferKind, CompletionStyle, Editor, HlKind, ImageId, LineOverflow, Mode, Overlay,
    Settings, Span, Window,
};
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
            images: HashMap::new(),
            font_path,
            point_size,
            glyphs: HashMap::new(),
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

    /// Draw frame `frame_index` of the editor. Writes `viewport_lines` back —
    /// per window, and on the editor from the focused pane — because only the
    /// renderer knows how many lines actually fit and the core scroll logic
    /// needs it.
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
            let lines = editor.frames[frame_index]
                .window(p.window)
                .map(|w| (w.buffer, w.scroll))
                .and_then(|(id, scroll)| {
                    let buf = editor.buffer_by_id(id)?;
                    let set = &editor.settings;
                    let doc = doc_rect(pane, status_h);
                    let cols =
                        visible_cols(doc.w - gutter_w(buf, set, self.cell_w), self.cell_w);
                    Some(visible_lines(buf, scroll, rows, cols, set))
                })
                .unwrap_or(rows);
            if let Some(w) = editor.frames[frame_index].window_mut(p.window) {
                w.viewport_lines = lines;
            }
            // Core clamps scrolling against this one, and it belongs to
            // whichever window is being typed into.
            if focused && p.window == current {
                editor.viewport_lines = lines;
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
            match (buf.kind, terminal) {
                (BufferKind::Dashboard, _) => {
                    self.draw_dashboard(editor, doc_rect(pane, status_h))
                }
                // A terminal is drawn from its live grid rather than from the
                // rope, because per-cell colour and the block cursor are both
                // gone by the time the grid has been flattened into text. The
                // rope is still what the buffer switcher reads.
                (BufferKind::Terminal, Some(screen)) => {
                    let (bg, fg) = (editor.settings.background, editor.settings.foreground);
                    self.draw_terminal(screen, doc_rect(pane, status_h), rgb(bg), rgb(fg));
                }
                _ => self.draw_document(editor, buf, win, pane, status_h, active),
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
            self.draw_completion(editor, area.w, area.h, status_h);
        }

        Ok(())
    }

    /// Put the drawn frame on screen. Blocks until the next vertical blank, so
    /// this is what paces the main loop — and why the caller drops the editor
    /// lock before getting here.
    pub fn present(&mut self) {
        self.canvas.present();
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
        let doc = doc_rect(pane, status_h);
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
        let gutter = gutter_w(buf, set, self.cell_w);
        let x0 = doc.x + gutter;
        let cols = visible_cols(doc.w - gutter, self.cell_w);
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

        // `row` is a display row in the pane, `line` a buffer line: with
        // wrapping the two advance at different rates.
        let (mut row, mut line) = (0usize, win.scroll);
        while row < rows && line < buf.len_lines() {
            let start = buf.line_start(line);
            let len = buf.line_len(line);
            let end = start + len;

            let (next, runs) = spans_for_line(spans, si, start, end);
            si = next;

            let ov_runs = overlays_for_line(overlays, start, end);
            let mut cells = expand_line(&buf.slice_string(start, end), set.tab_width);
            // `display` and an image both *replace* the cells they cover, which
            // is what makes wrapping, the cursor and the selection follow the
            // substitution instead of having to be told about it separately.
            //
            // An overlay reaching onto later lines substitutes on the first one
            // and blanks the rest: one image, then the empty rows its own source
            // lines have become. ponytail — a proper multi-line display would
            // have to change how many rows a line occupies, and rows are a fixed
            // height here.
            let mut images: Vec<(usize, zemacs_core::ImageId)> = Vec::new();
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
                            " ".repeat(image_cells(img.width, self.cell_w))
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

            // Rows this line wants, and how many of them are left in the pane.
            // ponytail: a wrapped line taller than the remaining rows is cut at
            // the bottom edge, because `scroll` counts buffer lines and there is
            // no row to scroll to. Ceiling: a paragraph-length line at the foot
            // of a short pane. Upgrade path: a scroll position of (line, row).
            let need = if wrap { wrap_row_count(cells.len(), cols) } else { 1 };
            let fits = need.min(rows - row);

            // Only the focused window of the focused frame gets a cursor: two
            // blocks on screen at once is two claims about where typing goes.
            let cursor_at = (active && line == cur_line)
                .then(|| cursor_pos(visual_col(&cells, cur_col), cols, need));

            // ponytail: no horizontal scrolling. A truncated line gives up its
            // last column to the marker; the tail is simply not reachable.
            // Upgrade path: an `hscroll` alongside `scroll`.
            let marker = (!wrap).then(|| truncation_marker(cells.len(), cols)).flatten();

            let mut ri = 0usize;
            for (r, (rs, re)) in wrap_rows(cells.len(), cols).take(fits).enumerate() {
                let y = doc.y + (row + r) as i32 * self.line_h;
                let cursor_col = cursor_at.and_then(|(cr, cc)| (cr == r).then_some(cc));
                // The marker replaces the last cell rather than following it:
                // drawn one column further right it would be the overflow it
                // exists to warn about.
                let shown = marker.unwrap_or(re - rs);

                if line == cur_line && selection.is_empty() {
                    self.fill(pane.x, y, pane.w, self.line_h, cur_bg);
                }
                // Overlay backgrounds go under the selection and the cursor: a
                // config painting a range must not be able to hide where the
                // editor thinks you are.
                for (col, &(_, src)) in cells[rs..rs + shown].iter().enumerate() {
                    if let (_, Some(k)) = overlay_face(&ov_runs, src) {
                        let x = x0 + col as i32 * self.cell_w;
                        let c = rgb(editor.theme.color(k, fg));
                        self.fill(x, y, self.cell_w, self.line_h, c);
                    }
                }
                for &s in &sel_cells {
                    if let Some((a, b)) = row_span(s, rs, cols) {
                        let x = x0 + a as i32 * self.cell_w;
                        self.fill(x, y, (b - a) as i32 * self.cell_w, self.line_h, sel_bg);
                    }
                }

                // The number belongs to the buffer line, not to each of the
                // rows it occupies: repeating it down a wrapped line would read
                // as several lines, so continuation rows get a blank gutter.
                // After the stripe, which is painted across the gutter too.
                if set.line_numbers && r == 0 {
                    let c = if line == cur_line { num_cur_c } else { num_c };
                    self.draw_str(&format!("{:>digits$}", line + 1), doc.x, y, c);
                }

                if block_cursor {
                    if let Some(cc) = cursor_col {
                        let x = x0 + cc as i32 * self.cell_w;
                        self.draw_cursor(x, y, self.cell_w, pane, cursor_c);
                    }
                }

                for (col, &(ch, src)) in cells[rs..rs + shown].iter().enumerate() {
                    let x = x0 + col as i32 * self.cell_w;
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
                    self.draw_char(ch, x, y, color);
                }
                // Images last of the text, over the blanks their substitution
                // reserved. One that started on an earlier display row is
                // skipped rather than clipped — a wrapped line's continuation
                // has no column for it.
                for &(vc, id) in &images {
                    if vc < rs || vc >= re {
                        continue;
                    }
                    let x = x0 + (vc - rs) as i32 * self.cell_w;
                    self.draw_image(editor, id, x, y);
                }
                if let Some(mc) = marker {
                    let c = if block_cursor && cursor_col == Some(mc) {
                        rgb(bg)
                    } else {
                        marker_c
                    };
                    self.draw_char(LineOverflow::MARKER, x0 + mc as i32 * self.cell_w, y, c);
                }

                if !block_cursor {
                    if let Some(cc) = cursor_col {
                        let x = x0 + cc as i32 * self.cell_w;
                        self.draw_cursor(x, y, 2, pane, cursor_c);
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
        let dim = rgb(mix(bg, fg, 0.72));
        let sel_bg = rgb(mix(bg, accent, 0.18));

        let lines = editor.dashboard.lines();
        let cols = visible_cols(doc.w, self.cell_w);
        let total = lines.len() as i32 * self.line_h;
        let y0 = doc.y + ((doc.h - total) / 2).max(0);

        for (i, (text, selected)) in lines.iter().enumerate() {
            let y = y0 + i as i32 * self.line_h;
            if y + self.line_h > doc.y + doc.h {
                break;
            }
            let n = text.chars().count() as i32;
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
            let color = if *selected {
                rgb(accent)
            } else if is_banner_line(text) {
                banner_c
            } else {
                dim
            };
            self.draw_str(text, x, y, color);
        }
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
        let right_cols: usize = right.iter().map(|s| s.text.chars().count()).sum();
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
            left -= text.chars().count();
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
        for (i, (text, selected)) in rows.iter().enumerate() {
            let ry = y + i as i32 * self.line_h;
            if ry + self.line_h > bottom {
                break;
            }
            let c = if *selected {
                self.fill(b.x + inset, ry, b.w - 2 * inset, self.line_h, sel_bg);
                self.draw_str("▸", x0, ry, rgb(accent));
                rgb(accent)
            } else {
                row_c
            };
            self.draw_str(&truncate(text, text_cols), x0 + 2 * self.cell_w, ry, c);
        }
    }

    // --- primitives -------------------------------------------------------

    /// Confine every following draw to `a`. Nothing else in the renderer knows
    /// about pane boundaries — this is what keeps a long line, a wide selection
    /// or an overhanging glyph inside the window it belongs to.
    fn set_clip(&mut self, a: Area) {
        let r = Rect::new(a.x, a.y, a.w.max(0) as u32, a.h.max(0) as u32);
        self.canvas.set_clip_rect(Some(r));
    }

    fn clear_clip(&mut self) {
        self.canvas.set_clip_rect(None);
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

    fn draw_cursor(&mut self, x: i32, y: i32, w: i32, pane: Area, color: Color) {
        self.fill(x, y, w.min(pane.x + pane.w - x), self.line_h, color);
    }

    /// Blit an overlay image into the row whose top-left is `(x, y)`.
    ///
    /// Uploaded once and kept — the id is a hash of the job that produced the
    /// pixels, so previewing the same equation again is a cache hit rather than
    /// a second texture. Straight (unpremultiplied) alpha, which is what
    /// [`BlendMode::Blend`] expects and what `zemacs-latex` promises.
    ///
    /// Vertical placement is the whole point of `Image::depth`: an image no
    /// taller than the line sits *on the baseline*, so `$x_1$` hangs below it
    /// exactly as the surrounding text's descenders do. A taller one — display
    /// math, which always has a line to itself — is hung from the top of its own
    /// row instead, because growing upward from the baseline would put it over
    /// the lines *above*, while the rows below it are the fragment's own source
    /// lines and are already blank.
    ///
    /// ponytail: no reflow either way. Nothing to the right of an image moves,
    /// so an image wider than the cells its substitution reserved paints over
    /// the text after it — which is only ever a rounding cell, since the
    /// reservation is `ceil(width / cell_w)`. The clip rect keeps it inside the
    /// pane. Upgrade path is a per-line table of cell advances, which is also
    /// what wide characters want.
    fn draw_image(&mut self, editor: &Editor, id: ImageId, x: i32, y: i32) {
        let Some(image) = editor.image(id) else {
            return;
        };
        let (w, h, depth) = (image.width, image.height, image.depth as i32);
        let iy = match h as i32 {
            ih if ih <= self.line_h => y + self.ascent + depth - ih,
            _ => y,
        };
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
        let _ = canvas.copy(tex, None, Rect::new(x, iy, w, h));
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
        if w <= 0 || h <= 0 {
            return;
        }
        self.canvas.set_draw_color(color);
        let _ = self.canvas.fill_rect(Rect::new(x, y, w as u32, h as u32));
    }

    /// Returns the x just past the last cell, so callers can chain / place a bar.
    fn draw_str(&mut self, s: &str, x: i32, y: i32, color: Color) -> i32 {
        self.draw_weighted(s, x, y, color, false)
    }

    /// `draw_str`, optionally in the bold face. Returns the x just past the
    /// last cell. Bold is *metrically* the same — the cell width never changes,
    /// so a bold run cannot shift the columns after it.
    fn draw_weighted(&mut self, s: &str, x: i32, y: i32, color: Color, bold: bool) -> i32 {
        let mut x = x;
        for c in s.chars() {
            if bold {
                self.draw_bold_char(c, x, y, color);
            } else {
                self.draw_char(c, x, y, color);
            }
            x += self.cell_w;
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
        }
    }

    /// `draw_str`, but `x` is where the text should *end*. Same glyph path.
    fn draw_right(&mut self, s: &str, x: i32, y: i32, color: Color) {
        let w = s.chars().count() as i32 * self.cell_w;
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
        }
    }
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
/// [`PAD`].
///
/// Returned as the *text* rectangle rather than the whole remainder so the row
/// count and the drawing loop cannot disagree about the inset.
fn doc_rect(pane: Area, status_h: i32) -> Area {
    let modeline = modeline_rect(pane, status_h);
    Area {
        x: pane.x + PAD,
        y: pane.y + PAD,
        w: (pane.w - 2 * PAD).max(0),
        h: (pane.h.max(0) - modeline.h - PAD).max(0),
    }
}

/// Document lines that fit in a pane — the window's `viewport_lines`.
///
/// Zero for a pane too short to show one: the modeline is still drawn, the text
/// just has nowhere to go. Core clamps this with `.max(1)` wherever it divides
/// by it, so an honest zero is safe and a lie about one row is not.
fn doc_lines(pane: Area, status_h: i32, line_h: i32) -> usize {
    (doc_rect(pane, status_h).h / line_h.max(1)).max(0) as usize
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

/// `s` cut to `cols` characters, ellipsised when anything was lost.
///
/// Counts *characters*: candidates are file paths and Lisp symbol names, so a
/// byte-wise cut would both mis-measure a non-ASCII row and panic on a split
/// codepoint.
fn truncate(s: &str, cols: usize) -> String {
    if s.chars().count() <= cols {
        return s.to_string();
    }
    match cols {
        0 => String::new(),
        1 => "…".to_string(),
        // ponytail: truncates the tail, so two long paths sharing a prefix look
        // identical. Ceiling: deep directory trees. Upgrade path: elide the
        // middle, keeping the basename.
        n => s.chars().take(n - 1).chain(['…']).collect(),
    }
}

/// A prompt label as a box title: `"Find file: "` -> `"Find file"`.
fn title_of(label: &str) -> &str {
    label.trim().trim_end_matches(':').trim_end()
}

/// Column at which to start drawing `s` so it lands centered in `cols` columns.
/// Counts *characters* — the dashboard banner is box-drawing art, so a byte
/// count would push it a third of the way off screen.
fn center_col(s: &str, cols: usize) -> usize {
    cols.saturating_sub(s.chars().count()) / 2
}

/// Box-drawing / block-element art versus prose. Lets the renderer tint the
/// dashboard banner without `Dashboard` having to describe its own layout.
fn is_banner_line(s: &str) -> bool {
    !s.trim().is_empty()
        && s.chars()
            .all(|c| c == ' ' || ('\u{2500}'..='\u{259f}').contains(&c))
}

/// Visual cells for one line: `(glyph, index of the source char in the line)`.
/// A tab becomes a run of spaces up to the next tab stop, every one of them
/// pointing back at the tab so cursor/selection/highlight math still works in
/// source-char offsets.
fn expand_line(line: &str, tab_width: usize) -> Vec<(char, usize)> {
    let tw = tab_width.max(1);
    let mut out = Vec::with_capacity(line.len());
    for (i, c) in line.chars().enumerate() {
        match c {
            '\t' => {
                for _ in 0..(tw - out.len() % tw) {
                    out.push((' ', i));
                }
            }
            '\n' | '\r' => {}
            _ => out.push((c, i)),
        }
    }
    out
}

// --- line overflow ---------------------------------------------------------
//
// Everything below counts *display cells*, never source chars and never bytes:
// a tab is several cells and a `→` is one, so wrapping on anything else puts
// the break in the wrong place (or, for bytes, mid-codepoint).

/// Display rows one line of `len` cells occupies in a pane `cols` wide.
///
/// Always at least one, so an empty line still has a row for its gutter number
/// and its cursor. `cols == 0` — a pane narrower than its own line numbers —
/// is the infinite-loop case: it yields one row rather than dividing by zero,
/// and the caller advances past the line either way.
fn wrap_row_count(len: usize, cols: usize) -> usize {
    match cols {
        0 => 1,
        c => len.div_ceil(c).max(1),
    }
}

/// The cell range `[start, end)` shown on each display row of a line of `len`
/// cells, in order. Truncation takes the first of these and nothing else.
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
fn lines_in_rows(heights: impl IntoIterator<Item = usize>, rows: usize) -> usize {
    let (mut used, mut lines) = (0usize, 0usize);
    for h in heights {
        if used >= rows {
            break;
        }
        used += h.max(1);
        lines += 1;
    }
    lines + rows.saturating_sub(used)
}

/// [`lines_in_rows`] for a real buffer. Only the visible lines are expanded —
/// the iterator is lazy and the count stops as soon as the pane is full — so a
/// 10k-line file costs one screenful, same as drawing it.
fn visible_lines(buf: &Buffer, scroll: usize, rows: usize, cols: usize, set: &Settings) -> usize {
    if set.line_overflow == LineOverflow::Truncate {
        return rows; // one row per line; the general path would agree, slower
    }
    let heights = (scroll..buf.len_lines()).map(|l| {
        let start = buf.line_start(l);
        let text = buf.slice_string(start, start + buf.line_len(l));
        wrap_row_count(expand_line(&text, set.tab_width).len(), cols)
    });
    lines_in_rows(heights, rows)
}

/// Digits reserved for the line-number gutter. Sized from the whole file so the
/// text column does not shift as you scroll into four-digit territory.
fn gutter_digits(buf: &Buffer) -> usize {
    buf.len_lines().to_string().len().max(3)
}

/// Width of the line-number gutter in pixels — zero when they are off. Both the
/// row count and the drawing loop need it, and they must not disagree about how
/// many columns the text has left.
fn gutter_w(buf: &Buffer, set: &Settings, cell_w: i32) -> i32 {
    match set.line_numbers {
        true => (gutter_digits(buf) as i32 + 1) * cell_w,
        false => 0,
    }
}

/// Visual column of source char `src`; the end of the line if it is past it
/// (which is where the cursor sits on an empty line, or at EOL in insert mode).
///
/// The *first* cell at or after `src`, not the cell for `src` exactly: a
/// [`substitute`]d range has no cell of its own for the characters it hides, and
/// a cursor inside one belongs at the front of what replaced them. On a line
/// with no overlays every source char still has a cell, so this is the same
/// answer it has always given.
fn visual_col(cells: &[(char, usize)], src: usize) -> usize {
    match cells.iter().position(|&(_, i)| i >= src) {
        // The ordinary answer, and the only one on a line with no overlays.
        Some(k) if cells[k].1 == src => k,
        // A source char with no cell of its own, because something replaced the
        // range it was in. The cursor belongs at the *front* of what replaced
        // it: landing after it would put the cursor a character further right
        // than the buffer says it is.
        Some(_) => cells.iter().rposition(|&(_, i)| i < src).map_or(0, |k| {
            cells.iter().position(|&(_, i)| i == cells[k].1).unwrap_or(k)
        }),
        // Past the last character — where the cursor sits on an empty line, or
        // at EOL in insert mode.
        None => cells.len(),
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

/// Cells a bitmap `width` pixels across reserves. At least one, so an image
/// narrower than a cell still hides the character it replaced.
fn image_cells(width: u32, cell_w: i32) -> usize {
    (width as usize).div_ceil(cell_w.max(1) as usize).max(1)
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

    #[test]
    fn tabs_expand_to_the_next_tab_stop() {
        assert_eq!(expand_line("\tx", 4).len(), 5);
        // "ab\tc": two chars, then 2 spaces to reach column 4, then 'c'.
        let cells = expand_line("ab\tc", 4);
        assert_eq!(
            cells.iter().map(|&(c, _)| c).collect::<String>(),
            "ab  c"
        );
        // Every expanded space maps back to the tab at source index 2.
        assert_eq!(cells[2].1, 2);
        assert_eq!(cells[3].1, 2);
        assert_eq!(cells[4].1, 3);
        // tab_width 0 must not divide by zero.
        assert_eq!(expand_line("\t", 0).len(), 1);
    }

    #[test]
    fn visual_col_follows_tab_expansion() {
        let cells = expand_line("\tfn", 4);
        assert_eq!(visual_col(&cells, 0), 0); // the tab itself
        assert_eq!(visual_col(&cells, 1), 4); // 'f' after the tab stop
        assert_eq!(visual_col(&cells, 3), 6); // past EOL -> end of the line
        assert_eq!(visual_col(&expand_line("", 4), 0), 0);
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

    // --- line overflow ----------------------------------------------------

    /// The glyphs a row shows, so a wrap can be asserted on what you'd read.
    fn row_text(cells: &[(char, usize)], (s, e): (usize, usize)) -> String {
        cells[s..e].iter().map(|&(c, _)| c).collect()
    }

    /// `visible_lines` for `text` wrapped in a `cols`-wide, `rows`-tall pane —
    /// the path the renderer actually takes, so a wrap measured in the wrong
    /// unit shows up here rather than only in the helper it was measured with.
    fn wrapped_lines(text: &str, rows: usize, cols: usize) -> usize {
        let set = Settings {
            line_overflow: LineOverflow::Wrap,
            ..Settings::default()
        };
        visible_lines(&Buffer::from_str(text), 0, rows, cols, &set)
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
        let doc = doc_rect(pane, mh());
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
        let doc = doc_rect(pane, mh());
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
        assert_eq!(visible_lines(&buf, 0, 20, 10, &set), 20);
        set.line_overflow = LineOverflow::Wrap;
        // 100 cells in a 10-column pane is 10 rows for the first line alone,
        // then three one-row lines, then seven rows of nothing.
        assert_eq!(visible_lines(&buf, 0, 20, 10, &set), 4 + 7);
        // Scrolled past the long line, the wrapped and truncated counts agree
        // again — nothing on screen is wider than the pane.
        assert_eq!(visible_lines(&buf, 1, 20, 10, &set), 20);
        // Tall enough to matter: the long line alone fills a 5-row pane, and
        // the count must not drop to zero or scrolling stops dead.
        assert_eq!(visible_lines(&buf, 0, 5, 10, &set), 1);
        assert_eq!(visible_lines(&buf, 0, 0, 10, &set), 0);
        // A pane with no columns must not hang: every line is one row.
        assert_eq!(visible_lines(&buf, 0, 5, 0, &set), 4 + 1);
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

    #[test]
    fn neither_mode_puts_a_cell_outside_its_pane() {
        // A 30-column left pane and a 500-character line, in both modes: every
        // column that gets drawn ends inside the pane, cursor and marker
        // included. This is the bug the whole feature exists for.
        let pane = Area { x: 0, y: 0, w: 2 * PAD + 30 * CW, h: 600 };
        let doc = doc_rect(pane, mh());
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
        let doc = doc_rect(pane, mh());
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
            let doc = doc_rect(pane, mh());
            assert!(doc.w >= 0 && doc.h >= 0, "h={h} -> {doc:?}");
        }
        // One pixel more than the chrome plus a line, and one row appears.
        let pane = Area { x: 0, y: 0, w: 400, h: mh() + PAD + LH };
        assert_eq!(doc_lines(pane, mh(), LH), 1);
        assert_eq!(doc_lines(pane, mh(), 0), doc_rect(pane, mh()).h as usize); // no /0
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
        let doc = doc_rect(pane, mh());
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
}
