//! Integration tests. Every one of these builds a real repository in a
//! temporary directory and drives the public API against it — nothing is
//! mocked, because the exact behaviour of the `git` binary *is* what this crate
//! is. `user.email`/`user.name` are set locally so the suite passes on a
//! machine with no global git identity, and `push`/`pull` run against a local
//! bare repository used as `origin`, so nothing here touches the network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use zemacs_git::{ChangeKind, FileChange, InProgress, Line, Section};

// ------------------------------------------------------------- scaffolding

/// A temp directory that deletes itself. Not worth a dependency.
struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Temp {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "zemacs-git-{}-{}-{tag}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Temp(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// For setup steps that are *expected* to fail, like a conflicting merge.
fn git_try(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
        .status
        .success()
}

fn init(tag: &str) -> Temp {
    let temp = Temp::new(tag);
    let dir = temp.path();
    git(dir, &["init", "-q", "."]);
    // `symbolic-ref` rather than `init -b main`: works on every git version,
    // and pins the branch name so assertions do not depend on the machine's
    // `init.defaultBranch`.
    git(dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(dir, &["config", "user.email", "test@zemacs.invalid"]);
    git(dir, &["config", "user.name", "zemacs test"]);
    // A global `commit.gpgsign = true` would otherwise break every commit here.
    git(dir, &["config", "commit.gpgsign", "false"]);
    // Deterministic unicode filenames regardless of the filesystem's
    // normalisation (HFS+ hands back NFD, APFS hands back what you wrote).
    git(dir, &["config", "core.precomposeunicode", "true"]);
    temp
}

fn write(repo: &Path, rel: &str, body: &str) {
    let path = repo.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn commit_all(repo: &Path, message: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", message]);
}

fn log_count(repo: &Path) -> usize {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["log", "--oneline"])
        .output()
        .unwrap();
    // Fails on an unborn HEAD, which is simply zero commits.
    if !out.status.success() {
        return 0;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

fn canon(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

/// Index of the line whose map entry is a file row for `path`.
fn line_of(map: &[Line], path: &str) -> usize {
    map.iter()
        .position(|line| matches!(line, Line::File { path: p, .. } if p == Path::new(path)))
        .unwrap_or_else(|| panic!("no file row for {path} in {map:#?}"))
}

// ------------------------------------------------------------------ tests

#[test]
fn repo_root_finds_the_root_and_gives_up_outside_a_repository() {
    let repo = init("root");
    let root = canon(repo.path());
    write(repo.path(), "sub/deep/file.rs", "x\n");

    assert_eq!(
        zemacs_git::repo_root(&repo.path().join("sub/deep")).map(|p| canon(&p)),
        Some(root.clone())
    );
    // A file, not a directory: its parent is used.
    assert_eq!(
        zemacs_git::repo_root(&repo.path().join("sub/deep/file.rs")).map(|p| canon(&p)),
        Some(root)
    );

    let plain = Temp::new("not-a-repo");
    assert_eq!(zemacs_git::repo_root(plain.path()), None);
    // ...and status there is an error, not a panic and not a clean tree.
    let err = zemacs_git::status(plain.path()).unwrap_err().to_string();
    assert!(err.contains("not a git repository"), "{err}");
}

#[test]
fn status_sorts_files_into_the_right_buckets() {
    let repo = init("buckets");
    write(repo.path(), "tracked.txt", "one\n");
    commit_all(repo.path(), "first");

    write(repo.path(), "tracked.txt", "one\ntwo\n"); // modified, unstaged
    write(repo.path(), "fresh.txt", "new\n");
    git(repo.path(), &["add", "fresh.txt"]); // added, staged
    write(repo.path(), "loose.txt", "hello\n"); // untracked

    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert!(!status.unborn && !status.detached);
    assert_eq!(status.upstream, None);
    assert_eq!(status.in_progress, None);
    assert!(!status.is_clean() && !status.has_conflicts());
    assert_eq!(
        status.staged,
        vec![FileChange {
            path: "fresh.txt".into(),
            status: ChangeKind::Added
        }]
    );
    assert_eq!(
        status.unstaged,
        vec![FileChange {
            path: "tracked.txt".into(),
            status: ChangeKind::Modified
        }]
    );
    assert_eq!(status.untracked, vec![PathBuf::from("loose.txt")]);
}

#[test]
fn a_deletion_is_reported_and_stageable_as_a_deletion() {
    let repo = init("delete");
    write(repo.path(), "doomed.txt", "bye\n");
    commit_all(repo.path(), "first");
    fs::remove_file(repo.path().join("doomed.txt")).unwrap();

    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.unstaged[0].status, ChangeKind::Deleted);

    zemacs_git::stage(repo.path(), Path::new("doomed.txt")).unwrap();
    let status = zemacs_git::status(repo.path()).unwrap();
    assert!(status.unstaged.is_empty());
    assert_eq!(status.staged[0].status, ChangeKind::Deleted);
}

#[test]
fn stage_and_unstage_move_files_between_buckets() {
    let repo = init("stage");
    write(repo.path(), "tracked.txt", "one\n");
    commit_all(repo.path(), "first");
    write(repo.path(), "tracked.txt", "one\ntwo\n");
    write(repo.path(), "loose.txt", "hello\n");

    zemacs_git::stage(repo.path(), Path::new("tracked.txt")).unwrap();
    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.staged[0].path, PathBuf::from("tracked.txt"));
    assert!(status.unstaged.is_empty());
    assert_eq!(status.untracked, vec![PathBuf::from("loose.txt")]);

    zemacs_git::unstage(repo.path(), Path::new("tracked.txt")).unwrap();
    let status = zemacs_git::status(repo.path()).unwrap();
    assert!(status.staged.is_empty());
    assert_eq!(status.unstaged[0].path, PathBuf::from("tracked.txt"));

    // stage_all also picks up untracked files, the way `git add -A` does.
    zemacs_git::stage_all(repo.path()).unwrap();
    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.staged.len(), 2);
    assert!(status.unstaged.is_empty() && status.untracked.is_empty());

    zemacs_git::unstage_all(repo.path()).unwrap();
    let status = zemacs_git::status(repo.path()).unwrap();
    assert!(status.staged.is_empty());
    assert_eq!(status.unstaged[0].path, PathBuf::from("tracked.txt"));
    assert_eq!(status.untracked, vec![PathBuf::from("loose.txt")]);
}

#[test]
fn commit_makes_a_commit_and_refuses_an_empty_message() {
    let repo = init("commit");
    write(repo.path(), "a.txt", "a\n");
    zemacs_git::stage_all(repo.path()).unwrap();

    // Refused before git is invoked, so nothing happens at all.
    assert!(zemacs_git::commit(repo.path(), "").is_err());
    assert!(zemacs_git::commit(repo.path(), "  \n\t ").is_err());
    assert_eq!(log_count(repo.path()), 0);

    let summary = zemacs_git::commit(repo.path(), "first commit").unwrap();
    assert!(summary.contains("first commit"), "{summary}");
    assert_eq!(log_count(repo.path()), 1);
    assert!(zemacs_git::status(repo.path()).unwrap().is_clean());

    write(repo.path(), "b.txt", "b\n");
    zemacs_git::stage_all(repo.path()).unwrap();
    zemacs_git::commit(repo.path(), "second commit").unwrap();
    assert_eq!(log_count(repo.path()), 2);

    // A failing git command surfaces git's own stderr rather than succeeding.
    let err = zemacs_git::commit(repo.path(), "nothing is staged")
        .unwrap_err()
        .to_string();
    assert!(err.contains("nothing to commit"), "{err}");
    assert_eq!(log_count(repo.path()), 2);
}

#[test]
fn a_path_with_spaces_quotes_and_non_ascii_survives_status_render_and_stage() {
    let repo = init("weird-path");
    let weird = "src/my file \"x\"/café.rs";
    write(repo.path(), weird, "fn main() {}\n");

    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.untracked, vec![PathBuf::from(weird)]);

    // Through render and back out of the line map, byte for byte.
    let (text, map) = zemacs_git::render(&status);
    let index = line_of(&map, weird);
    let Line::File { path, section } = &map[index] else {
        unreachable!()
    };
    assert_eq!(path, Path::new(weird));
    assert_eq!(*section, Section::Untracked);
    assert_eq!(text.lines().nth(index).unwrap(), format!("? {weird}"));

    zemacs_git::stage(repo.path(), path).unwrap();
    let status = zemacs_git::status(repo.path()).unwrap();
    assert!(status.untracked.is_empty());
    assert_eq!(
        status.staged,
        vec![FileChange {
            path: weird.into(),
            status: ChangeKind::Added
        }]
    );

    zemacs_git::commit(repo.path(), "add a difficult filename").unwrap();
    write(repo.path(), weird, "fn main() { todo!() }\n");
    let diff = zemacs_git::diff(repo.path(), Path::new(weird), false).unwrap();
    assert!(diff.contains("todo!()"), "{diff}");
}

#[test]
fn hostile_filenames_are_data_and_never_commands_or_flags() {
    let repo = init("hostile");
    // Never goes through a shell, so this is a file with a silly name.
    let names = ["; rm -rf ~", "$(touch pwned)", "-n", "--cached"];
    for name in names {
        write(repo.path(), name, "harmless\n");
    }

    let status = zemacs_git::status(repo.path()).unwrap();
    let mut untracked = status.untracked.clone();
    untracked.sort();
    let mut expected: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();
    expected.sort();
    assert_eq!(untracked, expected);

    // Each stages as a path, not as an option — `--` is what makes `-n` a file.
    for name in names {
        zemacs_git::stage(repo.path(), Path::new(name)).unwrap();
    }
    assert_eq!(zemacs_git::status(repo.path()).unwrap().staged.len(), 4);
    for name in names {
        zemacs_git::unstage(repo.path(), Path::new(name)).unwrap();
    }
    assert_eq!(
        zemacs_git::status(repo.path()).unwrap().untracked.len(),
        names.len()
    );
    // The subshell was never a subshell.
    assert!(!repo.path().join("pwned").exists());

    // A commit message beginning with a dash is a message, not a flag.
    zemacs_git::stage_all(repo.path()).unwrap();
    zemacs_git::commit(repo.path(), "--amend is a fine thing to write").unwrap();
    assert_eq!(log_count(repo.path()), 1);
}

#[test]
fn a_rename_is_reported_as_a_rename() {
    let repo = init("rename");
    // Rename detection is a similarity score, so the content has to be
    // substantial enough to match against itself.
    let body = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
    write(repo.path(), "old name.txt", body);
    commit_all(repo.path(), "first");

    fs::rename(
        repo.path().join("old name.txt"),
        repo.path().join("new name.txt"),
    )
    .unwrap();
    zemacs_git::stage_all(repo.path()).unwrap();

    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(
        status.staged,
        vec![FileChange {
            path: "new name.txt".into(),
            status: ChangeKind::Renamed {
                from: "old name.txt".into()
            }
        }]
    );

    let (text, map) = zemacs_git::render(&status);
    assert!(
        text.contains("R old name.txt -> new name.txt"),
        "rename rendered as garbage:\n{text}"
    );
    // The line acts on the new path, which is the one `git add` understands.
    let index = line_of(&map, "new name.txt");
    assert!(matches!(&map[index], Line::File { section, .. } if *section == Section::Staged));
}

#[test]
fn render_text_and_map_always_have_the_same_length() {
    let repo = init("render-lengths");

    // Clean, and with no commits yet.
    let status = zemacs_git::status(repo.path()).unwrap();
    let (text, map) = zemacs_git::render(&status);
    assert_eq!(text.lines().count(), map.len());
    assert!(text.contains("no commits yet"), "{text}");
    assert!(text.contains("Nothing to commit"), "{text}");

    write(repo.path(), "tracked.txt", "one\n");
    commit_all(repo.path(), "first");

    // Clean, with a commit.
    let (text, map) = zemacs_git::render(&zemacs_git::status(repo.path()).unwrap());
    assert_eq!(text.lines().count(), map.len());
    assert_eq!(text.lines().next().unwrap(), "Head:     main");

    // All three sections at once, including a filename containing a newline —
    // legal on unix, and exactly what would slide the map out of alignment.
    write(repo.path(), "tracked.txt", "one\ntwo\n");
    write(repo.path(), "staged.txt", "s\n");
    git(repo.path(), &["add", "staged.txt"]);
    write(repo.path(), "loose.txt", "l\n");
    write(repo.path(), "two\nlines.txt", "n\n");

    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.untracked.len(), 2);
    let (text, map) = zemacs_git::render(&status);
    assert_eq!(text.lines().count(), map.len(), "{text}");
    assert!(text.contains("Untracked files (2)"), "{text}");
    assert!(text.contains("Unstaged changes (1)"), "{text}");
    assert!(text.contains("Staged changes (1)"), "{text}");
    assert!(text.contains("? two?lines.txt"), "{text}");

    // Detached HEAD renders too.
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    git(repo.path(), &["checkout", "-q", "--detach", head.trim()]);
    let status = zemacs_git::status(repo.path()).unwrap();
    assert!(status.detached && status.branch.is_none());
    let (text, map) = zemacs_git::render(&status);
    assert_eq!(text.lines().count(), map.len());
    assert!(text.contains("Head:     (detached)"), "{text}");
}

#[test]
fn every_file_row_maps_to_the_file_it_shows() {
    let repo = init("line-map");
    write(repo.path(), "tracked.txt", "one\n");
    commit_all(repo.path(), "first");
    write(repo.path(), "tracked.txt", "one\ntwo\n");
    write(repo.path(), "staged.txt", "s\n");
    git(repo.path(), &["add", "staged.txt"]);
    write(repo.path(), "loose.txt", "l\n");

    let status = zemacs_git::status(repo.path()).unwrap();
    let (text, map) = zemacs_git::render(&status);
    let rows: Vec<&str> = text.lines().collect();

    for (expected_path, expected_section, expected_row) in [
        ("loose.txt", Section::Untracked, "? loose.txt"),
        ("tracked.txt", Section::Unstaged, "M tracked.txt"),
        ("staged.txt", Section::Staged, "A staged.txt"),
    ] {
        let index = line_of(&map, expected_path);
        assert_eq!(rows[index], expected_row);
        let Line::File { path, section } = &map[index] else {
            unreachable!()
        };
        assert_eq!(path, Path::new(expected_path));
        assert_eq!(*section, expected_section);
    }

    // Headings are headings, and the header is not a file.
    let heading = rows.iter().position(|r| r.starts_with("Staged")).unwrap();
    assert_eq!(map[heading], Line::Section(Section::Staged));
    assert_eq!(map[0], Line::Header);
}

#[test]
fn an_empty_repository_has_an_unborn_head_and_takes_a_first_commit() {
    let repo = init("empty");
    let status = zemacs_git::status(repo.path()).unwrap();
    assert!(status.unborn);
    assert!(!status.detached);
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert!(status.is_clean());

    write(repo.path(), "a.txt", "a\n");
    zemacs_git::stage(repo.path(), Path::new("a.txt")).unwrap();
    assert_eq!(zemacs_git::status(repo.path()).unwrap().staged.len(), 1);

    // Unstaging works with no HEAD to reset against.
    zemacs_git::unstage(repo.path(), Path::new("a.txt")).unwrap();
    let status = zemacs_git::status(repo.path()).unwrap();
    assert!(status.staged.is_empty());
    assert_eq!(status.untracked, vec![PathBuf::from("a.txt")]);

    zemacs_git::stage_all(repo.path()).unwrap();
    let summary = zemacs_git::commit(repo.path(), "first").unwrap();
    assert!(summary.contains("root-commit"), "{summary}");
    assert_eq!(log_count(repo.path()), 1);
    assert!(!zemacs_git::status(repo.path()).unwrap().unborn);
}

#[test]
fn diff_reads_the_worktree_and_the_index_separately() {
    let repo = init("diff");
    write(repo.path(), "a.txt", "base\n");
    commit_all(repo.path(), "first");

    write(repo.path(), "a.txt", "base\nindexed\n");
    git(repo.path(), &["add", "a.txt"]);
    write(repo.path(), "a.txt", "base\nindexed\nworktree\n");

    let staged = zemacs_git::diff(repo.path(), Path::new("a.txt"), true).unwrap();
    assert!(
        staged.contains("+indexed") && !staged.contains("+worktree"),
        "{staged}"
    );

    let unstaged = zemacs_git::diff(repo.path(), Path::new("a.txt"), false).unwrap();
    assert!(
        unstaged.contains("+worktree") && !unstaged.contains("+indexed"),
        "{unstaged}"
    );

    // An untracked file simply has no diff.
    write(repo.path(), "loose.txt", "l\n");
    assert!(zemacs_git::diff(repo.path(), Path::new("loose.txt"), false)
        .unwrap()
        .is_empty());
}

#[test]
fn a_conflicted_merge_shows_up_as_unmerged_and_in_progress() {
    let repo = init("conflict");
    write(repo.path(), "a.txt", "base\n");
    commit_all(repo.path(), "first");
    git(repo.path(), &["checkout", "-q", "-b", "other"]);
    write(repo.path(), "a.txt", "theirs\n");
    commit_all(repo.path(), "theirs");
    git(repo.path(), &["checkout", "-q", "main"]);
    write(repo.path(), "a.txt", "ours\n");
    commit_all(repo.path(), "ours");
    assert!(
        !git_try(repo.path(), &["merge", "other"]),
        "merge should conflict"
    );

    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.in_progress, Some(InProgress::Merge));
    assert!(status.has_conflicts());
    assert_eq!(
        status.unstaged,
        vec![FileChange {
            path: "a.txt".into(),
            status: ChangeKind::Unmerged
        }]
    );

    let (text, map) = zemacs_git::render(&status);
    assert_eq!(text.lines().count(), map.len());
    assert!(text.contains("State:    merge in progress"), "{text}");
    assert!(text.contains("U a.txt"), "{text}");

    // Staging a conflicted file is how it gets marked resolved.
    write(repo.path(), "a.txt", "resolved\n");
    zemacs_git::stage(repo.path(), Path::new("a.txt")).unwrap();
    let status = zemacs_git::status(repo.path()).unwrap();
    assert!(!status.has_conflicts());
    assert_eq!(status.staged[0].status, ChangeKind::Modified);
}

#[test]
fn push_and_pull_against_a_local_bare_remote() {
    let bare = Temp::new("bare");
    git(bare.path(), &["init", "-q", "--bare", "."]);
    // So a clone checks out the branch we actually push.
    git(bare.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let origin = bare.path().to_str().unwrap();

    let repo = init("push");
    write(repo.path(), "a.txt", "one\n");
    zemacs_git::stage_all(repo.path()).unwrap();
    zemacs_git::commit(repo.path(), "first").unwrap();

    // No remote at all: an error carrying git's own words, not a silent no-op.
    let err = zemacs_git::push(repo.path()).unwrap_err().to_string();
    assert!(err.contains("git push failed"), "{err}");

    // A remote but no upstream is still an error the user has to see.
    git(repo.path(), &["remote", "add", "origin", origin]);
    let err = zemacs_git::push(repo.path()).unwrap_err().to_string();
    assert!(err.contains("upstream"), "{err}");

    // The first `push -u` is deliberately outside this crate's API.
    git(repo.path(), &["push", "-q", "-u", "origin", "main"]);
    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.upstream.as_deref(), Some("origin/main"));
    assert_eq!((status.ahead, status.behind), (0, 0));

    write(repo.path(), "b.txt", "two\n");
    zemacs_git::stage_all(repo.path()).unwrap();
    zemacs_git::commit(repo.path(), "second").unwrap();
    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!((status.ahead, status.behind), (1, 0));
    let (text, _) = zemacs_git::render(&status);
    assert!(text.contains("Upstream: origin/main (ahead 1)"), "{text}");

    let summary = zemacs_git::push(repo.path()).unwrap();
    assert!(!summary.is_empty());
    assert_eq!(
        git(bare.path(), &["rev-parse", "main"]).trim(),
        git(repo.path(), &["rev-parse", "HEAD"]).trim()
    );
    assert_eq!(zemacs_git::status(repo.path()).unwrap().ahead, 0);

    // A clone of the same bare repo pulls the next commit down.
    let clone = Temp::new("clone");
    git(clone.path(), &["clone", "-q", origin, "."]);
    git(
        clone.path(),
        &["config", "user.email", "test@zemacs.invalid"],
    );
    git(clone.path(), &["config", "user.name", "zemacs test"]);

    write(repo.path(), "c.txt", "three\n");
    zemacs_git::stage_all(repo.path()).unwrap();
    zemacs_git::commit(repo.path(), "third").unwrap();
    zemacs_git::push(repo.path()).unwrap();

    let status = zemacs_git::status(clone.path()).unwrap();
    assert_eq!(status.upstream.as_deref(), Some("origin/main"));

    let summary = zemacs_git::pull(clone.path()).unwrap();
    assert!(
        clone.path().join("c.txt").exists(),
        "pull did not bring the commit down: {summary}"
    );
    assert_eq!(log_count(clone.path()), 3);
    assert!(zemacs_git::status(clone.path()).unwrap().is_clean());
}
