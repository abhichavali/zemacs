//! zemacs-figure — one image *file* in, one bitmap out.
//!
//! The sibling of `zemacs-latex`, and deliberately the same shape: it knows how
//! to turn a path into pixels the renderer can upload as a texture, and it does
//! not know that org, buffers or overlays exist. Everything about *which* files
//! become figures, and where the overlay goes, is Lisp's — see
//! `org-inline-images` in `runtime/modes/org-modern.lisp`.
//!
//! It exists because the display path already had everything else. An overlay
//! carrying `image` draws a bitmap instead of the cells it covers, the renderer
//! grows the line to fit it, and `Editor::add_image` remembers pixels under an
//! id. The only hole was that the *only* producer of those pixels was a LaTeX
//! run — so `[[file:fig-1.svg]]` had nowhere to get an id from. This fills that
//! hole and nothing else: no second image cache, no second overlay property, no
//! second eviction policy. `rs_image_file` in `zemacs-lisp` hands what comes out
//! of here to the same `add_image` the LaTeX primitive uses, keyed by a hash of
//! exactly the inputs that decided the pixels, so re-rendering an unchanged
//! figure is a lookup and the renderer's texture for it is still warm.
//!
//! **Two formats and one rule.** SVG through `resvg`, PNG through the decoder
//! `zemacs-latex` already builds. The rule is that [`load`]'s `max_width` is a
//! *maximum*: nothing is ever scaled up past the size it was authored at, so a
//! small diagram stays a small diagram and a plot exported at 1600 px is brought
//! down to something that fits a pane.
//!
//! SVG is the exception that proves it. A vector file has no pixels to be
//! authored at, only CSS px, so it is rasterised at `scale` — the display's
//! device-pixel ratio, which the caller knows and this does not — before the
//! maximum applies. That is the difference between a figure that matches the
//! text beside it on a Retina display and one that comes out half-height, and it
//! is exactly the correction `zemacs-latex` makes with its dpi.
//!
//! ponytail: PNG and SVG, not JPEG or WebP. PNG is what a plotting library and
//! an LLM both emit, SVG is what you actually want for a diagram, and a
//! photograph is not a figure in a textbook. The upgrade is one decoder crate
//! and one arm in [`decode_raster`]; `resvg` already builds `zune-jpeg` for
//! rasters *embedded in* an SVG, so the shape is there.
//!
//! ponytail: no cache. `zemacs-latex` keeps one on disk because a cold render
//! shells out to TeX twice and costs a few hundred milliseconds; reading a file
//! and rasterising it is a millisecond or two, and the in-memory dedupe by id
//! one layer up already stops it happening twice for the same figure.

use std::path::Path;

use anyhow::{bail, Context, Result};

/// A decoded figure: straight-alpha RGBA, ready for `zemacs_core::Image`.
///
/// Its own type rather than core's, for the reason `zemacs-latex::Preview` is
/// its own type: this crate is a converter and has no business knowing what an
/// editor is. The three fields are what the renderer needs and nothing more.
pub struct Figure {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4`, `R G B A`, straight (not premultiplied) alpha.
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Figure {
    /// Hand-written for `zemacs_core::Image`'s reason: without it one `{:?}` of
    /// a failed assertion prints a megabyte of pixel values.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Figure")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// The widest bitmap we will hand back whatever the caller asks for.
///
/// Not a policy about layout — that is `max_width`'s job — but a floor under
/// how badly a hostile or mistaken file can behave. At 4 bytes a pixel a
/// 8192-wide figure is already 32 MB a row, and a `.org` file is allowed to name
/// any path on the disk.
const HARD_MAX: u32 = 8192;

/// Decode `path` and answer its pixels.
///
/// `max_width` is in device pixels and is a *maximum*; `0` means "as authored".
/// `scale` is the display's device-pixel ratio and applies to vector formats
/// only, because a raster file's pixels are already device pixels.
///
/// Errors are for the status line — the caller has no recovery, only a message —
/// so they carry the path and stay on one line under `{:#}`.
pub fn load(path: &Path, max_width: u32, scale: f32) -> Result<Figure> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    if bytes.is_empty() {
        bail!("{} is empty", path.display());
    }
    let cap = if max_width == 0 {
        HARD_MAX
    } else {
        max_width.min(HARD_MAX)
    };
    if is_svg(path, &bytes) {
        render_svg(path, &bytes, cap, scale)
    } else {
        decode_raster(&bytes, cap)
    }
}

/// Whether to hand `bytes` to the SVG parser.
///
/// The extension first, because that is what the author meant, and a sniff
/// after it so a figure written out without one still works. `.svgz` is gzipped
/// SVG and `usvg` un-gzips it itself, which is why it is not a third case here.
///
/// The sniff is deliberately weak — an XML declaration, a doctype or an `<svg`
/// element, whichever comes first — because getting it wrong costs a parse error
/// with the path in it and not a crash.
fn is_svg(path: &Path, bytes: &[u8]) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ext == "svg" || ext == "svgz" {
        return true;
    }
    if !ext.is_empty() {
        return false;
    }
    let head = &bytes[..bytes.len().min(512)];
    head.starts_with(b"\x1f\x8b") // gzip, i.e. an unlabelled .svgz
        || String::from_utf8_lossy(head).trim_start().starts_with('<')
}

/// Rasterise an SVG at `scale`, brought down to `cap` if that would be too wide.
///
/// `resources_dir` is what lets a figure `<image href="detail.png"/>` a file
/// beside itself, which is how a plotting library writes a chart with a bitmap
/// inset. Without it those references resolve against the process's working
/// directory, which is wherever the editor happened to be launched from.
///
/// System fonts are loaded because a chart's axis labels are text and a figure
/// whose labels silently vanished is worse than an error. It costs one scan of
/// the font directories on the first figure of a session; `fontdb` is what
/// `usvg` caches it in, and `Options` is rebuilt per call — see the ceiling
/// below.
///
/// ponytail: that font scan happens once *per figure*, not once per session,
/// because `usvg::Options` is built here rather than kept. It is tens of
/// milliseconds on a machine with a lot of fonts, on the Lisp thread, and only
/// on a buffer full of SVGs. The fix is a `OnceLock<Arc<fontdb::Database>>`
/// beside this function the day a curriculum full of plots feels slow to open.
fn render_svg(path: &Path, bytes: &[u8], cap: u32, scale: f32) -> Result<Figure> {
    use resvg::{tiny_skia, usvg};

    let mut opt = usvg::Options {
        resources_dir: path.parent().map(|p| p.to_path_buf()),
        ..usvg::Options::default()
    };
    opt.fontdb_mut().load_system_fonts();

    let tree = usvg::Tree::from_data(bytes, &opt)
        .with_context(|| format!("cannot parse {}", path.display()))?;
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    if !(w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
        bail!("{} has no size", path.display());
    }

    // The device-pixel correction first, then the maximum on top of it — in that
    // order, or a figure that fits the pane on a 1× display would be cut to half
    // its width on a 2× one before ever being measured against the cap.
    let scale = scale.max(0.01);
    let scale = if (w * scale).ceil() > cap as f32 {
        cap as f32 / w
    } else {
        scale
    };
    let (pw, ph) = (
        ((w * scale).round() as u32).clamp(1, HARD_MAX),
        ((h * scale).round() as u32).clamp(1, HARD_MAX),
    );

    let mut pixmap =
        tiny_skia::Pixmap::new(pw, ph).with_context(|| format!("{pw}x{ph} is too large"))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(Figure {
        width: pw,
        height: ph,
        rgba: demultiply(&pixmap),
    })
}

/// tiny-skia paints in premultiplied alpha and `zemacs_core::Image` is straight,
/// which is the one conversion between the two worlds.
///
/// Not cosmetic: the renderer uploads these bytes as a straight-alpha texture,
/// so leaving them premultiplied would darken every antialiased edge towards
/// black — the same halo `zemacs-latex` overwrites dvipng's white palette entry
/// to avoid, in the other direction.
fn demultiply(pixmap: &resvg::tiny_skia::Pixmap) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixmap.data().len());
    for px in pixmap.pixels() {
        let c = px.demultiply();
        out.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    out
}

/// Decode a PNG to straight-alpha RGBA, then bring it down to `cap`.
///
/// The transformations are `zemacs-latex`'s, and for the same reason: `EXPAND`
/// turns a palette into RGB and a `tRNS` chunk into an alpha channel, `ALPHA`
/// covers a file that has neither, and `STRIP_16` guarantees eight bits a
/// sample. Between them every PNG a plotting library or an image editor writes
/// arrives here as one of four colour types.
fn decode_raster(bytes: &[u8], cap: u32) -> Result<Figure> {
    use png::{ColorType, Transformations};

    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(
        Transformations::EXPAND | Transformations::ALPHA | Transformations::STRIP_16,
    );
    let mut reader = decoder
        .read_info()
        .context("not a PNG, and not an SVG either")?;
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf).context("truncated PNG")?;
    buf.truncate(info.buffer_size());

    let rgba: Vec<u8> = match info.color_type {
        ColorType::Rgba => buf,
        ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        ColorType::Indexed => bail!("PNG palette was not expanded"),
    };
    let (width, height) = (info.width, info.height);
    if rgba.len() as u64 != u64::from(width) * u64::from(height) * 4 {
        bail!(
            "PNG is {width}x{height} but decoded to {} bytes",
            rgba.len()
        );
    }
    Ok(shrink(
        Figure {
            width,
            height,
            rgba,
        },
        cap,
    ))
}

/// Halve, third or quarter a bitmap until it fits `cap`, averaging each block.
///
/// An *integer* factor and a box average, which is not the best resampler and is
/// the one that needs no filter kernel, no floating point and no second pass:
/// the case that actually happens is a figure exported at 2× or 3× for print,
/// and an integer factor is exactly right for those and only slightly soft for
/// everything else. A figure already narrow enough is handed straight back with
/// no copy at all.
///
/// Averaged in *premultiplied* alpha and demultiplied afterwards, because
/// averaging straight RGB across a transparent pixel pulls whatever colour was
/// parked under `alpha = 0` into its visible neighbours — the halo again.
///
/// ponytail: the ceiling is that 1.5× is not expressible, so a 1200 px figure
/// against an 800 px cap is halved to 600 rather than fitted to 800. Nobody has
/// noticed; the upgrade is a Lanczos pass, which is a kernel and a scanline
/// buffer.
fn shrink(fig: Figure, cap: u32) -> Figure {
    if fig.width <= cap || cap == 0 {
        return fig;
    }
    let factor = fig.width.div_ceil(cap).max(2) as usize;
    let (w, h) = (fig.width as usize, fig.height as usize);
    let (nw, nh) = ((w / factor).max(1), (h / factor).max(1));
    let mut out = Vec::with_capacity(nw * nh * 4);
    for by in 0..nh {
        for bx in 0..nw {
            let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
            for dy in 0..factor {
                for dx in 0..factor {
                    let i =
                        (((by * factor + dy).min(h - 1)) * w + (bx * factor + dx).min(w - 1)) * 4;
                    let alpha = u32::from(fig.rgba[i + 3]);
                    r += u32::from(fig.rgba[i]) * alpha / 255;
                    g += u32::from(fig.rgba[i + 1]) * alpha / 255;
                    b += u32::from(fig.rgba[i + 2]) * alpha / 255;
                    a += alpha;
                }
            }
            let n = (factor * factor) as u32;
            let (r, g, b, a) = (r / n, g / n, b / n, a / n);
            // Back to straight alpha. A fully transparent block has no colour to
            // recover and is written as transparent black rather than divided by
            // zero.
            let un = |v: u32| {
                if a == 0 {
                    0
                } else {
                    ((v * 255 / a).min(255)) as u8
                }
            };
            out.extend_from_slice(&[un(r), un(g), un(b), a as u8]);
        }
    }
    Figure {
        width: nw as u32,
        height: nh as u32,
        rgba: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2×2 SVG rasterised at 1× is 2 px across; at 2× it is 4, which is the
    /// whole of the Retina correction. Nothing here needs a font, so this runs
    /// on a machine with none.
    #[test]
    fn an_svg_is_rasterised_at_the_display_scale() {
        let dir = std::env::temp_dir().join("zemacs_figure_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("square.svg");
        std::fs::write(
            &path,
            // `r##` and not `r#`: a `"#` closes the shorter delimiter, and an
            // SVG fill is written `"#ff0000"`.
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2">
                  <rect width="2" height="2" fill="#ff0000"/>
                </svg>"##,
        )
        .unwrap();

        let one = load(&path, 0, 1.0).expect("1x");
        assert_eq!((one.width, one.height), (2, 2));
        // Opaque red, straight alpha — premultiplied would be the same here, so
        // the demultiply is proved by the alpha rather than by the colour.
        assert_eq!(&one.rgba[..4], &[255, 0, 0, 255]);

        let two = load(&path, 0, 2.0).expect("2x");
        assert_eq!((two.width, two.height), (4, 4));

        // ...and the maximum wins over the scale, in that order: 2× would be 4 px
        // and the cap says 3, so it comes back at 3 rather than at 4 or at 1.
        let capped = load(&path, 3, 2.0).expect("capped");
        assert_eq!(capped.width, 3);
    }

    /// The maximum is a maximum: a figure narrower than it is untouched, and a
    /// wider one comes down by a whole factor.
    #[test]
    fn shrink_only_ever_shrinks() {
        let fig = Figure {
            width: 4,
            height: 2,
            rgba: vec![255; 4 * 2 * 4],
        };
        let same = shrink(fig, 8);
        assert_eq!((same.width, same.height), (4, 2));
        let half = shrink(same, 2);
        assert_eq!((half.width, half.height), (2, 1));
        assert_eq!(half.rgba, vec![255; 2 * 1 * 4]);
    }

    /// A file that is neither says so with the path in the message, because the
    /// only thing the caller can do with the error is put it on the status line.
    #[test]
    fn an_unreadable_path_is_an_error_and_not_a_panic() {
        let missing = std::path::Path::new("/nonexistent/zemacs/figure.png");
        let e = load(missing, 0, 1.0).unwrap_err();
        assert!(format!("{e:#}").contains("figure.png"), "{e:#}");
    }
}
