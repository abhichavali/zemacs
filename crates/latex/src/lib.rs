//! zemacs-latex — one LaTeX fragment in, one bitmap out.
//!
//! This is `org-latex-preview`'s engine and nothing else: it knows how to turn
//! `$E = mc^2$` into pixels the renderer can upload as a texture, and it does
//! not know that org, buffers or windows exist.
//!
//! The pipeline is Emacs': **`latex` → DVI → `dvipng` → PNG → RGBA**. Not
//! `pdflatex`, because a PDF has to go through a rasterizer we would then have
//! to link (poppler, mupdf, ghostscript); DVI is a page-description format so
//! simple that `dvipng` is a 2 MB self-contained converter, it is the path every
//! Emacs user has already exercised, and its output is exactly what we want —
//! a tight-cropped, transparent-background, single-colour bitmap.
//!
//! Three things make the difference between "renders" and "usable":
//!
//! * **Transparent background, themed foreground.** The image lands on the
//!   editor's own background, so `-bg Transparent` and `-fg` the theme colour.
//!   dvipng then writes a 16-entry palette whose colour is constant and whose
//!   *alpha* carries the antialiasing, which is precisely straight-alpha RGBA
//!   once expanded.
//! * **Depth.** Text sits on a baseline; `$x_1$` hangs below it. [`Preview::depth`]
//!   is how far, in pixels, so an inline preview can be placed against a line of
//!   text instead of floating.
//! * **Cache.** A cold render is a few hundred milliseconds. Keyed on the
//!   generated document plus dpi plus colour and kept on disk, a re-preview is
//!   a file read and a PNG decode, and it survives a restart.
//!
//! ponytail: `\begin{equation}` gets its equation number typeset at the article
//! class's `\textwidth`, so a numbered display fragment renders as wide as a
//! paper's text column with the number stranded on the right. `\[…\]` and
//! `equation*` do not. Fixing it means either measuring the fragment and
//! setting `\textwidth` to it, or `\usepackage{geometry}` — neither is worth it
//! until someone complains.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{fs, io};

use anyhow::{anyhow, bail, Context, Result};

/// How long either child gets before it is killed. Generous, because the very
/// first `dvipng` run on a machine can stop to build a font cache; a warm
/// preview is well under a second.
const TIMEOUT: Duration = Duration::from_secs(20);

/// TeX's point, 1/72.27 in — *not* the 1/72 in PostScript point. `\the\dp0`
/// prints in these, and dvipng's `-D` is in real dots per inch.
const TEX_PT_PER_INCH: f64 = 72.27;

/// Marker `\typeout`ed with the fragment's depth. Anything that cannot appear
/// in TeX's own log chatter will do.
const DEPTH_TAG: &str = "ZEMACSDEPTH";

/// Every document we build. `amsmath` and `amssymb` because real math needs
/// `align`, `\text` and half the symbols people actually type; `\pagestyle
/// {empty}` because a page number is ink, and `-T tight` would crop to include
/// it and everything between.
const PREAMBLE: &str = concat!(
    "\\documentclass{article}\n",
    "\\usepackage{amsmath}\n",
    "\\usepackage{amssymb}\n",
    "\\pagestyle{empty}\n",
);

/// A rendered fragment.
#[derive(Clone, PartialEq, Eq)]
pub struct Preview {
    pub width: u32,
    pub height: u32,
    /// Rows top to bottom, `width * height * 4` bytes, `R G B A` per pixel,
    /// stride `width * 4`, **straight** (not premultiplied) alpha. The RGB is
    /// the colour passed to [`render`] in *every* pixel, transparent ones
    /// included; only the alpha varies. So it is a coverage mask that can be
    /// blended over any background, and a filtered scale cannot smear some
    /// other colour into the glyph edges.
    pub rgba: Vec<u8>,
    /// Pixels the image descends *below* the text baseline — always `>= 0`, and
    /// `0` for a fragment that sits entirely on the line (`$E = mc^2$`). To
    /// place the image against a line of text, put its bottom edge `depth`
    /// pixels below that line's baseline.
    pub depth: u32,
}

impl std::fmt::Debug for Preview {
    /// Without this, one `{:?}` of a failed assertion prints a megabyte of
    /// pixel values into the test output.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Preview")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("depth", &self.depth)
            .field("rgba", &format_args!("{} bytes", self.rgba.len()))
            .finish()
    }
}

/// Render one fragment, delimiters and all — `$x^2$`, `\[…\]`,
/// `\begin{align}…\end{align}` are all pasted into the document body verbatim,
/// because each is already valid LaTeX there.
///
/// `dpi` is dvipng's resolution (see [`dpi_for_em`]) and `color` is the RGB the
/// glyphs are drawn in. Hits the disk cache first; a cold render shells out.
///
/// Errors are for the status line: a missing toolchain, a TeX syntax error (the
/// offending `!` line from the log), a timeout. Never panics.
pub fn render(source: &str, dpi: u32, color: (u8, u8, u8)) -> Result<Preview> {
    if source.trim().is_empty() {
        // LaTeX is perfectly happy to typeset nothing, and dvipng crops it to a
        // 1x1 transparent dot. Better to say so than to hand back a ghost.
        bail!("nothing to preview");
    }
    let doc = document(source);
    let cached = cache_path(&doc, dpi, color);
    if let Some(preview) = cached.as_deref().and_then(|p| load(p, color)) {
        return Ok(preview);
    }
    let (png, depth_pt) = run_tex(&doc, dpi, color)?;
    let depth = (depth_pt * f64::from(dpi) / TEX_PT_PER_INCH).round().max(0.0) as u32;
    // Store before decoding: a cache we failed to write is not an error, and a
    // decode failure is one we want reported rather than silently re-run.
    if let Some(path) = cached {
        store(&path, depth, &png);
    }
    decode(&png, depth, color)
}

/// Are both binaries there? For a caller that would rather say "install TeX
/// Live" up front than let the first preview fail.
pub fn available() -> bool {
    ["latex", "dvipng"].iter().all(|name| {
        Command::new(program(name))
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    })
}

/// The dvipng resolution that makes a preview match text drawn `px` pixels to
/// the em. `article`'s body font is 10pt, so one em of the document is 10 TeX
/// points and `px` pixels have to cover them.
pub fn dpi_for_em(px: f32) -> u32 {
    (f64::from(px) / 10.0 * TEX_PT_PER_INCH).round().clamp(30.0, 1200.0) as u32
}

// --- the document ---------------------------------------------------------

/// Is this display math, i.e. does it belong on a line of its own? Display math
/// cannot go in an `\hbox`, which is how the inline case measures its depth.
fn is_display(source: &str) -> bool {
    let s = source.trim_start();
    s.starts_with("$$") || s.starts_with("\\[") || s.starts_with("\\begin{")
}

/// Wrap a fragment in a document.
///
/// The inline case is boxed so TeX can be *asked* for the depth: `\the\dp0` in
/// the log is exact and needs nothing installed. dvipng's own `--depth` would
/// be the obvious answer and is what Emacs' preview-latex uses, but it only
/// means anything when `preview.sty` is loaded — without it dvipng measures
/// from the page origin and reports a constant ~170px offset. `preview.sty` is
/// not in BasicTeX, so requiring it would make previews work on some TeX
/// installs and not others.
fn document(source: &str) -> String {
    let body = if is_display(source) {
        // Display math is on its own line; there is no surrounding text for a
        // baseline to line up with, so the depth is not interesting.
        format!("\\noindent {source}\n")
    } else {
        format!(
            "\\setbox0=\\hbox{{{source}}}\n\\typeout{{{DEPTH_TAG} \\the\\dp0}}\n\\noindent\\box0\n"
        )
    };
    format!("{PREAMBLE}\\begin{{document}}\n{body}\\end{{document}}\n")
}

// --- shelling out ---------------------------------------------------------

/// Run the two children in a scratch directory, returning the PNG bytes and the
/// fragment's depth in TeX points.
fn run_tex(doc: &str, dpi: u32, color: (u8, u8, u8)) -> Result<(Vec<u8>, f64)> {
    let dir = TempDir::new()?;
    let tex = dir.0.join("f.tex");
    fs::write(&tex, doc).context("writing the LaTeX job")?;

    // `-halt-on-error` and `nonstopmode` together are what stop TeX sitting at
    // its `?` prompt forever on a bad fragment; a null stdin means even a
    // prompt we did not anticipate reads EOF and gives up. `-no-shell-escape`
    // because the fragment comes out of a buffer, and `\write18` in an org file
    // someone mailed you should not run commands.
    let ok = run(
        &program("latex"),
        &[
            "-interaction=nonstopmode",
            "-halt-on-error",
            "-no-shell-escape",
            "f.tex",
        ],
        &dir.0,
    )?;
    let log = fs::read_to_string(dir.0.join("f.log")).unwrap_or_default();
    if !ok {
        bail!("{}", tex_error(&log));
    }

    let fg = format!(
        "rgb {:.3} {:.3} {:.3}",
        f64::from(color.0) / 255.0,
        f64::from(color.1) / 255.0,
        f64::from(color.2) / 255.0
    );
    // `-T tight` crops to the ink, which is what makes the result a fragment
    // rather than a page with a fragment in the corner.
    let ok = run(
        &program("dvipng"),
        &[
            "-q",
            "-D",
            &dpi.to_string(),
            "-T",
            "tight",
            "-bg",
            "Transparent",
            "-fg",
            &fg,
            "-o",
            "f.png",
            "f.dvi",
        ],
        &dir.0,
    )?;
    if !ok {
        bail!("dvipng could not convert the fragment");
    }
    let png = fs::read(dir.0.join("f.png")).context("dvipng wrote no image")?;
    Ok((png, depth_pt(&log)))
}

/// Spawn `program`, wait for it, kill it if it overruns. `Ok(false)` means it
/// ran and failed, which is the caller's cue to go read the log.
///
/// Both children are silenced rather than piped: nothing we need is on stdout
/// (the depth and the errors are both in the `.log`), and a pipe nobody drains
/// is a deadlock waiting for a verbose enough error message.
fn run(program: &Path, args: &[&str], cwd: &Path) -> Result<bool> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| match e.kind() {
            ErrorKind::NotFound => anyhow!(
                "{} not found — install TeX Live, or put it on PATH",
                program.display()
            ),
            _ => anyhow!("cannot run {}: {e}", program.display()),
        })?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait().context("waiting for TeX")? {
            Some(status) => return Ok(status.success()),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait(); // reap, so we leave no zombie behind
                bail!(
                    "{} did not finish in {}s",
                    program.display(),
                    TIMEOUT.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// Where a TeX binary actually is. A macOS app launched from Finder inherits a
/// PATH without `/Library/TeX/texbin`, which is the only place MacTeX puts its
/// binaries — so previews would work from a terminal and mysteriously not from
/// the Dock.
fn program(name: &str) -> PathBuf {
    let mactex = Path::new("/Library/TeX/texbin").join(name);
    if mactex.is_file() {
        mactex
    } else {
        PathBuf::from(name)
    }
}

/// The first real complaint in a TeX log. TeX writes errors as a line starting
/// `!`, followed by context; the first one is the one that mattered, since
/// `-halt-on-error` stopped there.
fn tex_error(log: &str) -> String {
    log.lines()
        .find(|l| l.starts_with('!'))
        .map(|l| l.trim_start_matches("! ").trim_end().to_string())
        .unwrap_or_else(|| "LaTeX failed to typeset the fragment".into())
}

/// The depth `\typeout`ed into the log, in TeX points. Display fragments never
/// emit one, and a truncated log is not worth failing over — both give 0.
fn depth_pt(log: &str) -> f64 {
    log.lines()
        .find_map(|l| l.strip_prefix(DEPTH_TAG))
        .and_then(|rest| rest.trim().strip_suffix("pt"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0.0)
}

/// A scratch directory that deletes itself. TeX scatters `.aux`, `.log` and
/// `.dvi` next to its input, so it gets a directory rather than a file, and the
/// [`Drop`] is what keeps a failed or panicking render from leaving it behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<TempDir> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("zemacs-latex-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).context("creating a scratch directory")?;
        Ok(TempDir(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// --- PNG ------------------------------------------------------------------

/// Expand dvipng's output to straight-alpha RGBA.
///
/// dvipng writes a 4-bit palette PNG plus a `tRNS` chunk: one colour, sixteen
/// alphas. `EXPAND` turns the palette into RGB and `tRNS` into an alpha
/// channel, `ALPHA` covers the case where dvipng ever decides not to write a
/// `tRNS`, and `STRIP_16` guarantees 8 bits a sample.
///
/// The RGB is then overwritten with `color` throughout. dvipng leaves the fully
/// transparent palette entry *white* — invisible under normal blending, but a
/// white halo the moment anything filters or premultiplies the texture.
fn decode(bytes: &[u8], depth: u32, color: (u8, u8, u8)) -> Result<Preview> {
    use png::{ColorType, Transformations};

    let mut decoder = png::Decoder::new(io::Cursor::new(bytes));
    decoder.set_transformations(
        Transformations::EXPAND | Transformations::ALPHA | Transformations::STRIP_16,
    );
    let mut reader = decoder.read_info().context("dvipng wrote an unreadable PNG")?;
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf).context("truncated PNG")?;
    buf.truncate(info.buffer_size());

    let mut rgba: Vec<u8> = match info.color_type {
        ColorType::Rgba => buf,
        ColorType::Rgb => buf.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        ColorType::GrayscaleAlpha => {
            buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect()
        }
        ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        ColorType::Indexed => bail!("PNG palette was not expanded"),
    };
    for px in rgba.chunks_exact_mut(4) {
        px[..3].copy_from_slice(&[color.0, color.1, color.2]);
    }
    let (width, height) = (info.width, info.height);
    if rgba.len() as u64 != u64::from(width) * u64::from(height) * 4 {
        bail!("PNG is {width}x{height} but decoded to {} bytes", rgba.len());
    }
    Ok(Preview {
        width,
        height,
        rgba,
        depth,
    })
}

// --- cache ----------------------------------------------------------------

/// Where previews live between runs: `~/Library/Caches/zemacs/latex` on macOS,
/// `$XDG_CACHE_HOME/zemacs/latex` (or `~/.cache/...`) elsewhere. Regenerable,
/// so a cache directory and not a data one — deleting it costs a re-render.
pub fn cache_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg).join("zemacs").join("latex");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    if cfg!(target_os = "macos") {
        home.join("Library/Caches/zemacs/latex")
    } else {
        home.join(".cache/zemacs/latex")
    }
}

/// The cache file for a job, creating the directory on the way. `None` when the
/// directory cannot be made — then every render is a cold one, which is slow
/// but correct.
///
/// Hashing the *generated document* rather than the fragment means a change to
/// [`PREAMBLE`] invalidates every entry for free.
///
/// ponytail: `DefaultHasher` is SipHash-1-3 with a fixed key. It is not
/// promised to be stable across Rust releases, so a toolchain upgrade costs one
/// cold cache; and a 64-bit collision would show the wrong preview, which needs
/// ~4 billion distinct fragments to become likely. A real digest is one
/// dependency away if either ever matters.
fn cache_path(doc: &str, dpi: u32, color: (u8, u8, u8)) -> Option<PathBuf> {
    let dir = cache_dir();
    fs::create_dir_all(&dir).ok()?;
    let mut hasher = DefaultHasher::new();
    (doc, dpi, color).hash(&mut hasher);
    Some(dir.join(format!("{:016x}.preview", hasher.finish())))
}

/// An entry is the depth in decimal, a newline, then dvipng's PNG verbatim —
/// one file, one read, and still a PNG you can look at with `tail -c +N`.
///
/// Anything unexpected in it is a miss, never an error: a cache is a place a
/// half-written file or a leftover from an older format is normal.
fn load(path: &Path, color: (u8, u8, u8)) -> Option<Preview> {
    let bytes = fs::read(path).ok()?;
    let split = bytes.iter().position(|&b| b == b'\n')?;
    let depth = std::str::from_utf8(&bytes[..split]).ok()?.parse().ok()?;
    decode(&bytes[split + 1..], depth, color).ok()
}

/// Write through a temporary name and rename, so a reader never sees half a
/// file — two windows previewing the same fragment at once is normal.
fn store(path: &Path, depth: u32, png: &[u8]) {
    let mut entry = format!("{depth}\n").into_bytes();
    entry.extend_from_slice(png);
    let tmp = path.with_extension("part");
    if fs::write(&tmp, &entry).is_ok() && fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

// --- background rendering -------------------------------------------------

/// A rendering thread.
///
/// Same shape as `zemacs_syntax::Worker`: [`Worker::request`] hands over a job
/// and returns immediately, [`Worker::poll`] picks up what has finished. A
/// LaTeX run is hundreds of milliseconds and the editor must not spend them
/// waiting.
///
/// Unlike the syntax worker this queue does **not** coalesce. There, a burst of
/// requests is the same buffer typed at repeatedly and only the newest matters;
/// here each request is a different fragment, and dropping the older ones would
/// mean previewing one equation out of a screenful.
pub struct Worker {
    requests: crossbeam_channel::Sender<Job>,
    results: crossbeam_channel::Receiver<(u64, Result<Preview, String>)>,
}

struct Job {
    id: u64,
    source: String,
    dpi: u32,
    color: (u8, u8, u8),
}

/// Spawn the rendering thread. It exits when the [`Worker`] is dropped.
pub fn spawn_worker() -> Worker {
    let (req_tx, req_rx) = crossbeam_channel::unbounded::<Job>();
    let (res_tx, res_rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("zemacs-latex".into())
        .spawn(move || {
            while let Ok(job) = req_rx.recv() {
                // `{:#}` flattens anyhow's context chain into one line, which is
                // the shape a status line wants.
                let result = render(&job.source, job.dpi, job.color).map_err(|e| format!("{e:#}"));
                if res_tx.send((job.id, result)).is_err() {
                    break;
                }
            }
        })
        .expect("failed to spawn latex thread");
    Worker {
        requests: req_tx,
        results: res_rx,
    }
}

impl Worker {
    /// Queue a fragment. `id` is the caller's — a fragment's char offset, say —
    /// and comes back untouched with the result. Never blocks.
    pub fn request(&self, id: u64, source: &str, dpi: u32, color: (u8, u8, u8)) {
        let _ = self.requests.send(Job {
            id,
            source: source.to_string(),
            dpi,
            color,
        });
    }

    /// One finished job, or `None`. Call it in a `while let` — several can
    /// finish between two frames and each one is a different fragment.
    pub fn poll(&self) -> Option<(u64, Result<Preview, String>)> {
        self.results.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FG: (u8, u8, u8) = (220, 220, 220);

    /// Every test that shells out is guarded on this: a machine without TeX
    /// should report "nothing to run here", not a wall of failures.
    fn toolchain() -> bool {
        static ONCE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ONCE.get_or_init(available)
    }

    /// A dpi nothing else will have cached, so a cache test starts cold.
    fn unique_dpi() -> u32 {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed) as u32;
        // 900+ dpi renders are still fast and are nobody's real setting.
        900 + (std::process::id() % 97) * 3 + n
    }

    #[test]
    fn a_fragment_renders_to_rgba_of_plausible_size() {
        if !toolchain() {
            return;
        }
        let p = render("$E = mc^2$", 200, FG).expect("render failed");
        assert!(p.width > 20 && p.width < 2000, "{p:?}");
        assert!(p.height > 5 && p.height < 500, "{p:?}");
        assert_eq!(p.rgba.len(), (p.width * p.height * 4) as usize);
        // Transparent background, so most of the image is alpha 0 and the ink
        // is the colour we asked for. A blank or black image fails both.
        assert!(p.rgba.chunks_exact(4).any(|px| px[3] > 200), "no ink: {p:?}");
        assert!(p.rgba.chunks_exact(4).any(|px| px[3] == 0), "opaque: {p:?}");
        // Every pixel, transparent ones included — a stray white would haloe.
        for px in p.rgba.chunks_exact(4) {
            assert_eq!((px[0], px[1], px[2]), FG, "not the requested colour");
        }
    }

    /// The whole point of returning a depth: `$x_1$` hangs below its baseline
    /// and `$xx$` does not.
    #[test]
    fn a_subscript_descends_below_the_baseline_and_plain_text_does_not() {
        if !toolchain() {
            return;
        }
        assert_eq!(render("$xx$", 200, FG).unwrap().depth, 0);
        assert!(render("$x_1$", 200, FG).unwrap().depth > 0);
    }

    #[test]
    fn display_math_renders_in_every_delimiter_org_uses() {
        if !toolchain() {
            return;
        }
        for src in [
            "$$x + y$$",
            "\\[x + y\\]",
            "\\begin{align}a &= b\\\\c &= d\\end{align}",
        ] {
            let p = render(src, 150, FG).unwrap_or_else(|e| panic!("{src}: {e:#}"));
            assert!(p.width > 0 && p.height > 0, "{src}: {p:?}");
        }
    }

    #[test]
    fn the_second_render_of_a_fragment_comes_from_the_cache() {
        if !toolchain() {
            return;
        }
        let (src, dpi) = ("$1 + 1 = 2$", unique_dpi());
        let path = cache_path(&document(src), dpi, FG).expect("no cache directory");
        let _ = fs::remove_file(&path);

        let cold = render(src, dpi, FG).expect("cold render failed");
        assert!(path.is_file(), "nothing was cached at {}", path.display());

        let start = Instant::now();
        let warm = render(src, dpi, FG).expect("warm render failed");
        assert_eq!(cold, warm, "the cache returned different pixels");
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "a cache hit took {:?} — it shelled out again",
            start.elapsed()
        );
        let _ = fs::remove_file(&path);
    }

    /// Truncated, empty and outright wrong cache entries all have to read as a
    /// miss; an editor that cannot start because of a bad cache file is worse
    /// than one that re-renders.
    #[test]
    fn a_corrupt_cache_entry_is_a_miss_not_a_failure() {
        let dir = TempDir::new().unwrap();
        let path = dir.0.join("junk.preview");
        for junk in [&b""[..], b"\n", b"12\nnot a png", b"no newline at all", b"9\n\x89PNG"] {
            fs::write(&path, junk).unwrap();
            assert!(load(&path, FG).is_none(), "{junk:?} was accepted");
        }
        assert!(load(&dir.0.join("absent.preview"), FG).is_none());
    }

    #[test]
    fn a_malformed_fragment_reports_the_latex_error_instead_of_hanging() {
        if !toolchain() {
            return;
        }
        let start = Instant::now();
        let err = render("$\\thisIsNotACommand$", 150, FG).unwrap_err();
        // TeX would sit at its `?` prompt forever without nonstopmode.
        assert!(start.elapsed() < TIMEOUT, "it hung");
        let msg = format!("{err:#}");
        assert!(msg.contains("Undefined control sequence"), "unhelpful: {msg}");
    }

    /// `\end{document}` with no `\begin` is the other flavour of broken: TeX
    /// runs to completion but produces nothing to convert.
    #[test]
    fn an_empty_fragment_fails_cleanly() {
        if !toolchain() {
            return;
        }
        assert!(render("", 150, FG).is_err());
    }

    #[test]
    fn a_missing_binary_is_reported_rather_than_panicking() {
        let dir = TempDir::new().unwrap();
        let err = run(Path::new("zemacs-no-such-binary"), &[], &dir.0).unwrap_err();
        assert!(format!("{err:#}").contains("not found"), "{err:#}");
    }

    /// TeX's `.aux`/`.log`/`.dvi` litter is confined to a scratch directory
    /// whose [`Drop`] deletes it, which is what makes the error and panic paths
    /// clean up too — there is no other exit from [`run_tex`].
    #[test]
    fn a_scratch_directory_deletes_itself() {
        let path = {
            let dir = TempDir::new().unwrap();
            fs::write(dir.0.join("f.log"), "litter").unwrap();
            assert!(dir.0.is_dir());
            dir.0.clone()
        };
        assert!(!path.exists(), "{} survived", path.display());
    }

    #[test]
    fn the_worker_returns_every_job_it_was_given() {
        if !toolchain() {
            return;
        }
        let worker = spawn_worker();
        for (id, src) in ["$a$", "$b$", "$\\notacommand$"].iter().enumerate() {
            worker.request(id as u64, src, 150, FG);
        }
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        while seen.len() < 3 && Instant::now() < deadline {
            while let Some((id, result)) = worker.poll() {
                seen.push((id, result.is_ok()));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        seen.sort();
        assert_eq!(seen, [(0, true), (1, true), (2, false)]);
    }

    #[test]
    fn display_math_is_told_apart_from_inline() {
        for src in ["$$x$$", "\\[x\\]", "\\begin{equation}x\\end{equation}", "  $$x$$"] {
            assert!(is_display(src), "{src}");
        }
        for src in ["$x$", "\\(x\\)", "$x$$y$"] {
            assert!(!is_display(src), "{src}");
        }
    }

    #[test]
    fn a_depth_is_read_out_of_the_log_and_a_missing_one_is_zero() {
        assert_eq!(depth_pt("blah\nZEMACSDEPTH 1.49998pt\nblah"), 1.49998);
        assert_eq!(depth_pt("ZEMACSDEPTH 0.0pt"), 0.0);
        assert_eq!(depth_pt("no depth here"), 0.0);
        assert_eq!(depth_pt("ZEMACSDEPTH mangled"), 0.0);
    }

    #[test]
    fn the_first_bang_line_of_a_log_is_the_error() {
        let log = "This is pdfTeX\n! Undefined control sequence.\nl.6 \\foo\n! Emergency stop.";
        assert_eq!(tex_error(log), "Undefined control sequence.");
        assert!(tex_error("nothing wrong here").contains("failed"));
    }

    #[test]
    fn dpi_scales_with_the_font_and_stays_sane() {
        assert!(dpi_for_em(16.0) > dpi_for_em(12.0));
        assert!((dpi_for_em(16.0) as i64 - 116).abs() < 3, "{}", dpi_for_em(16.0));
        assert!(dpi_for_em(0.0) >= 30 && dpi_for_em(1e9) <= 1200);
    }
}
