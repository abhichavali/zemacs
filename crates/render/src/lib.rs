//! zemacs-render — the SDL2 window and text renderer.
//!
//! The whole frame is drawn with two primitives: filled rectangles
//! (`Canvas::fill_rect`) and single-glyph textures blitted at monospace cell
//! positions. That is *why* SDL2 instead of a GPU text stack: the cursor, the
//! visual selection, the current-line highlight and the status strip are all
//! just rects, and the text is a grid — no shaping, no atlas, no pipeline.
//!
//! Glyphs are cached one texture per `char`, rasterised white/blended once and
//! recoloured per draw with `set_color_mod`. So a frame costs one `copy` per
//! visible cell and zero rasterisation after the first sighting of a character.
//! The cache is thrown away when the font size (or the display's DPI) changes.
//!
//! Everything is measured in *cells*: `cell_w` is the font's advance for `'M'`
//! and `line_h` is `recommended_line_spacing()`. Layout therefore assumes a
//! monospace font; the font search below only offers monospace faces.

use std::collections::HashMap;
use std::path::PathBuf;

use sdl2::pixels::Color;
use sdl2::rect::Rect;
use sdl2::render::{BlendMode, Texture, TextureCreator, WindowCanvas};
use sdl2::ttf::{Font, Sdl2TtfContext};
use sdl2::video::WindowContext;
use zemacs_core::{CompletionStyle, Editor, HlKind, Mode, Span};

/// Outer margin, in pixels, around the text area and inside the status strip.
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
    cell_w: i32,
    line_h: i32,
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
        let font = ttf
            .load_font(&font_path, point_size)
            .map_err(|e| anyhow::anyhow!("cannot open font {}: {e}", font_path.display()))?;
        let (cell_w, line_h) = metrics(&font);

        Ok(Self {
            canvas,
            ttf,
            textures,
            font,
            font_path,
            point_size,
            glyphs: HashMap::new(),
            cell_w,
            line_h,
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
        self.point_size = want;
        self.glyphs.clear(); // rasterised at the old size, all of it is stale
        let (cell_w, line_h) = metrics(&self.font);
        self.cell_w = cell_w;
        self.line_h = line_h;
        Ok(())
    }

    /// Draw one frame. Writes `editor.viewport_lines` back because only the
    /// renderer knows how many lines actually fit, and the core scroll logic
    /// needs it.
    pub fn render(&mut self, editor: &mut Editor) -> anyhow::Result<()> {
        self.sync(editor)?; // cheap no-op; makes an app-side `sync` call optional

        let (w, h) = self
            .canvas
            .output_size()
            .map_err(|e| anyhow::anyhow!("output size: {e}"))?;
        let (w, h) = (w as i32, h as i32);

        let status_h = self.line_h + PAD;
        let text_h = (h - status_h - PAD).max(self.line_h);
        editor.viewport_lines = (text_h / self.line_h).max(1) as usize;

        let bg = editor.settings.background;
        self.canvas.set_draw_color(rgb(bg));
        self.canvas.clear();

        if editor.mode == Mode::Dashboard {
            self.draw_dashboard(editor, w, h - status_h);
        } else {
            self.draw_document(editor, w, text_h);
        }
        self.draw_status(editor, w, h, status_h);
        // Last, and over the status strip's scrim too: the popup is the only
        // live thing on screen while it is up.
        self.draw_completion(editor, w, h, status_h);

        self.canvas.present();
        Ok(())
    }

    // --- document ---------------------------------------------------------

    fn draw_document(&mut self, editor: &Editor, w: i32, text_h: i32) {
        let buf = &editor.buffer;
        let set = &editor.settings;
        let (bg, fg) = (set.background, set.foreground);
        let (cur_line, cur_col) = buf.cursor_line_col();
        let selection = editor.selection();
        let block_cursor = editor.mode != Mode::Insert;

        let digits = buf.len_lines().to_string().len().max(3);
        let gutter = if set.line_numbers {
            (digits as i32 + 1) * self.cell_w
        } else {
            0
        };
        let x0 = PAD + gutter;

        let sel_bg = rgb(mix(bg, fg, 0.28));
        let cur_bg = rgb(mix(bg, fg, 0.05));
        let cursor_c = rgb(mix(bg, fg, 0.85));
        let num_c = rgb(mix(bg, fg, 0.35));
        let num_cur_c = rgb(mix(bg, fg, 0.7));

        // Highlight spans are whole-buffer char offsets and sorted, so one
        // monotonic cursor walks them alongside the lines — no rescan per line.
        let mut si = 0usize;
        let rows = (text_h / self.line_h) as usize;

        for row in 0..rows {
            let line = editor.scroll + row;
            if line >= buf.len_lines() {
                break;
            }
            let y = PAD + row as i32 * self.line_h;
            let start = buf.line_start(line);
            let len = buf.line_len(line);
            let end = start + len;

            let (next, runs) = spans_for_line(&editor.highlights, si, start, end);
            si = next;

            let cells = expand_line(&buf.slice_string(start, end), set.tab_width);

            if line == cur_line && selection.is_none() {
                self.fill(0, y, w, self.line_h, cur_bg);
            }

            // Selection: `end + 1` is the newline, drawn as one trailing cell so
            // a linewise selection visibly swallows the line break.
            if let Some((s, e)) = selection {
                if e > start && s <= end {
                    let a = s.max(start) - start;
                    let b = e.min(end + 1) - start;
                    let c0 = visual_col(&cells, a) as i32;
                    let c1 = if b > len {
                        cells.len() as i32 + 1
                    } else {
                        visual_col(&cells, b) as i32
                    };
                    if c1 > c0 {
                        self.fill(x0 + c0 * self.cell_w, y, (c1 - c0) * self.cell_w, self.line_h, sel_bg);
                    }
                }
            }

            let cursor_vc = (line == cur_line).then(|| visual_col(&cells, cur_col) as i32);
            if block_cursor {
                if let Some(vc) = cursor_vc {
                    self.fill(x0 + vc * self.cell_w, y, self.cell_w, self.line_h, cursor_c);
                }
            }

            if set.line_numbers {
                let c = if line == cur_line { num_cur_c } else { num_c };
                self.draw_str(&format!("{:>digits$}", line + 1), PAD, y, c);
            }

            // ponytail: no horizontal scrolling — long lines are clipped at the
            // window edge. Upgrade path: an `hscroll` offset alongside `scroll`.
            let mut ri = 0usize;
            for (vc, &(ch, src)) in cells.iter().enumerate() {
                let x = x0 + vc as i32 * self.cell_w;
                if x >= w {
                    break;
                }
                while ri < runs.len() && runs[ri].1 <= src {
                    ri += 1;
                }
                let kind = match runs.get(ri) {
                    Some(&(s, _, k)) if s <= src => k,
                    _ => HlKind::Default,
                };
                let color = if block_cursor && cursor_vc == Some(vc as i32) {
                    rgb(bg) // knock the glyph out of the cursor block
                } else {
                    rgb(editor.theme.color(kind, fg))
                };
                self.draw_char(ch, x, y, color);
            }

            if !block_cursor {
                if let Some(vc) = cursor_vc {
                    self.fill(x0 + vc * self.cell_w, y, 2, self.line_h, cursor_c);
                }
            }
        }
    }

    // --- dashboard --------------------------------------------------------

    fn draw_dashboard(&mut self, editor: &Editor, w: i32, avail_h: i32) {
        let (bg, fg) = (editor.settings.background, editor.settings.foreground);
        let accent = editor.theme.color(HlKind::Function, fg);
        let banner_c = rgb(editor.theme.color(HlKind::Keyword, fg));
        let dim = rgb(mix(bg, fg, 0.72));
        let sel_bg = rgb(mix(bg, accent, 0.18));

        let lines = editor.dashboard.lines();
        let cols = ((w - 2 * PAD).max(0) / self.cell_w) as usize;
        let total = lines.len() as i32 * self.line_h;
        let y0 = (PAD + (avail_h - total) / 2).max(PAD);

        for (i, (text, selected)) in lines.iter().enumerate() {
            let y = y0 + i as i32 * self.line_h;
            if y + self.line_h > avail_h {
                break;
            }
            let n = text.chars().count() as i32;
            let x = PAD + center_col(text, cols) as i32 * self.cell_w;
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

    // --- status line ------------------------------------------------------

    fn draw_status(&mut self, editor: &Editor, w: i32, h: i32, status_h: i32) {
        let (bg, fg) = (editor.settings.background, editor.settings.foreground);
        let y = h - status_h;
        self.fill(0, y, w, status_h, rgb(mix(bg, fg, 0.09)));
        self.fill(0, y, w, 1, rgb(mix(bg, fg, 0.20)));

        let text = editor.status_line();
        let ty = y + PAD / 2;
        let color = rgb(mix(bg, fg, if editor.prompt.is_some() { 1.0 } else { 0.85 }));
        let end = self.draw_str(&text, PAD, ty, color);
        if editor.prompt.is_some() {
            self.fill(end, ty, 2, self.line_h, rgb(fg));
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

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
        if w <= 0 || h <= 0 {
            return;
        }
        self.canvas.set_draw_color(color);
        let _ = self.canvas.fill_rect(Rect::new(x, y, w as u32, h as u32));
    }

    /// Returns the x just past the last cell, so callers can chain / place a bar.
    fn draw_str(&mut self, s: &str, x: i32, y: i32, color: Color) -> i32 {
        let mut x = x;
        for c in s.chars() {
            self.draw_char(c, x, y, color);
            x += self.cell_w;
        }
        x
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
    let mut tex = textures.create_texture_from_surface(&surface).ok()?;
    tex.set_blend_mode(BlendMode::Blend);
    Some(tex)
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

/// Visual column of source char `src`; the end of the line if it is past it
/// (which is where the cursor sits on an empty line, or at EOL in insert mode).
fn visual_col(cells: &[(char, usize)], src: usize) -> usize {
    cells
        .iter()
        .position(|&(_, i)| i == src)
        .unwrap_or(cells.len())
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
