//! Integration tests. Every one builds a real tree in a temporary directory and
//! drives the public API against it: what this crate *is* is the behaviour of a
//! filesystem, of `git ls-files` and of `rg`, none of which a mock would tell
//! the truth about. Tests needing a binary that may not be installed skip
//! rather than fail.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use zemacs_project::{Cache, Marker, Project};

// ------------------------------------------------------------- scaffolding

/// A temp directory that deletes itself. Not worth a dependency.
struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Temp {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "zemacs-project-{}-{}-{tag}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        // Canonical, because macOS puts temp directories under a symlinked
        // `/var` and every root this crate returns is symlink-resolved.
        Temp(path.canonicalize().unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// Create `rel` and everything above it, with `body` inside.
    fn write(&self, rel: &str, body: &str) -> PathBuf {
        let path = self.0.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    fn dir(&self, rel: &str) -> PathBuf {
        let path = self.0.join(rel);
        fs::create_dir_all(&path).unwrap();
        path
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn have(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "setup `git {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository with an identity of its own, so the suite passes on a machine
/// with no global git config.
fn init(dir: &Path) {
    git(dir, &["init", "-q", "."]);
    git(dir, &["config", "user.email", "test@zemacs.invalid"]);
    git(dir, &["config", "user.name", "zemacs test"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

fn found(start: &Path) -> Project {
    zemacs_project::find(start).unwrap_or_else(|| panic!("no project for {}", start.display()))
}

fn names(files: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
    names.sort();
    names
}

// ---------------------------------------------------------- finding a root

#[test]
fn a_marker_is_found_from_several_levels_down() {
    let temp = Temp::new("nested");
    temp.write(".project", "");
    let deep = temp.write("a/b/c/d/e/notes.txt", "hi");

    let project = found(&deep);
    assert_eq!(project.root, temp.path());
    assert_eq!(project.marker, Marker::Explicit);
    // A directory is its own start, not its parent's.
    assert_eq!(found(&temp.0.join("a/b/c")).root, temp.path());
}

#[test]
fn a_cargo_member_inside_a_git_repo_resolves_to_the_repo() {
    let temp = Temp::new("workspace");
    temp.dir(".git");
    temp.write("Cargo.toml", "[workspace]\n");
    let member = temp.write("crates/render/Cargo.toml", "[package]\n");
    temp.write("crates/render/src/lib.rs", "");

    let project = found(&member);
    assert_eq!(project.root, temp.path());
    assert_eq!(project.marker, Marker::Git);

    // Even when the repository root has no build file of its own, so the only
    // Cargo.toml in the tree is the deep one.
    let bare = Temp::new("bare-workspace");
    bare.dir(".git");
    let inner = bare.write("crates/render/Cargo.toml", "[package]\n");
    assert_eq!(found(&inner).root, bare.path());
}

#[test]
fn a_build_file_is_the_root_only_when_no_vcs_lies_above_it() {
    let temp = Temp::new("no-vcs");
    temp.write("Cargo.toml", "[package]\n");
    let src = temp.write("src/deep/lib.rs", "");

    let project = found(&src);
    assert_eq!(project.root, temp.path());
    assert_eq!(project.marker, Marker::Cargo);
}

#[test]
fn the_deepest_build_file_wins_when_there_is_no_vcs_at_all() {
    let temp = Temp::new("nested-build");
    temp.write("package.json", "{}");
    let inner = temp.write("tools/gen/go.mod", "module gen\n");

    let project = found(&inner);
    assert_eq!(project.root, temp.path().join("tools/gen"));
    assert_eq!(project.marker, Marker::Go);
}

/// A submodule is a repository, so it is a project — the walk stops at the
/// first VCS marker going up, not the outermost one.
#[test]
fn the_deepest_vcs_root_wins_so_a_nested_repo_is_its_own_project() {
    let temp = Temp::new("submodule");
    temp.dir(".git");
    temp.dir("vendor/lib/.git");
    let inner = temp.write("vendor/lib/src/main.rs", "");

    assert_eq!(found(&inner).root, temp.path().join("vendor/lib"));
    assert_eq!(found(&temp.0.join("src/main.rs")).root, temp.path());
}

/// The escape hatch from the rule above it: a `.project` file pulls the root
/// back down inside a repository.
#[test]
fn an_explicit_marker_overrules_a_git_root_above_it() {
    let temp = Temp::new("explicit");
    temp.dir(".git");
    temp.write("crates/render/.project", "");
    let src = temp.write("crates/render/src/lib.rs", "");

    let project = found(&src);
    assert_eq!(project.root, temp.path().join("crates/render"));
    assert_eq!(project.marker, Marker::Explicit);
}

/// `.git` is a *file*, not a directory, in a submodule and in a linked
/// worktree, and both are real repositories.
#[test]
fn a_git_file_marks_a_root_the_same_as_a_git_directory() {
    let temp = Temp::new("gitfile");
    temp.write(".git", "gitdir: /elsewhere/.git/worktrees/x\n");
    let src = temp.write("src/lib.rs", "");
    assert_eq!(found(&src).marker, Marker::Git);
    assert_eq!(found(&src).root, temp.path());
}

/// The path of a file about to be created has the same project as the directory
/// it will land in.
#[test]
fn a_path_that_does_not_exist_yet_still_finds_its_root() {
    let temp = Temp::new("unborn");
    temp.dir(".git");
    temp.dir("src");
    assert_eq!(
        found(&temp.0.join("src/brand/new/file.rs")).root,
        temp.path()
    );
}

#[test]
fn a_directory_under_no_marker_at_all_has_no_project() {
    let temp = Temp::new("orphan");
    let deep = temp.dir("just/directories");
    // Nothing was created above the temp directory, and a temp directory is
    // never inside a checkout.
    assert!(zemacs_project::find(&deep).is_none());
}

#[test]
fn the_root_is_named_by_its_last_component() {
    let temp = Temp::new("named");
    let root = temp.dir("my-project");
    fs::create_dir(root.join(".git")).unwrap();
    assert_eq!(found(&root).name(), "my-project");
}

// --------------------------------------------------------- build and test

#[test]
fn the_build_command_comes_from_the_root_not_from_the_marker() {
    let temp = Temp::new("commands");
    temp.dir(".git");
    temp.write("Cargo.toml", "[package]\n");

    let project = found(temp.path());
    assert_eq!(project.marker, Marker::Git);
    let build = project.build().unwrap();
    assert_eq!(build.program, "cargo");
    assert_eq!(build.args, ["build"]);
    assert_eq!(project.test().unwrap().display(), "cargo test");
}

#[test]
fn a_project_with_nothing_to_build_says_so_instead_of_guessing() {
    let temp = Temp::new("nobuild");
    temp.dir(".git");
    let project = found(temp.path());
    assert!(project.build().is_none());
    assert!(project.test().is_none());
}

#[test]
fn a_makefile_project_builds_with_bare_make() {
    let temp = Temp::new("make");
    temp.write("Makefile", "all:\n\techo hi\n");
    let project = found(temp.path());
    assert_eq!(project.marker, Marker::Make);
    assert!(project.build().unwrap().args.is_empty());
    assert_eq!(project.test().unwrap().args, ["test"]);
}

// ------------------------------------------------------------------ files

#[test]
fn a_walked_project_skips_build_output_and_dependency_directories() {
    let temp = Temp::new("walk");
    temp.write("Cargo.toml", "[package]\n");
    temp.write("src/lib.rs", "");
    temp.write("src/nested/deep.rs", "");
    temp.write("target/debug/huge.o", "");
    temp.write("node_modules/left-pad/index.js", "");
    temp.write(".venv/lib/python3/site.py", "");
    // Dotfiles that are not in the skip list are source, and wanted.
    temp.write(".github/workflows/ci.yml", "");

    let files = zemacs_project::files(temp.path()).unwrap();
    assert!(!files.truncated);
    assert_eq!(
        names(&files.files),
        [
            ".github/workflows/ci.yml",
            "Cargo.toml",
            "src/lib.rs",
            "src/nested/deep.rs",
        ]
    );
}

#[test]
fn a_git_project_lists_new_files_and_leaves_ignored_ones_out() {
    if !have("git") {
        return;
    }
    let temp = Temp::new("gitfiles");
    init(temp.path());
    temp.write(".gitignore", "ignored.txt\nbuilt/\n");
    temp.write("tracked.rs", "");
    temp.write("ignored.txt", "");
    temp.write("built/output.o", "");
    git(temp.path(), &["add", "."]);
    git(temp.path(), &["commit", "-qm", "initial"]);
    // Never added: the file most likely to be the one being looked for.
    temp.write("src/brand-new.rs", "");

    let files = zemacs_project::files(temp.path()).unwrap();
    assert_eq!(
        names(&files.files),
        [".gitignore", "src/brand-new.rs", "tracked.rs"]
    );
}

/// The listing is of the project, not of the repository around it.
#[test]
fn a_project_inside_a_repo_lists_only_its_own_subtree() {
    if !have("git") {
        return;
    }
    let temp = Temp::new("subproject");
    init(temp.path());
    temp.write("outside.rs", "");
    temp.write("crates/render/.project", "");
    temp.write("crates/render/src/lib.rs", "");

    let project = found(&temp.0.join("crates/render/src/lib.rs"));
    let files = zemacs_project::files(&project.root).unwrap();
    assert_eq!(names(&files.files), [".project", "src/lib.rs"]);
}

#[test]
fn listed_paths_are_relative_to_the_root_and_join_back_to_real_files() {
    let temp = Temp::new("relative");
    temp.write("Cargo.toml", "[package]\n");
    temp.write("src/lib.rs", "fn main() {}");

    let files = zemacs_project::files(temp.path()).unwrap();
    for rel in &files.files {
        assert!(rel.is_relative(), "{}", rel.display());
        assert!(temp.path().join(rel).is_file(), "{}", rel.display());
    }
}

/// A symlink is listed but never followed, so a link to `/` does not turn a
/// project listing into a walk of the whole disk.
#[cfg(unix)]
#[test]
fn a_symlinked_directory_is_listed_but_not_descended_into() {
    let temp = Temp::new("symlink");
    temp.write("Cargo.toml", "[package]\n");
    temp.write("real/inside.rs", "");
    std::os::unix::fs::symlink(temp.0.join("real"), temp.0.join("link")).unwrap();

    let files = zemacs_project::files(temp.path()).unwrap();
    assert_eq!(
        names(&files.files),
        ["Cargo.toml", "link", "real/inside.rs"]
    );
}

// ------------------------------------------------------------ directories

#[test]
fn directories_are_every_parent_of_a_listed_file_plus_the_root() {
    let temp = Temp::new("dirs");
    temp.write("Cargo.toml", "[package]\n");
    temp.write("src/lib.rs", "");
    temp.write("src/deep/inner/mod.rs", "");
    temp.write("tests/it.rs", "");
    // Empty, and therefore invisible — the same thing git does.
    temp.dir("empty");

    let files = zemacs_project::files(temp.path()).unwrap();
    assert_eq!(
        names(&zemacs_project::directories(&files)),
        [".", "src", "src/deep", "src/deep/inner", "tests"]
    );
}

// ------------------------------------------------------------------ cache

#[test]
fn the_cache_serves_the_same_listing_until_it_is_told_to_forget() {
    let temp = Temp::new("cache");
    temp.write("Cargo.toml", "[package]\n");
    temp.write("src/lib.rs", "");

    let mut cache = Cache::new();
    let before = cache.files(temp.path()).unwrap().files.len();
    temp.write("src/added.rs", "");

    // Well inside the staleness window, so the new file is not visible yet.
    assert_eq!(cache.files(temp.path()).unwrap().files.len(), before);

    cache.forget(temp.path());
    assert_eq!(cache.files(temp.path()).unwrap().files.len(), before + 1);
}

#[test]
fn a_root_that_is_not_a_directory_is_an_error_rather_than_an_empty_project() {
    let temp = Temp::new("badroot");
    let file = temp.write("Cargo.toml", "[package]\n");
    assert!(zemacs_project::files(&file).is_err());
    assert!(Cache::new().files(&temp.0.join("nope")).is_err());
}

// ----------------------------------------------------------------- search

#[test]
fn search_hits_are_relative_paths_with_a_line_number() {
    if !have("rg") {
        return;
    }
    let temp = Temp::new("search");
    temp.write("Cargo.toml", "[package]\n");
    temp.write("src/lib.rs", "fn one() {}\nfn needle() {}\n");
    temp.write("src/other.rs", "// nothing here\n");

    let hits = zemacs_project::search(temp.path(), "needle").unwrap();
    assert_eq!(hits, ["src/lib.rs:2:fn needle() {}"]);

    // The shape the caller parses back: `path:line:text`, split twice.
    let mut parts = hits[0].splitn(3, ':');
    let path = parts.next().unwrap();
    assert_eq!(parts.next().unwrap(), "2");
    assert!(temp.path().join(path).is_file());
}

#[test]
fn a_pattern_matching_nothing_is_an_empty_list_and_not_an_error() {
    if !have("rg") {
        return;
    }
    let temp = Temp::new("nomatch");
    temp.write("src/lib.rs", "fn one() {}\n");
    assert!(zemacs_project::search(temp.path(), "haystack")
        .unwrap()
        .is_empty());
    // An empty pattern would otherwise match every line in the project.
    assert!(zemacs_project::search(temp.path(), "   ")
        .unwrap()
        .is_empty());
}

#[test]
fn a_pattern_that_will_not_compile_reports_ripgreps_own_complaint() {
    if !have("rg") {
        return;
    }
    let temp = Temp::new("badregex");
    temp.write("src/lib.rs", "fn one() {}\n");
    assert!(zemacs_project::search(temp.path(), "a(b").is_err());
}

/// A leading dash is a pattern, not a flag, and nothing reaches a shell.
#[test]
fn a_pattern_that_looks_like_a_flag_is_still_a_pattern() {
    if !have("rg") {
        return;
    }
    let temp = Temp::new("dashes");
    temp.write("src/lib.rs", "let x = -count;\n");
    let hits = zemacs_project::search(temp.path(), "-count").unwrap();
    assert_eq!(hits, ["src/lib.rs:1:let x = -count;"]);
}
