//! Project-scoped ripgrep.
//!
//! Shelling out rather than linking a regex engine and walking the tree here:
//! ripgrep's speed is the reason anyone reaches for `consult-ripgrep`, and it
//! already knows about `.gitignore`, binary files and encodings.
//!
//! Nothing here goes near a shell — the pattern is a single `arg` after `--`,
//! so a pattern of `; rm -rf ~` or `-i` is a pattern.

use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

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
    let out = match out {
        Ok(out) => out,
        Err(e) if e.kind() == ErrorKind::NotFound => bail!("ripgrep (rg) is not installed"),
        Err(e) => return Err(e).context("running rg"),
    };

    // ripgrep exits 0 with matches, 1 with none — an empty list, not a failure —
    // and 2 for a real problem, of which an uncompilable regex is the one the
    // user causes by typing. Its own message is the best one available.
    if !matches!(out.status.code(), Some(0) | Some(1)) {
        let msg = String::from_utf8_lossy(&out.stderr);
        bail!("{}", msg.trim().lines().next().unwrap_or("rg failed"));
    }

    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .take(SEARCH_LIMIT)
        .map(str::to_string)
        .collect())
}

/// No path argument: ripgrep then searches the working directory and prints
/// paths relative to it, which is exactly the required output. Passing `.`
/// instead would prefix every hit with `./`.
///
/// ponytail: `output()` buffers everything ripgrep prints before a single line
/// is taken, so a pattern matching a million lines costs the memory even though
/// only [`SEARCH_LIMIT`] survive. `--max-count` bounds the damage per file. The
/// upgrade path is reading stdout line by line and killing the child at the
/// limit, which also gets the results on screen sooner.
fn run(program: &str, root: &Path, pattern: &str) -> std::io::Result<Output> {
    Command::new(program)
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
        .output()
}
