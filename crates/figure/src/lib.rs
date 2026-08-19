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
//! **Three formats and one rule.** SVG through `resvg`, PNG through the decoder
//! `zemacs-latex` already builds, JPEG through the one `resvg` already builds.
//! The rule is that [`load`]'s `max_width` is a *maximum*: nothing is ever
//! scaled up past the size it was authored at, so a small diagram stays a small
//! diagram and a plot exported at 1600 px is brought down to something that fits
//! a pane.
//!
//! Which of the two raster decoders runs is decided by the file's *magic
//! number* and not by its name, so a `.png` that something re-saved as a JPEG
//! still reads.
//!
//! SVG is the exception that proves it. A vector file has no pixels to be
//! authored at, only CSS px, so it is rasterised at `scale` — the display's
//! device-pixel ratio, which the caller knows and this does not — before the
//! maximum applies. That is the difference between a figure that matches the
//! text beside it on a Retina display and one that comes out half-height, and it
//! is exactly the correction `zemacs-latex` makes with its dpi.
//!
//! JPEG was the exception this note used to argue against: "a photograph is not
//! a figure in a textbook", which held for as long as a figure arrived by being
//! typed as a link. It stopped holding the moment you could *drag* one in — what
//! you drag is a photograph, and refusing the commonest format on the machine
//! made the gesture look broken. The upgrade the note predicted turned out to be
//! exactly what it said: one line in the manifest for a crate `resvg` already
//! builds, and one arm in [`decode_raster`].
//!
//! ponytail: still no WebP, and no animation in any format. Ceiling: a `.webp`
//! saved out of a browser, which is a real thing and is one more decoder crate —
//! a real one this time, since nothing here builds one already.
//!
//! ponytail: no cache. `zemacs-latex` keeps one on disk because a cold render
//! shells out to TeX twice and costs a few hundred milliseconds; reading a PNG
//! and decoding it is a millisecond or two, an SVG of a page's worth of shapes
//! and labels is a few more, and the in-memory dedupe by id one layer up
//! already stops either happening twice for the same figure. The one cost here
//! that was never a millisecond is the *font* scan an SVG needs, which is
//! hundreds of them and is why [`fonts`] exists.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, bail, Context, Result};

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
/// whose labels silently vanished is worse than an error. `Options` is still
/// rebuilt per call — it is a handful of fields and one of them, the resources
/// directory, differs per figure — but the expensive field is borrowed from
/// [`fonts`] rather than filled in again.
fn render_svg(path: &Path, bytes: &[u8], cap: u32, scale: f32) -> Result<Figure> {
    use resvg::{tiny_skia, usvg};

    let opt = usvg::Options {
        resources_dir: path.parent().map(|p| p.to_path_buf()),
        // Not `fontdb_mut()`: that is `Arc::make_mut`, which would deep-copy
        // every one of the 1693 faces the moment the handle is shared — the
        // cure costing more than the disease.
        fontdb: fonts(),
        ..usvg::Options::default()
    };

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

/// The system's fonts, scanned once for the life of the process.
///
/// The scan is the whole cost of an SVG figure. Measured on a 420×200 diagram
/// at 2×, it is a few hundred milliseconds against a cold page cache and 40 ms
/// warm, where parsing and rasterising the diagram itself is 4 ms — so a
/// section with ten plots in it spent nearly half a second walking
/// `/System/Library/Fonts` ten times over, on the Lisp thread, for ten
/// identical answers. `usvg::Options::fontdb` is an
/// `Arc<fontdb::Database>` precisely so that answer can be shared, and `usvg`
/// only ever reads it: a custom `font_resolver` that wants to add a face during
/// parsing clones the database first, which is a path this crate never takes.
/// `OnceLock` makes the sharing sound from the Lisp thread, the image thread
/// and whatever calls this next — the losers of the race drop their own scan
/// and take the winner's.
///
/// The first figure of a session still pays, and pays on whichever thread
/// happens to draw it. That is tolerable where it lands today because it lands
/// on the Lisp thread during `org-inline-images`, which is already the slow
/// part of opening a document full of figures and is not a keystroke. If a
/// half-second stall on the *first* `.org` file ever gets noticed, the fix is
/// to call [`load`] on anything at all from `zemacs-app`'s startup — this is
/// deliberately not done here, because a library crate that spawns a thread on
/// first use is a library crate you cannot reason about.
///
/// ponytail: one database for every figure, so a per-document font path — a
/// `#+FONTS:` header, a figure that ships its own `.ttf` — would have to bypass
/// this rather than extend it. Nothing passes per-figure font configuration
/// today ([`load`] takes a path, a width and a scale, and its one caller is
/// `rs_image_file`), so there is nothing to break. The upgrade is `usvg`'s own
/// `font_resolver`, which is built for exactly that and takes this database as
/// its base.
fn fonts() -> Arc<resvg::usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = resvg::usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone()
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
    // Sniffed, not switched on the extension. The caller reached here because a
    // *name* said this was a raster, and a name is a claim; two magic numbers
    // are what the file actually is. A `.png` that Preview re-saved as a JPEG
    // is a real thing and would otherwise fail with "not a PNG, and not an SVG
    // either" about a file that is perfectly readable.
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return decode_jpeg(bytes, cap);
    }
    decode_png(bytes, cap)
}

/// A JPEG, which is what a photograph is.
///
/// `zune-jpeg` rather than a second image crate because `resvg` already builds
/// it — see the manifest. Asked for RGBA directly, so there is no channel
/// shuffle here the way the PNG path needs one: a JPEG has no alpha to preserve
/// and the decoder fills it with 255.
fn decode_jpeg(bytes: &[u8], cap: u32) -> Result<Figure> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(bytes), options);
    let rgba = decoder.decode().map_err(|e| anyhow!("not a JPEG: {e}"))?;
    let (width, height) = decoder
        .dimensions()
        .map(|(w, h)| (w as u32, h as u32))
        .ok_or_else(|| anyhow!("JPEG has no dimensions"))?;
    if rgba.len() as u64 != u64::from(width) * u64::from(height) * 4 {
        bail!(
            "JPEG is {width}x{height} but decoded to {} bytes",
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

fn decode_png(bytes: &[u8], cap: u32) -> Result<Figure> {
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

    /// A photograph is a JPEG, and dragging one into a document is the gesture
    /// that made this format worth decoding at all.
    ///
    /// The bytes are a real JPEG built here rather than a fixture, so the test
    /// carries no binary: a 1×1 baseline grey image, which is the smallest
    /// thing `zune-jpeg` will accept. What is asserted is the contract every
    /// caller depends on — RGBA out, four bytes per pixel, opaque — because the
    /// decoder is asked for a colourspace it has to convert into and getting
    /// that argument wrong yields three bytes per pixel and a renderer that
    /// reads past the end of the buffer.
    #[test]
    fn a_jpeg_decodes_to_opaque_rgba() {
        let dir = std::env::temp_dir().join("zemacs_figure_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("photo.jpg");
        std::fs::write(&path, ONE_PIXEL_JPEG).unwrap();

        let fig = load(&path, 0, 1.0).expect("a JPEG must decode");
        assert_eq!((fig.width, fig.height), (1, 1));
        assert_eq!(fig.rgba.len(), 4, "four bytes per pixel, alpha included");
        assert_eq!(fig.rgba[3], 255, "a JPEG has no transparency");

        // ...and the format is decided by the *bytes*, not the name. A `.png`
        // that Preview re-saved as a JPEG is a real thing, and before the sniff
        // it failed with "not a PNG, and not an SVG either" about a file that
        // reads perfectly well.
        let lying = dir.join("actually-a-jpeg.png");
        std::fs::write(&lying, ONE_PIXEL_JPEG).unwrap();
        assert!(load(&lying, 0, 1.0).is_ok(), "the magic number wins");

        // An SVG is still an SVG, so the new branch cannot have swallowed one.
        assert!(is_svg(std::path::Path::new("d.svg"), b"<svg/>"));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&lying);
    }

    /// A real 1×1 baseline JPEG, tables and all.
    ///
    /// Bytes rather than a fixture file, so the suite stays text and a test can
    /// be read without opening anything else — the SVG above is inline for the
    /// same reason. It is this long because a baseline JPEG carries a
    /// quantisation table and two Huffman tables before it may carry a pixel;
    /// there is no shorter valid one worth hand-rolling.
    const ONE_PIXEL_JPEG: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x48, 0x00, 0x48, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x01, 0x00, 0x01, 0x03,
        0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, 0xFF, 0xC4, 0x00, 0x1F, 0x00, 0x00,
        0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0xFF, 0xC4, 0x00,
        0xB5, 0x10, 0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00,
        0x00, 0x01, 0x7D, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06,
        0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1,
        0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16, 0x17, 0x18,
        0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A,
        0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59,
        0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78,
        0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96,
        0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3,
        0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9,
        0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5,
        0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA,
        0xFF, 0xC4, 0x00, 0x1F, 0x01, 0x00, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
        0x09, 0x0A, 0x0B, 0xFF, 0xC4, 0x00, 0xB5, 0x11, 0x00, 0x02, 0x01, 0x02, 0x04, 0x04, 0x03,
        0x04, 0x07, 0x05, 0x04, 0x04, 0x00, 0x01, 0x02, 0x77, 0x00, 0x01, 0x02, 0x03, 0x11, 0x04,
        0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08,
        0x14, 0x42, 0x91, 0xA1, 0xB1, 0xC1, 0x09, 0x23, 0x33, 0x52, 0xF0, 0x15, 0x62, 0x72, 0xD1,
        0x0A, 0x16, 0x24, 0x34, 0xE1, 0x25, 0xF1, 0x17, 0x18, 0x19, 0x1A, 0x26, 0x27, 0x28, 0x29,
        0x2A, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A,
        0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
        0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
        0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4,
        0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA,
        0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7,
        0xD8, 0xD9, 0xDA, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF2, 0xF3, 0xF4,
        0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x02, 0x02, 0x02, 0x02,
        0x02, 0x02, 0x03, 0x02, 0x02, 0x03, 0x05, 0x03, 0x03, 0x03, 0x05, 0x06, 0x05, 0x05, 0x05,
        0x05, 0x06, 0x08, 0x06, 0x06, 0x06, 0x06, 0x06, 0x08, 0x0A, 0x08, 0x08, 0x08, 0x08, 0x08,
        0x08, 0x0A, 0x0A, 0x0A, 0x0A, 0x0A, 0x0A, 0x0A, 0x0A, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C,
        0x0E, 0x0E, 0x0E, 0x0E, 0x0E, 0x0F, 0x0F, 0x0F, 0x0F, 0x0F, 0x0F, 0x0F, 0x0F, 0x0F, 0x0F,
        0xFF, 0xDB, 0x00, 0x43, 0x01, 0x02, 0x02, 0x02, 0x04, 0x04, 0x04, 0x07, 0x04, 0x04, 0x07,
        0x10, 0x0B, 0x09, 0x0B, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10,
        0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10,
        0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10,
        0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0xFF, 0xDD, 0x00, 0x04, 0x00, 0x01,
        0xFF, 0xDA, 0x00, 0x0C, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3F, 0x00, 0xF8,
        0xEE, 0x8A, 0x28, 0xAF, 0xC4, 0xCF, 0xE8, 0xC3, 0xFF, 0xD9,
    ];

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

    /// The shared font database is shared, and sharing it changes no pixels.
    ///
    /// Two halves of one claim. That [`fonts`] hands back the same allocation
    /// twice is the whole point of the `OnceLock` and is checked by pointer
    /// rather than by clock — a timing assertion would say the same thing and
    /// would say it flakily on a machine with three fonts installed, where the
    /// scan this saves costs nothing to begin with.
    ///
    /// That the sharing is *inert* is the risk it introduced, so a second
    /// figure with different text and a different directory is rendered in
    /// between, and the first one is rendered again afterwards and must come
    /// back byte for byte. Text on purpose: a figure with no `<text>` in it
    /// never touches the database and would prove nothing.
    #[test]
    fn one_font_database_is_shared_and_leaks_nothing_between_figures() {
        let dir = std::env::temp_dir().join("zemacs_figure_fonts_test");
        std::fs::create_dir_all(&dir).unwrap();
        let label = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(
                &path,
                format!(
                    r##"<svg xmlns="http://www.w3.org/2000/svg" width="60" height="20">
                          <text x="2" y="14" font-family="serif" font-size="12"
                                fill="#000000">{text}</text>
                        </svg>"##
                ),
            )
            .unwrap();
            path
        };

        assert!(
            Arc::ptr_eq(&fonts(), &fonts()),
            "the font database was scanned twice"
        );

        let a = label("a.svg", "one");
        let first = load(&a, 0, 2.0).expect("a");
        let _other = load(&label("b.svg", "two"), 0, 1.0).expect("b");
        let again = load(&a, 0, 2.0).expect("a again");

        assert_eq!((first.width, first.height), (again.width, again.height));
        assert_eq!(first.rgba, again.rgba, "a figure changed under a sibling");
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
