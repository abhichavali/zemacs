//! zemacs-project — `project.el`, minus the backends.
//!
//! A project is a directory that some [`Marker`] file says is the top of
//! something. Everything else here hangs off that one answer:
//!
//! * **Where am I** ([`find`]): walk up from a path until a marker turns up, and
//!   report the root *and* the marker, because "which file made this the root"
//!   is the first question asked when the answer looks wrong.
//! * **What is in it** ([`files`], [`directories`]): the candidate lists behind
//!   `project-find-file` and a project-scoped dired. Paths come back *relative
//!   to the root* — they are what a completion prompt shows, and the absolute
//!   form is one `root.join` away when a file actually has to be opened.
//! * **Faster than that** ([`Cache`]): re-walking a repository between two
//!   keystrokes is the difference between a prompt that feels instant and one
//!   that does not. The cache is owned by the caller — there is no global state
//!   in this crate, so two frames can hold two of them and neither surprises the
//!   other.
//! * **Where have I been** ([`recent`], [`remember`]): a persisted list so that
//!   "switch project" is a prompt over real history rather than a path to type.
//! * **What is in it, textually** ([`search`]): ripgrep, scoped to the root.
//! * **How is it built** ([`Project::build`], [`Project::test`]): a program plus
//!   arguments, never a shell string — a project living in `~/my code/thing`
//!   must not need quoting to compile.
//!
//! Nothing here prints, panics on bad input, or holds state between calls.
//! "Nothing found" is an empty result; only a genuinely broken world (a root
//! that is not a directory, a ripgrep that is not installed) is an `Err`.
//!
//! ponytail: no project-wide replace, no `project-shell`, no per-project
//! variables, no `.projectile`-style ignore file. Each is additive and none is
//! needed to open a file in a repository.

use std::path::{Path, PathBuf};

mod files;
mod recent;
mod search;

pub use files::{directories, files, Cache, Files, FILE_LIMIT};
pub use recent::{recent, recents_file, remember};
pub use search::{search, SEARCH_LIMIT};

/// What made a directory a project root.
///
/// The variant order *is* the precedence order — see [`find`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Marker {
    /// `.project`, dropped by hand. The only way to overrule the rule below it:
    /// a workspace member inside a repository becomes its own project exactly
    /// when someone says so.
    Explicit,
    Git,
    Mercurial,
    Subversion,
    Cargo,
    /// `package.json`.
    Npm,
    /// `pyproject.toml`.
    Python,
    /// `go.mod`.
    Go,
    /// `Makefile`. Only that spelling: `makefile` and `GNUmakefile` are missed
    /// on a case-sensitive filesystem, and are one more variant each if that
    /// ever matters.
    Make,
}

/// Hand-placed, and it wins over everything.
const EXPLICIT: [Marker; 1] = [Marker::Explicit];
/// A repository is a project. Checked before [`BUILD`] in every directory.
const VCS: [Marker; 3] = [Marker::Git, Marker::Mercurial, Marker::Subversion];
/// Only consulted when the walk reaches `/` without finding any of the above.
const BUILD: [Marker; 5] = [
    Marker::Cargo,
    Marker::Npm,
    Marker::Python,
    Marker::Go,
    Marker::Make,
];

impl Marker {
    /// The file or directory whose presence is the marker.
    pub fn file_name(self) -> &'static str {
        match self {
            Marker::Explicit => ".project",
            Marker::Git => ".git",
            Marker::Mercurial => ".hg",
            Marker::Subversion => ".svn",
            Marker::Cargo => "Cargo.toml",
            Marker::Npm => "package.json",
            Marker::Python => "pyproject.toml",
            Marker::Go => "go.mod",
            Marker::Make => "Makefile",
        }
    }

    /// One word for a modeline, where `.git` reads as a directory and `git`
    /// reads as a fact about the project.
    pub fn label(self) -> &'static str {
        match self {
            Marker::Explicit => "project",
            Marker::Git => "git",
            Marker::Mercurial => "hg",
            Marker::Subversion => "svn",
            Marker::Cargo => "cargo",
            Marker::Npm => "npm",
            Marker::Python => "python",
            Marker::Go => "go",
            Marker::Make => "make",
        }
    }

    /// `(build, test)`, or `None` for a marker that says nothing about how the
    /// code is built — every VCS one.
    fn commands(self) -> Option<(Command, Command)> {
        let (build, test) = match self {
            Marker::Cargo => (("cargo", &["build"][..]), ("cargo", &["test"][..])),
            Marker::Npm => (("npm", &["run", "build"][..]), ("npm", &["test"][..])),
            // `python3`, not `python`: on a machine with both, `python` is
            // either python 2 or missing entirely.
            Marker::Python => (
                ("python3", &["-m", "build"][..]),
                ("python3", &["-m", "pytest"][..]),
            ),
            Marker::Go => (
                ("go", &["build", "./..."][..]),
                ("go", &["test", "./..."][..]),
            ),
            // No target for `make`: the default target is the build, and it is
            // the one thing every Makefile has.
            Marker::Make => (("make", &[][..]), ("make", &["test"][..])),
            Marker::Explicit | Marker::Git | Marker::Mercurial | Marker::Subversion => return None,
        };
        Some((Command::new(build.0, build.1), Command::new(test.0, test.1)))
    }
}

/// A program to spawn plus its arguments, deliberately *not* a shell string.
///
/// Handing a caller `"cargo test"` means the caller splits it, and splitting is
/// where paths with spaces and arguments with quotes go wrong. Spawn
/// [`Command::program`] with [`Command::args`] and there is nothing to escape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
}

impl Command {
    fn new(program: &str, args: &[&str]) -> Command {
        Command {
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }

    /// For the echo area — "running: cargo test". Not for a shell: this is
    /// space-joined, and a shell would re-split it wrongly.
    pub fn display(&self) -> String {
        let mut out = self.program.clone();
        for arg in &self.args {
            out.push(' ');
            out.push_str(arg);
        }
        out
    }
}

/// A project root and the reason it is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// Absolute, and symlink-resolved as far as the filesystem allowed.
    pub root: PathBuf,
    /// Which marker [`find`] stopped on.
    pub marker: Marker,
}

impl Project {
    /// The root's last component, for a prompt listing several projects — the
    /// full path is unreadable at a glance and mostly identical between them.
    pub fn name(&self) -> String {
        self.root
            .file_name()
            .unwrap_or(self.root.as_os_str())
            .to_string_lossy()
            .into_owned()
    }

    /// What to run to build this project, to be spawned with [`Project::root`]
    /// as the working directory. `None` when nothing in the root says how.
    pub fn build(&self) -> Option<Command> {
        Some(self.build_marker()?.commands()?.0)
    }

    /// What to run to test it. Same contract as [`Project::build`].
    pub fn test(&self) -> Option<Command> {
        Some(self.build_marker()?.commands()?.1)
    }

    /// The marker that describes how the project is *built*, which is usually
    /// not the one that made it a root: a Cargo workspace is identified by its
    /// `.git`, and `cargo build` is still the answer. Costs one `stat` per
    /// build marker, and only when someone asks to compile.
    fn build_marker(&self) -> Option<Marker> {
        if BUILD.contains(&self.marker) {
            return Some(self.marker);
        }
        marker_in(&self.root, &BUILD)
    }
}

/// The project containing `start`, or `None` if nothing above it is a project.
///
/// `start` may be a file or a directory, and need not exist yet — the root of a
/// file that is about to be created is the same question as the root of one that
/// already is. A relative path is resolved against the process's working
/// directory.
///
/// **Precedence: the deepest VCS root wins, and a build file only counts when
/// there is no VCS root anywhere above.** So a Cargo workspace member inside a
/// git repository resolves to the *repository*. The reason is that the interesting
/// operations are repository-shaped: `project-find-file` should offer a sibling
/// crate, and a project search should cross crate boundaries, because that is
/// where the definition being hunted actually lives. The cost is that a
/// genuinely independent sub-project inside a repository resolves too high, and
/// the fix for that is a `.project` file in it — checked before the VCS marker
/// in every directory, so it can pull the root back down.
///
/// Nested repositories are the same rule read the other way: the walk stops at
/// the *first* `.git` going up, so a submodule is its own project rather than
/// part of its superproject.
pub fn find(start: &Path) -> Option<Project> {
    let start = absolute(start);
    // A file's project is its directory's project. `parent` is `None` only for
    // `/`, which `is_dir` already caught.
    let from = if start.is_dir() {
        start.as_path()
    } else {
        start.parent()?
    };

    let mut build: Option<Project> = None;
    for dir in from.ancestors() {
        if let Some(marker) = marker_in(dir, &EXPLICIT).or_else(|| marker_in(dir, &VCS)) {
            return Some(Project {
                root: dir.to_path_buf(),
                marker,
            });
        }
        // Remembered, not returned: a `.git` further up still outranks it. The
        // first one found is kept, so the *deepest* build file wins among
        // themselves — a workspace member is a better answer than nothing.
        if build.is_none() {
            if let Some(marker) = marker_in(dir, &BUILD) {
                build = Some(Project {
                    root: dir.to_path_buf(),
                    marker,
                });
            }
        }
    }
    build
}

/// The first marker of `tier` present in `dir`.
///
/// `exists` rather than `symlink_metadata`, because `.git` is a *file* in a
/// submodule and in a linked worktree, and both are real repositories.
fn marker_in(dir: &Path, tier: &[Marker]) -> Option<Marker> {
    tier.iter()
        .copied()
        .find(|m| dir.join(m.file_name()).exists())
}

/// Absolute and symlink-free, so that `ancestors()` walks real parents: without
/// this, `a/../b` would have `a/..` as an ancestor and the markers in `a` would
/// be tested against the wrong directory.
fn absolute(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    resolved(&path)
}

/// `canonicalize` for a path that may not exist yet: resolve the deepest
/// ancestor that does and re-attach the rest.
fn resolved(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolved(parent).join(name),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mapping is a table, and a table is worth pinning down so that a
    /// mistyped `npm run build` is a test failure rather than a puzzled user.
    #[test]
    fn every_build_marker_names_a_program_and_no_vcs_marker_does() {
        for marker in BUILD {
            let (build, test) = marker.commands().expect("build marker has commands");
            assert!(!build.program.is_empty(), "{marker:?}");
            assert!(!test.program.is_empty(), "{marker:?}");
        }
        for marker in VCS.iter().chain(EXPLICIT.iter()) {
            assert!(marker.commands().is_none(), "{marker:?}");
        }
        assert_eq!(Marker::Cargo.commands().unwrap().1.display(), "cargo test");
        assert_eq!(Marker::Npm.commands().unwrap().0.display(), "npm run build");
        assert_eq!(Marker::Make.commands().unwrap().0.display(), "make");
    }
}
