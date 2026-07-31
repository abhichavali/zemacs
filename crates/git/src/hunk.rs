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

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

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

fn apply(
    repo: &Path,
    diff: &FileDiff,
    index: usize,
    wanted: Section,
    flags: &[&str],
) -> Result<()> {
    // Applying a staged diff to the working tree, or a working-tree diff to the
    // index, would half-succeed and leave a mess: refuse rather than guess.
    if diff.section != wanted {
        bail!("expected a {wanted:?} diff, got {:?}", diff.section);
    }
    let Some(patch) = diff.patch(index) else {
        bail!("no hunk {index} in {}", diff.path.display());
    };
    let mut args = vec!["apply".to_string()];
    args.extend(flags.iter().map(|f| f.to_string()));
    // A tab-versus-space quibble must not stop a hunk the user can see from
    // being staged; `-` is the patch on stdin, so no temporary file exists to
    // leak or to race with.
    args.push("--whitespace=nowarn".into());
    args.push("-".into());
    run_with(repo, args, &[], Some(&patch))?;
    Ok(())
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

    /// A binary file's diff is a preamble and nothing else; that is a diff with
    /// no hunks rather than a parse failure.
    #[test]
    fn a_diff_with_no_hunks_is_all_preamble() {
        let (preamble, hunks) = split("diff --git a/x b/x\nBinary files a/x and b/x differ\n");
        assert!(hunks.is_empty());
        assert!(preamble.ends_with("differ\n"));
    }
}
