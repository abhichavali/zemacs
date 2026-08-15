//! Diffs, split at their `@@` headers, and staging exactly one of them.
//!
//! Staging a single hunk is the verb Magit exists for, and git has no porcelain
//! that does it: `git add` takes paths. The trick `git add -p` uses internally
//! is to hand a one-hunk patch back to `git apply --cached` on stdin, and that
//! is what happens here — which is why a [`Hunk`] keeps git's exact bytes
//! rather than a parsed model of the change. The text the status buffer draws
//! *is* the patch that gets applied, so there is no way for the two to disagree.
//!
//! Reversing the same patch is the other three verbs: `--cached -R` unstages a
//! hunk, plain `-R` throws it away.
//!
//! Staging *part* of a hunk is the same trick one level down, and the one place
//! here where git's bytes are not enough: a patch that carries only some of a
//! hunk's changes has to be written from scratch — see [`rewrite`], which is
//! also where the difference between staging a region and unstaging one lives.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::{run_with, Section};

/// One file's diff on one side of the index, cut into hunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Path relative to the repository root, matching [`crate::FileChange`].
    pub path: PathBuf,
    /// Which diff this is: [`Section::Unstaged`] is the working tree against
    /// the index, [`Section::Staged`] the index against HEAD. No other section
    /// has a diff.
    pub section: Section,
    /// The `diff --git`/`index`/`---`/`+++` lines. Not interesting to look at,
    /// but every hunk needs it in front to be a patch git will apply.
    pub preamble: String,
    /// Empty for a binary file, which has a preamble and nothing to show.
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// The `index`th hunk as a standalone patch, or `None` if there is no such
    /// hunk. Byte-for-byte git's own output, so applying it cannot fail on
    /// whitespace we invented.
    pub fn patch(&self, index: usize) -> Option<String> {
        let hunk = self.hunks.get(index)?;
        Some(format!("{}{}", self.preamble, hunk.body))
    }

    /// The same, carrying only *some* of that hunk's changed lines: `lines`
    /// numbers the hunk's body the way [`crate::Line::Hunk`] does, so 0 is the
    /// `@@` header and never a change.
    ///
    /// `reverse` is whether the patch is bound for `git apply -R`, which decides
    /// what happens to the lines nobody picked — see [`rewrite`].
    pub fn partial(&self, index: usize, lines: &[usize], reverse: bool) -> Result<String> {
        let Some(hunk) = self.hunks.get(index) else {
            bail!("no hunk {index} in {}", self.path.display());
        };
        Ok(format!(
            "{}{}",
            self.preamble,
            rewrite(hunk, lines, reverse)?
        ))
    }
}

/// One `@@` hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The `@@ -a,b +c,d @@ ...` line without its newline — the fold line the
    /// status buffer shows above the body.
    pub header: String,
    /// Header line included, newline-terminated: a patch body as git wrote it.
    pub body: String,
}

/// Read `path`'s diff on one side of the index and split it into hunks.
///
/// Errors for any section other than staged or unstaged: an untracked file has
/// no diff, and a stash or a commit is not a working-tree change.
pub fn file_diff(repo: &Path, path: &Path, section: Section) -> Result<FileDiff> {
    let staged = match section {
        Section::Staged => true,
        Section::Unstaged => false,
        other => bail!("{other:?} has no per-file diff"),
    };
    let (preamble, hunks) = split(&crate::diff(repo, path, staged)?);
    Ok(FileDiff {
        path: path.to_path_buf(),
        section,
        preamble,
        hunks,
    })
}

/// Add exactly one hunk of a working-tree diff to the index, leaving the rest of
/// the file unstaged.
pub fn stage_hunk(repo: &Path, diff: &FileDiff, index: usize) -> Result<()> {
    apply(repo, diff, index, Section::Unstaged, &["--cached"])
}

/// Take exactly one hunk of a staged diff back out of the index. The working
/// tree keeps the change either way.
pub fn unstage_hunk(repo: &Path, diff: &FileDiff, index: usize) -> Result<()> {
    apply(repo, diff, index, Section::Staged, &["--cached", "-R"])
}

/// **Destroys work.** Reverses one hunk of a working-tree diff *in the working
/// tree*: those added lines are deleted from the file and those removed lines
/// come back, with no copy kept anywhere — the change was never committed, never
/// staged, and is not recoverable through git afterwards. Stash instead if there
/// is any doubt.
pub fn discard_hunk(repo: &Path, diff: &FileDiff, index: usize) -> Result<()> {
    apply(repo, diff, index, Section::Unstaged, &["-R"])
}

/// Add only some of one hunk's changed lines to the index — Magit's `s` over a
/// region, or on the single line under the cursor.
pub fn stage_lines(repo: &Path, diff: &FileDiff, index: usize, lines: &[usize]) -> Result<()> {
    apply_lines(repo, diff, index, Section::Unstaged, &["--cached"], lines)
}

/// Take only some of one staged hunk's lines back out of the index.
pub fn unstage_lines(repo: &Path, diff: &FileDiff, index: usize, lines: &[usize]) -> Result<()> {
    apply_lines(
        repo,
        diff,
        index,
        Section::Staged,
        &["--cached", "-R"],
        lines,
    )
}

/// **Destroys work.** [`discard_hunk`] for part of a hunk: exactly those added
/// lines are deleted from the file in the working tree and exactly those removed
/// lines come back, with no copy kept anywhere.
pub fn discard_lines(repo: &Path, diff: &FileDiff, index: usize, lines: &[usize]) -> Result<()> {
    apply_lines(repo, diff, index, Section::Unstaged, &["-R"], lines)
}

fn apply(
    repo: &Path,
    diff: &FileDiff,
    index: usize,
    wanted: Section,
    flags: &[&str],
) -> Result<()> {
    expect(diff, wanted)?;
    let Some(patch) = diff.patch(index) else {
        bail!("no hunk {index} in {}", diff.path.display());
    };
    send(repo, &patch, flags)
}

fn apply_lines(
    repo: &Path,
    diff: &FileDiff,
    index: usize,
    wanted: Section,
    flags: &[&str],
    lines: &[usize],
) -> Result<()> {
    expect(diff, wanted)?;
    // `-R` in the flags *is* the question the rewriter has to answer — which
    // side of the patch git will read as the pre-image — so it is read off them
    // rather than passed a second time and left free to disagree with them.
    send(repo, &diff.partial(index, lines, flags.contains(&"-R"))?, flags)
}

/// Applying a staged diff to the working tree, or a working-tree diff to the
/// index, would half-succeed and leave a mess: refuse rather than guess.
fn expect(diff: &FileDiff, wanted: Section) -> Result<()> {
    if diff.section != wanted {
        bail!("expected a {wanted:?} diff, got {:?}", diff.section);
    }
    Ok(())
}

fn send(repo: &Path, patch: &str, flags: &[&str]) -> Result<()> {
    let mut args = vec!["apply".to_string()];
    args.extend(flags.iter().map(|f| f.to_string()));
    // A tab-versus-space quibble must not stop a hunk the user can see from
    // being staged; `-` is the patch on stdin, so no temporary file exists to
    // leak or to race with.
    args.push("--whitespace=nowarn".into());
    args.push("-".into());
    run_with(repo, args, &[], Some(patch))?;
    Ok(())
}

/// One hunk rewritten to carry only the `+`/`-` lines named by `selected`, with
/// its `@@` counts recomputed to match what the body now holds. The header line
/// is included and everything is newline-terminated, exactly like [`Hunk::body`],
/// so the result drops straight in behind a preamble.
///
/// **`reverse` is the whole of the difference between staging a region and
/// unstaging one, and it is the one thing here that cannot be seen when it is
/// wrong**: either way the patch applies cleanly and git says nothing, and the
/// wrong one quietly moves the *complement* of what was asked for.
///
/// Applied forwards, git reads the patch's **old** side as the pre-image. A `+`
/// nobody picked is not in that side and must be **dropped**; a `-` nobody
/// picked *is* in it and has to survive into the result, which is to say become
/// **context**. That is staging.
///
/// Applied with `-R`, git reads the **new** side as the pre-image, and the two
/// swap: an unpicked `+` is already in the file and becomes **context**, an
/// unpicked `-` is not in it and is **dropped**. Unstaging and discarding are
/// both `-R`, so both take that half.
///
/// Context lines are on both sides and are never touched. A `\ No newline at end
/// of file` describes the line above it, so it lives and dies with that line.
///
/// ponytail: the preamble is passed through as git wrote it, so part of a file
/// being *created* or *deleted* cannot be picked at — the surviving side
/// contradicts the `/dev/null` above it and git refuses with "new file … depends
/// on old contents". That refusal is the acceptable half of not handling it: the
/// index is untouched and the user is told. The upgrade is rewriting the
/// preamble to an ordinary modify (`--- a/p`, `+++ b/p`, no `new file mode`)
/// whenever the rewritten hunk leaves the empty side non-empty.
fn rewrite(hunk: &Hunk, selected: &[usize], reverse: bool) -> Result<String> {
    let context = if reverse { b'+' } else { b'-' };
    let (mut old, mut new, mut picked) = (0usize, 0usize, 0usize);
    // Whether the line a `\` would be talking about has just been dropped.
    let mut gone = false;
    let mut body = String::new();

    // `split_inclusive`, not `lines`: a file with CRLF endings carries the `\r`
    // as content, and `lines` would eat it off every line the patch touches.
    // `skip(1)` steps over the `@@` header, which is rebuilt at the end.
    for (at, line) in hunk.body.split_inclusive('\n').enumerate().skip(1) {
        match line.as_bytes().first().copied() {
            Some(b'\\') => {
                if !gone {
                    body.push_str(line);
                }
            }
            Some(mark @ (b'+' | b'-')) => {
                gone = false;
                if selected.contains(&at) {
                    body.push_str(line);
                    picked += 1;
                    if mark == b'-' {
                        old += 1;
                    } else {
                        new += 1;
                    }
                } else if mark == context {
                    // Only the marker changes; the rest of the line stays the
                    // bytes git wrote, trailing `\r` and all.
                    body.push(' ');
                    body.push_str(&line[1..]);
                    old += 1;
                    new += 1;
                } else {
                    gone = true;
                }
            }
            // A context line, and the bare newline some tools write instead of
            // a lone space. On both sides, and never dropped.
            _ => {
                gone = false;
                body.push_str(line);
                old += 1;
                new += 1;
            }
        }
    }
    // A patch of nothing but context applies cleanly and does nothing at all,
    // which is the one outcome the user could mistake for success.
    if picked == 0 {
        bail!("no added or removed line in the selection");
    }

    let (a, b, c, d, tail) = header(&hunk.header)?;
    Ok(format!(
        "@@ -{},{old} +{},{new} @@{tail}\n{body}",
        start(a, b, old),
        start(c, d, new),
    ))
}

/// `@@ -a,b +c,d @@ tail` in pieces. git leaves the count out when it is one,
/// which is the only shape of this line that is not fixed.
fn header(line: &str) -> Result<(usize, usize, usize, usize, &str)> {
    let named = || format!("not a hunk header: {line:?}");
    // The ranges cannot hold a ` @@`, so the first one is always the real one
    // even when the hunk's trailing context happens to contain it.
    let (ranges, tail) = line
        .strip_prefix("@@ ")
        .and_then(|rest| rest.split_once(" @@"))
        .with_context(named)?;
    let (old, new) = ranges.split_once(' ').with_context(named)?;
    let range = |text: &str, sign: char| -> Result<(usize, usize)> {
        let text = text.strip_prefix(sign).with_context(named)?;
        Ok(match text.split_once(',') {
            Some((at, len)) => (at.parse()?, len.parse()?),
            None => (text.parse()?, 1),
        })
    };
    let (a, b) = range(old, '-')?;
    let (c, d) = range(new, '+')?;
    Ok((a, b, c, d, tail))
}

/// Where a rewritten range begins.
///
/// A unified diff numbers an *empty* range by the line before it — `-0,0` is
/// "before the first line", `-7,0` is "after line 7" — so a range that empties
/// has to step back one and a range that fills has to step forward one. Without
/// this, unstaging one line of a newly added file writes `@@ -0,2 …`, which
/// names a line 0 that no file has.
fn start(at: usize, was: usize, now: usize) -> usize {
    let first = if was == 0 { at + 1 } else { at };
    if now == 0 {
        first.saturating_sub(1)
    } else {
        first
    }
}

/// Cut a single file's unified diff into its preamble and its hunks.
///
/// A body line always begins with a space, `+`, `-` or `\`, so a line beginning
/// with `@@` in column zero is always a hunk header and never content — `+@@`
/// is what content would look like. `split_inclusive` keeps the newlines, which
/// is what makes reassembly byte-exact.
fn split(text: &str) -> (String, Vec<Hunk>) {
    let mut preamble = String::new();
    let mut hunks: Vec<Hunk> = Vec::new();
    for line in text.split_inclusive('\n') {
        if line.starts_with("@@") {
            hunks.push(Hunk {
                header: line.trim_end_matches(['\n', '\r']).to_string(),
                body: line.to_string(),
            });
        } else if let Some(last) = hunks.last_mut() {
            last.body.push_str(line);
        } else {
            preamble.push_str(line);
        }
    }
    (preamble, hunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concatenating the preamble with every hunk has to give the input back,
    /// or a patch built from a subset of the hunks would not be git's bytes.
    #[test]
    fn splitting_a_diff_and_rejoining_it_is_the_identity() {
        let text = "diff --git a/a.txt b/a.txt\nindex 1..2 100644\n--- a/a.txt\n+++ b/a.txt\n\
@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n\
@@ -10,3 +10,3 @@\n ten\n-eleven\n+ELEVEN\n twelve\n\\ No newline at end of file\n";
        let (preamble, hunks) = split(text);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].header, "@@ -1,3 +1,3 @@");
        // The trailing marker belongs to the hunk above it, not to a third one.
        assert!(hunks[1].body.ends_with("\\ No newline at end of file\n"));
        let rejoined: String =
            preamble.clone() + &hunks.iter().map(|h| h.body.clone()).collect::<String>();
        assert_eq!(rejoined, text);

        let diff = FileDiff {
            path: "a.txt".into(),
            section: Section::Unstaged,
            preamble,
            hunks,
        };
        assert!(diff.patch(0).unwrap().contains("+TWO"));
        assert!(!diff.patch(0).unwrap().contains("ELEVEN"));
        assert_eq!(diff.patch(2), None);
    }

    fn hunk(body: &str) -> Hunk {
        Hunk {
            header: body.lines().next().unwrap().to_string(),
            body: body.to_string(),
        }
    }

    /// The two halves of the rewrite, side by side on one hunk, because the only
    /// way to be sure `reverse` is not backwards is to see both answers at once:
    /// forwards the unpicked `-` becomes context and the unpicked `+` goes,
    /// reversed it is the other way round.
    #[test]
    fn the_lines_nobody_picked_swap_sides_when_the_patch_is_reversed() {
        let h = hunk("@@ -1,3 +1,3 @@\n-one\n+ONE\n two\n-three\n+THREE\n");
        // Body lines: 1 `-one`, 2 `+ONE`, 3 ` two`, 4 `-three`, 5 `+THREE`.
        assert_eq!(
            rewrite(&h, &[4, 5], false).unwrap(),
            "@@ -1,3 +1,3 @@\n one\n two\n-three\n+THREE\n"
        );
        assert_eq!(
            rewrite(&h, &[4, 5], true).unwrap(),
            "@@ -1,3 +1,3 @@\n ONE\n two\n-three\n+THREE\n"
        );
    }

    /// A range that empties or fills has to move its start by one, because a
    /// unified diff numbers an empty range by the line *before* it. Without it,
    /// picking a line out of a newly added file writes `-0,2` and names a line
    /// zero that no file has.
    #[test]
    fn a_range_that_empties_or_fills_moves_its_start_by_one() {
        // A new file, one line of it taken back out of the index.
        let added = hunk("@@ -0,0 +1,3 @@\n+p\n+q\n+r\n");
        assert!(
            rewrite(&added, &[2], true).unwrap().starts_with("@@ -1,2 +1,3 @@\n"),
            "{}",
            rewrite(&added, &[2], true).unwrap()
        );
        // ...and the other way: a replacement whose new side is left empty.
        let gone = hunk("@@ -1,2 +1,1 @@\n-a\n-b\n+c\n");
        assert!(
            rewrite(&gone, &[1, 2], false).unwrap().starts_with("@@ -1,2 +0,0 @@\n"),
            "{}",
            rewrite(&gone, &[1, 2], false).unwrap()
        );
    }

    /// git leaves a count of one out and hangs the enclosing function off the
    /// end, and both have to come back the way they went in.
    #[test]
    fn a_hunk_header_with_no_counts_and_a_tail_survives_the_rewrite() {
        assert_eq!(
            header("@@ -1 +1 @@ fn foo() {").unwrap(),
            (1, 1, 1, 1, " fn foo() {")
        );
        let h = hunk("@@ -1 +1 @@ fn foo() {\n-a\n+b\n");
        assert_eq!(
            rewrite(&h, &[1, 2], false).unwrap(),
            "@@ -1,1 +1,1 @@ fn foo() {\n-a\n+b\n"
        );
        assert!(header("not a header").is_err());
        // Nothing picked is not an empty patch, it is a refusal.
        assert!(rewrite(&h, &[], false).is_err());
    }

    /// A binary file's diff is a preamble and nothing else; that is a diff with
    /// no hunks rather than a parse failure.
    #[test]
    fn a_diff_with_no_hunks_is_all_preamble() {
        let (preamble, hunks) = split("diff --git a/x b/x\nBinary files a/x and b/x differ\n");
        assert!(hunks.is_empty());
        assert!(preamble.ends_with("differ\n"));
    }
}
