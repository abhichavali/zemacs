//! Project-scoped ripgrep.
//!
//! Shelling out rather than linking a regex engine and walking the tree here:
//! ripgrep's speed is the reason anyone reaches for `consult-ripgrep`, and it
//! already knows about `.gitignore`, binary files and encodings.
//!
//! Nothing here goes near a shell — the pattern is a single `arg` after `--`,
//! so a pattern of `; rm -rf ~` or `-i` is a pattern.

use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// Hits past this are dropped. A one-character pattern matches most of a
/// repository, and a completion prompt cannot show a million lines anyway.
pub const SEARCH_LIMIT: usize = 2000;

/// Where Homebrew puts it. Only tried when `rg` is not on `PATH`, which is the
/// normal state of an editor launched from a GUI rather than a login shell.
///
/// ponytail: hard-coded, and wrong on Linux and on an Intel Mac
/// (`/usr/local/bin/rg`). The upgrade path is a configurable program name, which
/// is worth having the moment anything else needs to find a binary.
const FALLBACK: &str = "/opt/homebrew/bin/rg";

/// Lines matching `pattern` under `root`, as `path:line:text`, where `path` is
/// **relative to `root`** — that is what a user reads, and `root.join(path)` is
/// what opens it.
///
/// `pattern` is a regular expression, matched case-insensitively unless it
/// contains an uppercase letter (ripgrep's `--smart-case`). An empty pattern is
/// an empty list rather than every line in the project.
///
/// Errors when ripgrep is missing or the pattern will not compile — both are
/// things the user has to be told, and neither is distinguishable from "no
/// matches" if reported as an empty list.
pub fn search(root: &Path, pattern: &str) -> Result<Vec<String>> {
    if pattern.trim().is_empty() {
        return Ok(Vec::new());
    }

    let out = match run("rg", root, pattern) {
        Err(e) if e.kind() == ErrorKind::NotFound => run(FALLBACK, root, pattern),
        other => other,
    };
    let (hits, failure) = match out {
        Ok(out) => out,
        Err(e) if e.kind() == ErrorKind::NotFound => bail!("ripgrep (rg) is not installed"),
        Err(e) => return Err(e).context("running rg"),
    };
    if let Some(msg) = failure {
        bail!("{msg}");
    }
    Ok(hits)
}

/// At most [`SEARCH_LIMIT`] hits, and — when ripgrep failed outright rather than
/// merely finding nothing — the one line of its own complaint worth showing.
///
/// No path argument: ripgrep then searches the working directory and prints
/// paths relative to it, which is exactly the required output. Passing `.`
/// instead would prefix every hit with `./`.
///
/// Lines are taken as they arrive and the child is killed the moment the limit
/// is reached, rather than waiting for ripgrep to finish and then throwing most
/// of what it wrote away. That is not a memory nicety: the caller is
/// synchronous, holds the editor lock and re-runs this on every keystroke of
/// the prompt, so the whole editor is frozen for as long as ripgrep runs. On a
/// tree nothing ignores — 683 MB of vendored sources — a pattern of `e` was
/// 1.17 s and 152 MB buffered to show two thousand lines; stopping at the limit
/// is 8 ms. `--max-count` bounds the hits per file, this bounds the run.
fn run(program: &str, root: &Path, pattern: &str) -> std::io::Result<(Vec<String>, Option<String>)> {
    let mut child = Command::new(program)
        .current_dir(root)
        .args([
            "--line-number",
            "--no-heading",
            "--color=never",
            "--smart-case",
            // An unreadable file is one missing hit, not an error banner.
            "--no-messages",
            "--max-count=50",
        ])
        .arg("--")
        .arg(pattern)
        // Not inherited, which `spawn` would do and `output` never did: given no
        // path to search, ripgrep searches *stdin* whenever stdin is readable —
        // a pipe or a file. The editor's own stdin is whatever launched it, so
        // inheriting it means searching that instead of the project, and waiting
        // for a pipe nobody will ever write to, forever, holding the lock.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Kept rather than dropped because a pattern that will not compile is a
        // message only ripgrep can write, and read only once stdout is done. A
        // full stderr pipe cannot deadlock the child against us: `--no-messages`
        // silences the per-file complaints, which leaves the startup errors, and
        // ripgrep writes one of those and exits before it has searched a line.
        .stderr(Stdio::piped())
        .spawn()?;

    // Read bytes rather than `Lines`: ripgrep prints a matching line as it found
    // it, and a file that is text enough to search can still hold a byte no
    // `String` will take. Lossy is what the whole path did before.
    let mut out = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut buf = Vec::new();
    let mut hits = Vec::new();
    while hits.len() < SEARCH_LIMIT {
        buf.clear();
        if out.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let line = buf.strip_suffix(b"\n").unwrap_or(&buf);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        hits.push(String::from_utf8_lossy(line).into_owned());
    }

    // Hitting the limit is the normal outcome of a short pattern, so neither the
    // signal we just sent nor the broken pipe it leaves ripgrep holding may
    // reach the caller as a failure. `wait` regardless of how the child ended:
    // killing one and walking away leaves a zombie until the editor exits.
    let limited = hits.len() == SEARCH_LIMIT;
    if limited {
        let _ = child.kill();
    }
    let mut err = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_end(&mut err);
    }
    let status = child.wait()?;

    // ripgrep exits 0 with matches, 1 with none — an empty list, not a failure —
    // and 2 for a real problem, of which an uncompilable regex is the one the
    // user causes by typing. Its own message is the best one available.
    let failure = (!limited && !matches!(status.code(), Some(0) | Some(1))).then(|| {
        let msg = String::from_utf8_lossy(&err);
        msg.trim().lines().next().unwrap_or("rg failed").to_string()
    });
    Ok((hits, failure))
}
