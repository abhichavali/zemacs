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

use zemacs_git::{
    Action, ChangeKind, Face, FileChange, InProgress, Line, RebaseOutcome, Section, Span, Status,
    TodoItem, View,
};

// ------------------------------------------------------------- scaffolding

/// Nothing here can run without the `git` binary, and a machine that has not
/// got one has nothing to fail about — so the interactive tests return instead
/// of exploding.
fn no_git() -> bool {
    Command::new("git").arg("--version").output().is_err()
}

/// Render a status with nothing folded and no diff open, which is what every
/// test that predates folding assumes.
fn draw(status: &Status) -> (String, Vec<Line>, Vec<Span>) {
    zemacs_git::render(&View::of(status.clone()))
}

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
    let (text, map, _) = draw(&status);
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

    let (text, map, _) = draw(&status);
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
    let (text, map, _) = draw(&status);
    assert_eq!(text.lines().count(), map.len());
    assert!(text.contains("no commits yet"), "{text}");
    assert!(text.contains("Nothing to commit"), "{text}");

    write(repo.path(), "tracked.txt", "one\n");
    commit_all(repo.path(), "first");

    // Clean, with a commit.
    let (text, map, _) = draw(&zemacs_git::status(repo.path()).unwrap());
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
    let (text, map, _) = draw(&status);
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
    let (text, map, _) = draw(&status);
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
    let (text, map, _) = draw(&status);
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

    let (text, map, _) = draw(&status);
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

    // `push_upstream` is the one that configures it; this is the same thing by
    // hand, so that this test goes on testing plain `push`.
    git(repo.path(), &["push", "-q", "-u", "origin", "main"]);
    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.upstream.as_deref(), Some("origin/main"));
    assert_eq!((status.ahead, status.behind), (0, 0));

    write(repo.path(), "b.txt", "two\n");
    zemacs_git::stage_all(repo.path()).unwrap();
    zemacs_git::commit(repo.path(), "second").unwrap();
    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!((status.ahead, status.behind), (1, 0));
    let (text, ..) = draw(&status);
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

// ------------------------------------------------------------------ colour

/// Sorted, disjoint, inside the text, and never across a line break — the four
/// things a renderer walking spans and text together assumes and cannot check.
fn check_spans(text: &str, spans: &[Span]) {
    let chars = text.chars().count();
    let mut previous = 0;
    for span in spans {
        assert!(span.start >= previous, "spans out of order at {span:?}");
        assert!(span.start < span.end, "empty or inverted span {span:?}");
        assert!(span.end <= chars, "span {span:?} past the end of {chars}");
        assert!(
            !slice(text, span).contains('\n'),
            "span {span:?} crosses a line"
        );
        previous = span.end;
    }
}

/// What a span covers. By characters, which is the only way its offsets mean
/// anything, and the reason a test can assert a word rather than a number.
fn slice(text: &str, span: &Span) -> String {
    text.chars()
        .skip(span.start)
        .take(span.end - span.start)
        .collect()
}

/// Every span as the text it covers and its face.
fn coloured(status: &Status) -> Vec<(String, Face)> {
    let (text, _, spans) = draw(status);
    check_spans(&text, &spans);
    spans.iter().map(|s| (slice(&text, s), s.kind)).collect()
}

/// `assert!(faces.contains(...))` with a message that shows what was there.
fn assert_coloured(faces: &[(String, Face)], text: &str, kind: Face) {
    assert!(
        faces.iter().any(|(t, k)| t == text && *k == kind),
        "expected {text:?} as {kind:?}, got {faces:#?}"
    );
}

#[test]
fn each_section_and_its_files_share_a_face() {
    let repo = init("colour-sections");
    write(repo.path(), "tracked.txt", "one\n");
    commit_all(repo.path(), "first");
    write(repo.path(), "tracked.txt", "one\ntwo\n");
    write(repo.path(), "staged.txt", "s\n");
    git(repo.path(), &["add", "staged.txt"]);
    write(repo.path(), "loose.txt", "l\n");

    let faces = coloured(&zemacs_git::status(repo.path()).unwrap());

    // The heading and the rows under it are one colour, so a glance at the
    // buffer says which side of the index a file is on.
    assert_coloured(&faces, "Staged changes", Face::String);
    assert_coloured(&faces, "staged.txt", Face::String);
    assert_coloured(&faces, "Unstaged changes", Face::Keyword);
    assert_coloured(&faces, "tracked.txt", Face::Keyword);
    assert_coloured(&faces, "Untracked files", Face::Comment);
    assert_coloured(&faces, "loose.txt", Face::Comment);

    // Counts are counts wherever they appear, and the status letter is a mark.
    assert_coloured(&faces, "(1)", Face::Number);
    for letter in ["A", "M", "?"] {
        assert_coloured(&faces, letter, Face::Constant);
    }
}

#[test]
fn the_header_separates_its_labels_from_their_values() {
    let bare = Temp::new("colour-header-bare");
    git(bare.path(), &["init", "-q", "--bare", "."]);
    git(bare.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let repo = init("colour-header");
    write(repo.path(), "a.txt", "a\n");
    commit_all(repo.path(), "first");
    let origin = bare.path().to_str().unwrap();
    git(repo.path(), &["remote", "add", "origin", origin]);
    git(repo.path(), &["push", "-q", "-u", "origin", "main"]);
    write(repo.path(), "b.txt", "b\n");
    commit_all(repo.path(), "second");

    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.ahead, 1);
    let faces = coloured(&status);

    // The labels are furniture and the values are the news, so they part ways.
    assert_coloured(&faces, "Head:", Face::Comment);
    assert_coloured(&faces, "main", Face::Heading1);
    assert_coloured(&faces, "Upstream:", Face::Comment);
    assert_coloured(&faces, "origin/main", Face::Link);
    assert_coloured(&faces, "(ahead 1)", Face::Number);
}

#[test]
fn a_conflict_and_a_half_finished_merge_are_the_loudest_things_on_the_screen() {
    let repo = init("colour-conflict");
    write(repo.path(), "shared.txt", "base\n");
    commit_all(repo.path(), "first");
    git(repo.path(), &["checkout", "-q", "-b", "other"]);
    write(repo.path(), "shared.txt", "theirs\n");
    commit_all(repo.path(), "theirs");
    git(repo.path(), &["checkout", "-q", "main"]);
    write(repo.path(), "shared.txt", "ours\n");
    commit_all(repo.path(), "ours");
    assert!(
        !git_try(repo.path(), &["merge", "other"]),
        "merge should conflict"
    );

    let status = zemacs_git::status(repo.path()).unwrap();
    assert!(status.has_conflicts() && status.in_progress == Some(InProgress::Merge));

    let faces = coloured(&status);
    assert_coloured(&faces, "State:", Face::Comment);
    assert_coloured(&faces, "merge in progress", Face::Bold);
    // The conflicted path takes its own face rather than the section's, so it
    // does not read as just another unstaged change.
    assert_coloured(&faces, "shared.txt", Face::Bold);
    assert_coloured(&faces, "U", Face::Constant);
}

#[test]
fn a_rename_dims_the_old_name_and_colours_the_new_one() {
    let repo = init("colour-rename");
    let body = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
    write(repo.path(), "old.txt", body);
    commit_all(repo.path(), "first");
    fs::rename(repo.path().join("old.txt"), repo.path().join("new.txt")).unwrap();
    zemacs_git::stage_all(repo.path()).unwrap();

    let faces = coloured(&zemacs_git::status(repo.path()).unwrap());
    assert_coloured(&faces, "old.txt", Face::Comment);
    assert_coloured(&faces, " -> ", Face::Punctuation);
    assert_coloured(&faces, "new.txt", Face::String);
}

#[test]
fn a_clean_tree_still_says_so_in_colour() {
    let repo = init("colour-clean");
    write(repo.path(), "a.txt", "a\n");
    commit_all(repo.path(), "first");

    let faces = coloured(&zemacs_git::status(repo.path()).unwrap());
    assert_coloured(
        &faces,
        "Nothing to commit, working tree clean",
        Face::Comment,
    );
}

/// The whole reason the offsets are characters: git tracks paths as bytes, and
/// one non-ASCII name must not slide every span after it.
#[test]
fn span_offsets_are_characters_not_bytes() {
    let repo = init("colour-utf8");
    write(repo.path(), "src/café.rs", "fn main() {}\n");
    write(repo.path(), "日本語.txt", "x\n");
    write(repo.path(), "plain.txt", "x\n");

    let faces = coloured(&zemacs_git::status(repo.path()).unwrap());

    // `slice` counts characters, so these only come out whole if `render`
    // counted characters too — and `plain.txt`, which follows them, proves the
    // offsets did not drift by the four extra bytes ahead of it.
    assert_coloured(&faces, "src/café.rs", Face::Comment);
    assert_coloured(&faces, "日本語.txt", Face::Comment);
    assert_coloured(&faces, "plain.txt", Face::Comment);
    assert_coloured(&faces, "(3)", Face::Number);
}

// -------------------------------------------------------------- interactive

fn head(repo: &Path) -> String {
    git(repo, &["rev-parse", "HEAD"]).trim().to_string()
}

fn short(repo: &Path, rev: &str) -> String {
    git(repo, &["rev-parse", "--short", rev]).trim().to_string()
}

fn message(repo: &Path, rev: &str) -> String {
    git(repo, &["log", "-1", "--format=%B", rev])
        .trim()
        .to_string()
}

fn read(repo: &Path, rel: &str) -> String {
    fs::read_to_string(repo.join(rel)).unwrap()
}

/// The bytes of a blob git is holding — `":a.txt"` is a.txt as the index has it.
/// The only way to see what a partial stage actually put there, byte for byte,
/// rather than what a diff of it says.
fn blob(repo: &Path, spec: &str) -> String {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["show", spec])
        .output()
        .unwrap();
    assert!(out.status.success(), "git show {spec} failed");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Three commits, each adding a file of its own, so nothing conflicts with
/// anything and a rebase can be about the ordering rather than the content.
fn three(tag: &str) -> Temp {
    let repo = init(tag);
    for name in ["first", "second", "third"] {
        write(repo.path(), &format!("{name}.txt"), &format!("{name}\n"));
        commit_all(repo.path(), name);
    }
    repo
}

/// Twenty lines with two far-apart edits: two hunks, because the three lines of
/// context around each never meet.
fn two_hunks(tag: &str) -> Temp {
    let repo = init(tag);
    let base: String = (1..=20).map(|n| format!("line {n}\n")).collect();
    write(repo.path(), "a.txt", &base);
    commit_all(repo.path(), "first");
    let edited = base
        .replace("line 2\n", "LINE TWO\n")
        .replace("line 18\n", "LINE EIGHTEEN\n");
    write(repo.path(), "a.txt", &edited);
    repo
}

#[test]
fn a_plan_names_the_commits_it_will_replay_oldest_first() {
    if no_git() {
        return;
    }
    let repo = three("plan");

    let plan = zemacs_git::plan_last(repo.path(), 2).unwrap();
    assert_eq!(
        plan.base.as_deref(),
        Some(git(repo.path(), &["rev-parse", "HEAD~2"]).trim())
    );
    let subjects: Vec<&str> = plan.todo.iter().map(|i| i.subject.as_str()).collect();
    assert_eq!(subjects, vec!["second", "third"]);
    assert!(plan.todo.iter().all(|i| i.action == Action::Pick));

    // Asking for more commits than exist reaches the root, which git spells
    // `--root` rather than as a revision.
    let plan = zemacs_git::plan_last(repo.path(), 9).unwrap();
    assert_eq!(plan.base, None);
    assert_eq!(plan.todo.len(), 3);

    let plan = zemacs_git::plan_onto(repo.path(), "HEAD~2").unwrap();
    assert_eq!(plan.todo.len(), 2);
    assert!(zemacs_git::plan_onto(repo.path(), "HEAD").is_err());
}

/// The point of writing the todo file ourselves: git does what the file says
/// and no editor is ever launched.
#[test]
fn a_reordered_todo_list_reorders_the_branch() {
    if no_git() {
        return;
    }
    let repo = three("reorder");
    let mut plan = zemacs_git::plan_last(repo.path(), 2).unwrap();
    plan.todo.swap(0, 1);

    match zemacs_git::rebase_start(repo.path(), &plan).unwrap() {
        RebaseOutcome::Done(_) => {}
        other => panic!("expected a clean rebase, got {other:?}"),
    }
    assert_eq!(log_count(repo.path()), 3);
    assert_eq!(message(repo.path(), "HEAD"), "second");
    assert_eq!(message(repo.path(), "HEAD~1"), "third");
    // Nothing is half-finished, and every file is still there.
    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.in_progress, None);
    assert_eq!(status.rebase, None);
    assert!(status.is_clean());
    assert!(repo.path().join("second.txt").exists());
}

/// A conflict is the normal outcome of reordering commits, so it comes back as
/// a state to render rather than as an error to report.
#[test]
fn a_rebase_that_conflicts_is_stopped_and_not_an_error() {
    if no_git() {
        return;
    }
    let repo = init("rebase-conflict");
    write(repo.path(), "a.txt", "one\n");
    commit_all(repo.path(), "one");
    write(repo.path(), "a.txt", "two\n");
    commit_all(repo.path(), "two");
    write(repo.path(), "a.txt", "three\n");
    commit_all(repo.path(), "three");
    let before = head(repo.path());

    // Replaying "three" before "two" cannot apply: it expects "two" to be there.
    let mut plan = zemacs_git::plan_last(repo.path(), 2).unwrap();
    plan.todo.swap(0, 1);
    let RebaseOutcome::Stopped { rebase, .. } =
        zemacs_git::rebase_start(repo.path(), &plan).unwrap()
    else {
        panic!("expected the rebase to stop on the conflict");
    };
    assert!(rebase.is_conflicted());
    assert_eq!(rebase.conflicts, vec![PathBuf::from("a.txt")]);
    assert_eq!(rebase.branch.as_deref(), Some("main"));
    assert!(rebase.total >= 2);

    // ...and the status buffer says the same thing, from the repository alone.
    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.in_progress, Some(InProgress::Rebase));
    assert!(status.has_conflicts());
    let rebase = status.rebase.clone().unwrap();
    assert_eq!(rebase.conflicts, vec![PathBuf::from("a.txt")]);
    let (text, map, spans) = draw(&status);
    assert_eq!(text.lines().count(), map.len());
    check_spans(&text, &spans);
    assert!(text.contains("State:    rebase in progress"), "{text}");
    assert!(text.contains("Rebase:   main onto "), "{text}");
    assert!(text.contains("conflicted)"), "{text}");
    assert!(text.contains("U a.txt"), "{text}");

    // Aborting is the way out, and it puts everything back.
    zemacs_git::rebase_abort(repo.path()).unwrap();
    assert_eq!(head(repo.path()), before);
    assert_eq!(read(repo.path(), "a.txt"), "three\n");
    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.in_progress, None);
    assert_eq!(status.rebase, None);
    assert!(status.is_clean());
    assert_eq!(log_count(repo.path()), 3);
}

/// The other kind of stop: an `edit` step, which is waiting rather than
/// complaining, and says so by having no conflicts.
#[test]
fn an_edit_step_stops_the_rebase_without_conflicting() {
    if no_git() {
        return;
    }
    let repo = three("rebase-edit");
    let mut plan = zemacs_git::plan_last(repo.path(), 2).unwrap();
    plan.todo[0].action = Action::Edit;

    let RebaseOutcome::Stopped { rebase, .. } =
        zemacs_git::rebase_start(repo.path(), &plan).unwrap()
    else {
        panic!("an edit step has to stop");
    };
    assert!(!rebase.is_conflicted());
    assert_eq!(rebase.stopped.as_ref().unwrap().subject, "second");
    // One step still to replay, and the buffer can show it.
    assert_eq!(rebase.todo.len(), 1);
    assert_eq!(rebase.todo[0].subject, "third");
    let (text, map, _) = draw(&zemacs_git::status(repo.path()).unwrap());
    assert_eq!(text.lines().count(), map.len());
    assert!(text.contains("Rebase todo (1)"), "{text}");
    assert!(map.contains(&Line::Todo { index: 0 }), "{map:#?}");

    assert!(matches!(
        zemacs_git::rebase_continue(repo.path()).unwrap(),
        RebaseOutcome::Done(_)
    ));
    assert_eq!(log_count(repo.path()), 3);
    assert_eq!(zemacs_git::status(repo.path()).unwrap().rebase, None);
}

/// `c f` then `r f`: a `fixup!` commit, and the autosquash that folds it into
/// the commit it names. git writes the todo list itself, so the assertion is
/// on what is left — two commits, the fixup gone, its change in the right one.
#[test]
fn a_fixup_commit_is_folded_in_by_autosquash() {
    if no_git() {
        return;
    }
    let repo = three("fixup");
    let second = short(repo.path(), "HEAD~1");

    write(repo.path(), "second.txt", "second, corrected\n");
    git(repo.path(), &["add", "second.txt"]);
    zemacs_git::commit_fixup(repo.path(), &second).unwrap();
    assert_eq!(log_count(repo.path()), 4);
    assert_eq!(message(repo.path(), "HEAD"), "fixup! second");

    // ...with an unrelated edit still in the tree, which git would otherwise
    // refuse to rebase over; `--autostash` carries it across.
    write(repo.path(), "loose.txt", "not yet\n");
    assert!(matches!(
        zemacs_git::rebase_autosquash(repo.path(), "HEAD~3").unwrap(),
        RebaseOutcome::Done(_)
    ));
    assert_eq!(log_count(repo.path()), 3);
    assert_eq!(message(repo.path(), "HEAD~1"), "second");
    assert_eq!(
        fs::read_to_string(repo.path().join("second.txt")).unwrap(),
        "second, corrected\n"
    );
    assert!(repo.path().join("loose.txt").exists(), "the stash came back");

    // Nothing staged is nothing to fix up with.
    assert!(zemacs_git::commit_fixup(repo.path(), "HEAD").is_err());
}

/// `r i` starts from a commit and reaches HEAD, oldest first — the list an
/// editor is opened on — and refuses a commit that is not on the branch.
#[test]
fn an_interactive_plan_runs_from_a_commit_to_head() {
    if no_git() {
        return;
    }
    let repo = three("plan-from");
    let plan = zemacs_git::plan_from(repo.path(), &short(repo.path(), "HEAD~1")).unwrap();
    assert_eq!(plan.base.as_deref(), Some(git(repo.path(), &["rev-parse", "HEAD~2"]).trim()));
    let subjects: Vec<&str> = plan.todo.iter().map(|t| t.subject.as_str()).collect();
    assert_eq!(subjects, ["second", "third"]);
    assert!(plan.todo.iter().all(|t| t.action == Action::Pick));

    // From the root: no base, and git spells that `--root`.
    let root = zemacs_git::plan_from(repo.path(), &short(repo.path(), "HEAD~2")).unwrap();
    assert_eq!(root.base, None);
    assert_eq!(root.todo.len(), 3);

    zemacs_git::branch_create(repo.path(), "aside", Some("HEAD~2")).unwrap();
    git(repo.path(), &["checkout", "-q", "aside"]);
    write(repo.path(), "aside.txt", "a\n");
    commit_all(repo.path(), "aside");
    let err = zemacs_git::plan_from(repo.path(), &short(repo.path(), "main"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("not on the current branch"), "{err}");
}

/// What a revision prompt completes over: heads, then remotes, then tags, and
/// never `origin/HEAD`, which is a branch already in the list under its name.
#[test]
fn refs_lists_branches_remotes_and_tags_in_that_order() {
    if no_git() {
        return;
    }
    let repo = init("refs");
    write(repo.path(), "a.txt", "a\n");
    commit_all(repo.path(), "first");
    zemacs_git::branch_create(repo.path(), "feature", None).unwrap();
    zemacs_git::tag(repo.path(), "v1", "HEAD").unwrap();

    let remote = Temp::new("refs-remote");
    git(remote.path(), &["init", "-q", "--bare", "."]);
    git(repo.path(), &["remote", "add", "origin", remote.path().to_str().unwrap()]);
    git(repo.path(), &["push", "-q", "origin", "main"]);
    git(repo.path(), &["remote", "set-head", "origin", "main"]);

    let refs = zemacs_git::refs(repo.path()).unwrap();
    assert_eq!(refs, ["feature", "main", "origin/main", "v1"], "{refs:?}");

    zemacs_git::tag_delete(repo.path(), "v1").unwrap();
    assert!(!zemacs_git::refs(repo.path()).unwrap().contains(&"v1".to_string()));

    zemacs_git::branch_rename(repo.path(), "trunk").unwrap();
    assert_eq!(
        zemacs_git::status(repo.path()).unwrap().branch.as_deref(),
        Some("trunk")
    );
    assert!(zemacs_git::fetch_all(repo.path()).is_ok());
}

#[test]
fn squashing_a_commit_into_its_parent_leaves_one_commit_and_both_messages() {
    if no_git() {
        return;
    }
    let repo = three("squash");
    let third = short(repo.path(), "HEAD");

    assert!(matches!(
        zemacs_git::squash(repo.path(), &third).unwrap(),
        RebaseOutcome::Done(_)
    ));
    assert_eq!(log_count(repo.path()), 2);
    let combined = message(repo.path(), "HEAD");
    assert!(
        combined.contains("second") && combined.contains("third"),
        "{combined}"
    );
    // Both files survive the fold; only the commits were merged.
    assert!(repo.path().join("second.txt").exists());
    assert!(repo.path().join("third.txt").exists());

    // The root commit has nothing to squash into, and says so.
    let err = zemacs_git::squash(repo.path(), &short(repo.path(), "HEAD~1"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("nothing to squash into"), "{err}");
}

#[test]
fn dropping_a_commit_removes_it_and_keeps_the_rest() {
    if no_git() {
        return;
    }
    let repo = three("drop");
    let second = short(repo.path(), "HEAD~1");

    assert!(matches!(
        zemacs_git::drop_commit(repo.path(), &second).unwrap(),
        RebaseOutcome::Done(_)
    ));
    assert_eq!(log_count(repo.path()), 2);
    assert!(!repo.path().join("second.txt").exists());
    assert!(repo.path().join("first.txt").exists());
    assert!(repo.path().join("third.txt").exists());
}

#[test]
fn rewording_changes_the_message_and_nothing_else() {
    if no_git() {
        return;
    }
    let repo = three("reword");
    let second = short(repo.path(), "HEAD~1");

    assert!(matches!(
        zemacs_git::reword(repo.path(), &second, "second, better said").unwrap(),
        RebaseOutcome::Done(_)
    ));
    assert_eq!(log_count(repo.path()), 3);
    assert_eq!(message(repo.path(), "HEAD~1"), "second, better said");
    assert_eq!(message(repo.path(), "HEAD"), "third");
    assert!(zemacs_git::status(repo.path()).unwrap().is_clean());

    // Rewording the tip must not quietly commit whatever happens to be staged:
    // the user asked to change a sentence, not to make a commit.
    write(repo.path(), "staged.txt", "s\n");
    zemacs_git::stage_all(repo.path()).unwrap();
    zemacs_git::reword(repo.path(), "HEAD", "third, better said").unwrap();
    assert_eq!(message(repo.path(), "HEAD"), "third, better said");
    assert_eq!(log_count(repo.path()), 3);
    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.staged[0].path, PathBuf::from("staged.txt"));

    assert!(zemacs_git::reword(repo.path(), "HEAD", "  ").is_err());
}

#[test]
fn amending_folds_the_index_into_the_last_commit() {
    if no_git() {
        return;
    }
    let repo = init("amend");
    write(repo.path(), "a.txt", "a\n");
    commit_all(repo.path(), "first");
    write(repo.path(), "b.txt", "b\n");
    zemacs_git::stage_all(repo.path()).unwrap();

    zemacs_git::amend(repo.path(), None).unwrap();
    assert_eq!(log_count(repo.path()), 1);
    assert_eq!(message(repo.path(), "HEAD"), "first");
    assert!(git(repo.path(), &["show", "--name-only", "--format=", "HEAD"]).contains("b.txt"));

    zemacs_git::amend(repo.path(), Some("first, with both")).unwrap();
    assert_eq!(message(repo.path(), "HEAD"), "first, with both");
    assert!(zemacs_git::amend(repo.path(), Some(" ")).is_err());
}

// -------------------------------------------------------------------- hunks

#[test]
fn staging_one_hunk_stages_only_that_hunk() {
    if no_git() {
        return;
    }
    let repo = two_hunks("hunk-stage");

    let diff = zemacs_git::file_diff(repo.path(), Path::new("a.txt"), Section::Unstaged).unwrap();
    assert_eq!(diff.hunks.len(), 2, "{diff:#?}");
    zemacs_git::stage_hunk(repo.path(), &diff, 0).unwrap();

    let staged = zemacs_git::diff(repo.path(), Path::new("a.txt"), true).unwrap();
    assert!(staged.contains("+LINE TWO"), "{staged}");
    assert!(!staged.contains("LINE EIGHTEEN"), "{staged}");
    let unstaged = zemacs_git::diff(repo.path(), Path::new("a.txt"), false).unwrap();
    assert!(unstaged.contains("+LINE EIGHTEEN"), "{unstaged}");
    assert!(!unstaged.contains("LINE TWO"), "{unstaged}");
    // The working tree kept both edits the whole time.
    assert!(read(repo.path(), "a.txt").contains("LINE TWO"));

    // And back out again, one hunk at a time.
    let diff = zemacs_git::file_diff(repo.path(), Path::new("a.txt"), Section::Staged).unwrap();
    assert_eq!(diff.hunks.len(), 1);
    zemacs_git::unstage_hunk(repo.path(), &diff, 0).unwrap();
    assert!(zemacs_git::status(repo.path()).unwrap().staged.is_empty());
    assert!(read(repo.path(), "a.txt").contains("LINE TWO"));

    // A staged diff is not a worktree diff; applying one as the other would
    // half-succeed, so it is refused.
    let staged = zemacs_git::file_diff(repo.path(), Path::new("a.txt"), Section::Unstaged).unwrap();
    assert!(zemacs_git::unstage_hunk(repo.path(), &staged, 0).is_err());
    assert!(zemacs_git::stage_hunk(repo.path(), &staged, 7).is_err());
}

#[test]
fn discarding_a_hunk_throws_away_only_that_hunk() {
    if no_git() {
        return;
    }
    let repo = two_hunks("hunk-discard");
    let diff = zemacs_git::file_diff(repo.path(), Path::new("a.txt"), Section::Unstaged).unwrap();

    zemacs_git::discard_hunk(repo.path(), &diff, 1).unwrap();
    let body = read(repo.path(), "a.txt");
    assert!(body.contains("LINE TWO"), "{body}");
    assert!(!body.contains("LINE EIGHTEEN"), "{body}");
    assert!(body.contains("line 18"), "{body}");
}

// ------------------------------------------------------- part of one hunk
//
// Every test below makes the same shape of claim, and it is the only claim
// worth making about a patch rewriter: *exactly* these lines moved to the index
// and *exactly* those did not. `git apply` reports nothing when it stages the
// complement of what was asked for — it is a patch, and it applies — so an
// assertion that the call returned `Ok` would pass for the bug that matters.

/// Just the `+`/`-` lines of a diff, in order: what changed, with nothing about
/// where. The preamble's `---`/`+++` are behind the first `@@` and so are gone
/// before the filter sees them.
fn changes(diff: &str) -> Vec<String> {
    diff.lines()
        .skip_while(|l| !l.starts_with("@@"))
        .filter(|l| l.starts_with('+') || l.starts_with('-'))
        .map(|l| l.to_string())
        .collect()
}

/// Both sides of the index for one file: `(staged, unstaged)`.
fn sides(repo: &Path, rel: &str) -> (Vec<String>, Vec<String>) {
    let path = Path::new(rel);
    (
        changes(&zemacs_git::diff(repo, path, true).unwrap()),
        changes(&zemacs_git::diff(repo, path, false).unwrap()),
    )
}

/// The body-line numbers of the lines reading `wanted`, in the numbering
/// `Line::Hunk`'s `line` uses and the staging functions take. Named by their
/// text so a test says which lines it means rather than counting them.
fn at(diff: &zemacs_git::FileDiff, hunk: usize, wanted: &[&str]) -> Vec<usize> {
    let body: Vec<&str> = diff.hunks[hunk].body.lines().collect();
    wanted
        .iter()
        .map(|w| {
            body.iter()
                .position(|line| line == w)
                .unwrap_or_else(|| panic!("no {w:?} in {body:#?}"))
        })
        .collect()
}

/// A repository holding `a.txt` committed as `first` and then edited to
/// `second`, which is one unstaged file with one change to pick lines out of.
fn edited(tag: &str, first: &str, second: &str) -> Temp {
    let repo = init(tag);
    write(repo.path(), "a.txt", first);
    commit_all(repo.path(), "first");
    write(repo.path(), "a.txt", second);
    repo
}

fn unstaged_diff(repo: &Path) -> zemacs_git::FileDiff {
    zemacs_git::file_diff(repo, Path::new("a.txt"), Section::Unstaged).unwrap()
}

fn staged_diff(repo: &Path) -> zemacs_git::FileDiff {
    zemacs_git::file_diff(repo, Path::new("a.txt"), Section::Staged).unwrap()
}

#[test]
fn staging_some_added_lines_leaves_the_others_unstaged() {
    if no_git() {
        return;
    }
    let repo = edited("part-add", "head\ntail\n", "head\nadd1\nadd2\nadd3\ntail\n");
    let diff = unstaged_diff(repo.path());
    assert_eq!(diff.hunks.len(), 1, "{diff:#?}");

    zemacs_git::stage_lines(repo.path(), &diff, 0, &at(&diff, 0, &["+add2"])).unwrap();

    let (staged, unstaged) = sides(repo.path(), "a.txt");
    assert_eq!(staged, ["+add2"]);
    assert_eq!(unstaged, ["+add1", "+add3"]);
    // The working tree never moves: staging copies into the index.
    assert_eq!(read(repo.path(), "a.txt"), "head\nadd1\nadd2\nadd3\ntail\n");
}

#[test]
fn staging_some_removed_lines_leaves_the_others_in_the_index() {
    if no_git() {
        return;
    }
    let repo = edited("part-del", "one\ntwo\nthree\nfour\nfive\n", "one\nfive\n");
    let diff = unstaged_diff(repo.path());

    zemacs_git::stage_lines(repo.path(), &diff, 0, &at(&diff, 0, &["-three"])).unwrap();

    // The two deletions nobody picked had to become *context*, not vanish: had
    // they been dropped the patch would still apply and would still be one line
    // shorter than the index it was built against.
    let (staged, unstaged) = sides(repo.path(), "a.txt");
    assert_eq!(staged, ["-three"]);
    assert_eq!(unstaged, ["-two", "-four"]);
}

#[test]
fn staging_part_of_a_mixed_hunk_moves_both_halves_of_the_change() {
    if no_git() {
        return;
    }
    let repo = edited(
        "part-mixed",
        "alpha\nbravo\ncharlie\ndelta\n",
        "ALPHA\nbravo\nCHARLIE\ndelta\n",
    );
    let diff = unstaged_diff(repo.path());

    let lines = at(&diff, 0, &["-charlie", "+CHARLIE"]);
    zemacs_git::stage_lines(repo.path(), &diff, 0, &lines).unwrap();

    let (staged, unstaged) = sides(repo.path(), "a.txt");
    assert_eq!(staged, ["-charlie", "+CHARLIE"]);
    assert_eq!(unstaged, ["-alpha", "+ALPHA"]);
}

/// A change on the first and last line of a hunk with no context on either end:
/// the two places an index that counts the `@@` header as a body line, or one
/// that stops a line early, would pick the wrong pair and say nothing about it.
#[test]
fn the_first_and_the_last_line_of_a_hunk_are_the_lines_they_look_like() {
    if no_git() {
        return;
    }
    let repo = edited("part-edges", "one\ntwo\nthree\n", "ONE\ntwo\nTHREE\n");
    let diff = unstaged_diff(repo.path());
    assert_eq!(diff.hunks[0].body.lines().nth(1), Some("-one"), "{diff:#?}");

    zemacs_git::stage_lines(repo.path(), &diff, 0, &at(&diff, 0, &["-one", "+ONE"])).unwrap();
    let (staged, unstaged) = sides(repo.path(), "a.txt");
    assert_eq!(staged, ["-one", "+ONE"]);
    assert_eq!(unstaged, ["-three", "+THREE"]);

    // ...and the other end, from scratch, so neither result can be the other's.
    let repo = edited("part-edges-2", "one\ntwo\nthree\n", "ONE\ntwo\nTHREE\n");
    let diff = unstaged_diff(repo.path());
    let last = at(&diff, 0, &["-three", "+THREE"]);
    assert_eq!(*last.last().unwrap(), diff.hunks[0].body.lines().count() - 1);
    zemacs_git::stage_lines(repo.path(), &diff, 0, &last).unwrap();
    let (staged, unstaged) = sides(repo.path(), "a.txt");
    assert_eq!(staged, ["-three", "+THREE"]);
    assert_eq!(unstaged, ["-one", "+ONE"]);
}

/// One line and no region is the same call with a one-element slice, which is
/// the gesture `s` makes when nothing is selected.
#[test]
fn staging_one_line_of_a_hunk_stages_that_line_and_no_other() {
    if no_git() {
        return;
    }
    let repo = edited(
        "part-one",
        "keep\ndrop me\nkeep2\n",
        "keep\nadded\nkeep2\nalso added\n",
    );
    let diff = unstaged_diff(repo.path());
    let lines = at(&diff, 0, &["+added"]);
    assert_eq!(lines.len(), 1);
    zemacs_git::stage_lines(repo.path(), &diff, 0, &lines).unwrap();

    let (staged, unstaged) = sides(repo.path(), "a.txt");
    assert_eq!(staged, ["+added"]);
    assert_eq!(unstaged, ["-drop me", "+also added"]);
}

/// The half that is easy to get backwards: unstaging reads the *new* side of
/// the patch as the pre-image, so the lines nobody picked swap roles. Get it
/// wrong and this test sees the complement staged, with no error anywhere.
#[test]
fn unstaging_some_lines_takes_out_those_and_leaves_the_rest_staged() {
    if no_git() {
        return;
    }
    let repo = edited("part-unstage", "one\ntwo\nthree\n", "ONE\ntwo\nTHREE\n");
    git(repo.path(), &["add", "-A"]);
    let diff = staged_diff(repo.path());

    let lines = at(&diff, 0, &["-one", "+ONE"]);
    zemacs_git::unstage_lines(repo.path(), &diff, 0, &lines).unwrap();

    let (staged, unstaged) = sides(repo.path(), "a.txt");
    assert_eq!(staged, ["-three", "+THREE"], "the other pair stays staged");
    assert_eq!(unstaged, ["-one", "+ONE"], "and this pair came back out");
    // The working tree is untouched by either direction.
    assert_eq!(read(repo.path(), "a.txt"), "ONE\ntwo\nTHREE\n");
}

/// **Destroys work**, and so does the test: the discarded line has to be gone
/// from the file on disk and the one beside it has to still be there.
#[test]
fn discarding_some_lines_reverts_those_and_leaves_the_rest_edited() {
    if no_git() {
        return;
    }
    let repo = edited("part-discard", "one\ntwo\nthree\n", "ONE\ntwo\nTHREE\n");
    let diff = unstaged_diff(repo.path());

    let lines = at(&diff, 0, &["-one", "+ONE"]);
    zemacs_git::discard_lines(repo.path(), &diff, 0, &lines).unwrap();

    assert_eq!(read(repo.path(), "a.txt"), "one\ntwo\nTHREE\n");
    assert_eq!(sides(repo.path(), "a.txt").1, ["-three", "+THREE"]);
}

/// A `\ No newline at end of file` describes the line above it. Keep it when
/// that line is dropped and git rejects the patch; drop it when that line stays
/// and the file quietly grows a newline it never had.
#[test]
fn a_file_with_no_final_newline_keeps_not_having_one() {
    if no_git() {
        return;
    }
    let repo = edited("part-nonl", "one\ntwo\nthree", "ONE\ntwo\nTHREE");
    let diff = unstaged_diff(repo.path());
    assert!(
        diff.hunks[0].body.contains("\\ No newline at end of file"),
        "{diff:#?}"
    );

    // The change *on* the unterminated line, so the marker travels with it.
    zemacs_git::stage_lines(repo.path(), &diff, 0, &at(&diff, 0, &["-three", "+THREE"])).unwrap();
    assert_eq!(blob(repo.path(), ":a.txt"), "one\ntwo\nTHREE");
    let (staged, unstaged) = sides(repo.path(), "a.txt");
    assert_eq!(staged, ["-three", "+THREE"]);
    assert_eq!(unstaged, ["-one", "+ONE"]);

    // And the change *above* it, where the marker has to survive its `-three`
    // becoming context and the unpicked `+THREE`'s copy has to go with it.
    let repo = edited("part-nonl-2", "one\ntwo\nthree", "ONE\ntwo\nTHREE");
    let diff = unstaged_diff(repo.path());
    zemacs_git::stage_lines(repo.path(), &diff, 0, &at(&diff, 0, &["-one", "+ONE"])).unwrap();
    assert_eq!(blob(repo.path(), ":a.txt"), "ONE\ntwo\nthree");
}

/// git carries a `\r` as content, so the rewriter has to as well: rewriting a
/// `-` to a space with anything that splits on lines would silently strip it
/// off every line it touched and stage a whole-file line-ending change.
#[test]
fn crlf_line_endings_survive_a_partial_stage() {
    if no_git() {
        return;
    }
    let repo = edited(
        "part-crlf",
        "one\r\ntwo\r\nthree\r\n",
        "ONE\r\ntwo\r\nTHREE\r\n",
    );
    let diff = unstaged_diff(repo.path());
    // git wrote the `\r`; `lines()` — which is how the status buffer draws a
    // body and how `at` finds one — is what would drop it.
    assert!(diff.hunks[0].body.contains("-three\r\n"), "{diff:#?}");

    let lines = at(&diff, 0, &["-three", "+THREE"]);
    zemacs_git::stage_lines(repo.path(), &diff, 0, &lines).unwrap();
    assert_eq!(blob(repo.path(), ":a.txt"), "one\r\ntwo\r\nTHREE\r\n");
}

/// A selection of nothing but context stages nothing, and has to say so: the
/// patch would apply, change nothing, and look exactly like success.
#[test]
fn a_selection_with_no_change_in_it_is_refused() {
    if no_git() {
        return;
    }
    let repo = edited("part-empty", "one\ntwo\nthree\n", "ONE\ntwo\nthree\n");
    let diff = unstaged_diff(repo.path());

    assert!(zemacs_git::stage_lines(repo.path(), &diff, 0, &[]).is_err());
    assert!(zemacs_git::stage_lines(repo.path(), &diff, 0, &at(&diff, 0, &[" two"])).is_err());
    // Out-of-range indices name no line and so select nothing.
    assert!(zemacs_git::stage_lines(repo.path(), &diff, 0, &[99]).is_err());
    assert!(zemacs_git::stage_lines(repo.path(), &diff, 7, &[1]).is_err());
    // ...and the wrong side of the index is refused before any of that.
    assert!(zemacs_git::unstage_lines(repo.path(), &diff, 0, &[1]).is_err());
    assert!(sides(repo.path(), "a.txt").0.is_empty(), "nothing staged");
}

/// The `@@` counts have to be recomputed, and a range that empties or fills has
/// to move its start by one — `-0,0` means "before the first line". Checked on
/// the patch itself, because a wrong count is what makes git reject a patch that
/// was otherwise right, and a right count on a wrong start is what makes it
/// accept one that is not.
#[test]
fn the_hunk_header_counts_what_the_body_now_holds() {
    if no_git() {
        return;
    }
    let repo = edited("part-header", "one\ntwo\nthree\nfour\nfive\n", "one\nfive\n");
    let diff = unstaged_diff(repo.path());
    let patch = diff
        .partial(0, &at(&diff, 0, &["-three"]), false)
        .unwrap();
    // Five lines in, four out: the two unpicked deletions are context now.
    assert!(patch.contains("@@ -1,5 +1,4 @@"), "{patch}");

    // Reversed, the same selection keeps the *new* side whole instead.
    let patch = diff.partial(0, &at(&diff, 0, &["-three"]), true).unwrap();
    assert!(patch.contains("@@ -1,3 +1,2 @@"), "{patch}");
}

/// The whole chain the UI walks, end to end: draw the status, find the row the
/// cursor would be sitting on, read the map, hand its `line` straight to the
/// stager. An off-by-one anywhere along it stages the line *next to* the one on
/// screen, which is a wrong commit and no error message.
#[test]
fn the_line_the_map_names_is_the_line_that_gets_staged() {
    if no_git() {
        return;
    }
    let repo = edited("part-map", "one\ntwo\nthree\n", "ONE\ntwo\nTHREE\n");
    let mut view = View::load(repo.path()).unwrap();
    view.toggle_file(repo.path(), Section::Unstaged, Path::new("a.txt"))
        .unwrap();
    let (text, map, _) = zemacs_git::render(&view);

    // The two rows a region over the second change would cover.
    let picked: Vec<usize> = ["-three", "+THREE"]
        .iter()
        .map(|wanted| text.lines().position(|l| l == *wanted).unwrap())
        .map(|row| match &map[row] {
            Line::Hunk { line, .. } => *line,
            other => panic!("{other:?} is not a hunk line"),
        })
        .collect();
    let Line::Hunk { path, section, index, .. } = &map[text.lines().position(|l| l == "+THREE").unwrap()]
    else {
        unreachable!()
    };

    let diff = view.diff_of(*section, path).unwrap();
    zemacs_git::stage_lines(repo.path(), diff, *index, &picked).unwrap();

    let (staged, unstaged) = sides(repo.path(), "a.txt");
    assert_eq!(staged, ["-three", "+THREE"]);
    assert_eq!(unstaged, ["-one", "+ONE"]);
}

/// The corner this deliberately does not handle, pinned so it stays *loud*.
///
/// A file being created or deleted has `/dev/null` on one side of its preamble,
/// and a partial patch leaves that side non-empty — which contradicts the
/// preamble. git refuses and touches nothing, which is the one outcome that is
/// allowed: the failure mode worth being afraid of here is a patch that applies
/// and stages something else.
#[test]
fn part_of_a_whole_file_being_added_or_removed_is_refused_and_changes_nothing() {
    if no_git() {
        return;
    }
    let repo = init("part-newfile");
    write(repo.path(), "a.txt", "p\nq\nr\n");
    commit_all(repo.path(), "first");

    write(repo.path(), "new.txt", "p\nq\nr\n");
    git(repo.path(), &["add", "new.txt"]);
    let diff = zemacs_git::file_diff(repo.path(), Path::new("new.txt"), Section::Staged).unwrap();
    let err = zemacs_git::unstage_lines(repo.path(), &diff, 0, &at(&diff, 0, &["+q"])).unwrap_err();
    assert!(format!("{err:#}").contains("depends on old contents"), "{err:#}");
    assert_eq!(blob(repo.path(), ":new.txt"), "p\nq\nr\n", "index untouched");

    fs::remove_file(repo.path().join("a.txt")).unwrap();
    let diff = unstaged_diff(repo.path());
    let err = zemacs_git::stage_lines(repo.path(), &diff, 0, &at(&diff, 0, &["-q"])).unwrap_err();
    assert!(format!("{err:#}").contains("still has contents"), "{err:#}");
    assert_eq!(blob(repo.path(), ":a.txt"), "p\nq\nr\n", "index untouched");
}

#[test]
fn an_open_file_shows_its_hunks_and_every_line_points_back_at_one() {
    if no_git() {
        return;
    }
    let repo = two_hunks("hunk-render");
    let mut view = View::load(repo.path()).unwrap();
    view.toggle_file(repo.path(), Section::Unstaged, Path::new("a.txt"))
        .unwrap();

    let (text, map, spans) = zemacs_git::render(&view);
    assert_eq!(text.lines().count(), map.len(), "{text}");
    check_spans(&text, &spans);
    assert!(text.contains("+LINE TWO"), "{text}");
    assert!(text.contains("@@ "), "{text}");

    // The cursor anywhere in the second hunk stages the second hunk.
    let at = text
        .lines()
        .position(|l| l == "+LINE EIGHTEEN")
        .expect("the second hunk should be on screen");
    let Line::Hunk {
        path,
        section,
        index,
        ..
    } = &map[at]
    else {
        panic!("{:?} is not a hunk line", map[at])
    };
    assert_eq!(path, Path::new("a.txt"));
    assert_eq!(*section, Section::Unstaged);
    let diff = view.diff_of(Section::Unstaged, path).unwrap();
    zemacs_git::stage_hunk(repo.path(), diff, *index).unwrap();
    let staged = zemacs_git::diff(repo.path(), Path::new("a.txt"), true).unwrap();
    assert!(
        staged.contains("+LINE EIGHTEEN") && !staged.contains("LINE TWO"),
        "{staged}"
    );

    // Shutting it again takes the hunks off the screen.
    view.toggle_file(repo.path(), Section::Unstaged, Path::new("a.txt"))
        .unwrap();
    assert!(!zemacs_git::render(&view).0.contains("LINE EIGHTEEN"));
    // A refresh keeps the folds and drops nothing else.
    view.collapsed.push(Section::Recent);
    view.refresh(repo.path()).unwrap();
    assert!(view.is_collapsed(Section::Recent));
    assert!(view.diffs.is_empty());

    // Untracked files have no diff to open.
    write(repo.path(), "loose.txt", "l\n");
    view.refresh(repo.path()).unwrap();
    assert!(view
        .toggle_file(repo.path(), Section::Untracked, Path::new("loose.txt"))
        .is_err());
}

// ------------------------------------------------------ everything else

#[test]
fn stashes_go_on_the_stack_show_up_in_the_status_and_come_back_off() {
    if no_git() {
        return;
    }
    let repo = init("stash");
    write(repo.path(), "a.txt", "one\n");
    commit_all(repo.path(), "first");
    write(repo.path(), "a.txt", "one\ntwo\n");

    zemacs_git::stash_push(repo.path(), "work in progress").unwrap();
    assert_eq!(read(repo.path(), "a.txt"), "one\n");
    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.stashes.len(), 1);
    assert_eq!(status.stashes[0].name, "stash@{0}");
    assert!(status.stashes[0].subject.contains("work in progress"));
    let (text, map, _) = draw(&status);
    assert_eq!(text.lines().count(), map.len());
    assert!(text.contains("Stashes (1)"), "{text}");
    assert!(map.contains(&Line::Stash {
        name: "stash@{0}".into()
    }));

    zemacs_git::stash_pop(repo.path(), "stash@{0}").unwrap();
    assert_eq!(read(repo.path(), "a.txt"), "one\ntwo\n");
    assert!(zemacs_git::status(repo.path()).unwrap().stashes.is_empty());

    zemacs_git::stash_push(repo.path(), "").unwrap();
    zemacs_git::stash_drop(repo.path(), "stash@{0}").unwrap();
    assert!(zemacs_git::status(repo.path()).unwrap().stashes.is_empty());
    assert_eq!(read(repo.path(), "a.txt"), "one\n");
}

#[test]
fn branches_are_made_moved_onto_and_deleted() {
    if no_git() {
        return;
    }
    let repo = init("branch");
    write(repo.path(), "a.txt", "a\n");
    commit_all(repo.path(), "first");

    zemacs_git::branch_create(repo.path(), "feature", None).unwrap();
    let list = zemacs_git::branches(repo.path()).unwrap();
    assert!(list.iter().any(|b| b.name == "main" && b.head), "{list:#?}");
    assert!(
        list.iter().any(|b| b.name == "feature" && !b.head),
        "{list:#?}"
    );

    zemacs_git::checkout(repo.path(), "feature").unwrap();
    assert_eq!(
        zemacs_git::status(repo.path()).unwrap().branch.as_deref(),
        Some("feature")
    );
    zemacs_git::checkout(repo.path(), "main").unwrap();
    zemacs_git::branch_delete(repo.path(), "feature").unwrap();

    // A branch carrying commits nobody else has is refused, until it is not.
    zemacs_git::checkout_new(repo.path(), "solo", None).unwrap();
    write(repo.path(), "b.txt", "b\n");
    commit_all(repo.path(), "only here");
    zemacs_git::checkout(repo.path(), "main").unwrap();
    assert!(zemacs_git::branch_delete(repo.path(), "solo").is_err());
    zemacs_git::branch_delete_force(repo.path(), "solo").unwrap();
    assert!(zemacs_git::branches(repo.path())
        .unwrap()
        .iter()
        .all(|b| b.name != "solo"));
}

#[test]
fn reset_moves_head_and_each_flavour_keeps_what_it_says() {
    if no_git() {
        return;
    }
    let repo = init("reset");
    write(repo.path(), "a.txt", "a\n");
    commit_all(repo.path(), "first");
    write(repo.path(), "b.txt", "b\n");
    commit_all(repo.path(), "second");
    let second = head(repo.path());

    zemacs_git::reset_soft(repo.path(), "HEAD~1").unwrap();
    assert_eq!(log_count(repo.path()), 1);
    assert_eq!(
        zemacs_git::status(repo.path()).unwrap().staged[0].path,
        PathBuf::from("b.txt")
    );

    zemacs_git::reset_mixed(repo.path(), "HEAD").unwrap();
    let status = zemacs_git::status(repo.path()).unwrap();
    assert!(status.staged.is_empty());
    assert_eq!(status.untracked, vec![PathBuf::from("b.txt")]);

    // Back to where we were, then all the way back with the working tree.
    zemacs_git::reset_hard(repo.path(), &second).unwrap();
    assert_eq!(log_count(repo.path()), 2);
    write(repo.path(), "a.txt", "edited\n");
    zemacs_git::reset_hard(repo.path(), "HEAD").unwrap();
    assert_eq!(read(repo.path(), "a.txt"), "a\n");
}

#[test]
fn cherry_pick_copies_a_commit_and_revert_undoes_one() {
    if no_git() {
        return;
    }
    let repo = init("pick");
    write(repo.path(), "a.txt", "a\n");
    commit_all(repo.path(), "first");
    git(repo.path(), &["checkout", "-q", "-b", "other"]);
    write(repo.path(), "b.txt", "b\n");
    commit_all(repo.path(), "on other");
    let picked = head(repo.path());
    git(repo.path(), &["checkout", "-q", "main"]);

    zemacs_git::cherry_pick(repo.path(), &picked).unwrap();
    assert!(repo.path().join("b.txt").exists());
    assert_eq!(log_count(repo.path()), 2);

    zemacs_git::revert(repo.path(), "HEAD").unwrap();
    assert!(!repo.path().join("b.txt").exists());
    // Undoing by committing, so history grows rather than shrinks.
    assert_eq!(log_count(repo.path()), 3);
}

#[test]
fn discard_throws_away_a_working_tree_change_and_an_untracked_file() {
    if no_git() {
        return;
    }
    let repo = init("discard");
    write(repo.path(), "a.txt", "committed\n");
    commit_all(repo.path(), "first");
    write(repo.path(), "a.txt", "edited\n");
    write(repo.path(), "loose.txt", "l\n");

    zemacs_git::discard(repo.path(), Path::new("a.txt")).unwrap();
    assert_eq!(read(repo.path(), "a.txt"), "committed\n");
    // Untracked files are a different verb on purpose, so this one left it.
    assert!(repo.path().join("loose.txt").exists());

    zemacs_git::discard_untracked(repo.path(), Path::new("loose.txt")).unwrap();
    assert!(!repo.path().join("loose.txt").exists());
    assert!(zemacs_git::status(repo.path()).unwrap().is_clean());
}

#[test]
fn the_recent_log_is_the_recent_log() {
    if no_git() {
        return;
    }
    let repo = three("log");
    let status = zemacs_git::status(repo.path()).unwrap();
    let subjects: Vec<&str> = status.recent.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, vec!["third", "second", "first"]);

    let (text, map, spans) = draw(&status);
    assert_eq!(text.lines().count(), map.len());
    check_spans(&text, &spans);
    assert!(text.contains("Recent commits (3)"), "{text}");
    let hash = status.recent[0].hash.clone();
    assert!(map.contains(&Line::Commit { hash }), "{map:#?}");
}

/// A todo file zemacs wrote has to be one git will read, and one zemacs can
/// read back — the reorder happens between those two, so both have to hold.
#[test]
fn a_todo_list_written_here_is_read_back_the_same() {
    let items = vec![
        TodoItem::pick("abc1234", "a subject with spaces"),
        TodoItem {
            action: Action::Squash,
            hash: "def5678".into(),
            subject: "another".into(),
        },
    ];
    let text = zemacs_git::write_todo(&items);
    assert_eq!(
        text,
        "pick abc1234 a subject with spaces\nsquash def5678 another\n"
    );
    assert_eq!(zemacs_git::parse_todo(&text).unwrap(), items);
}

// ------------------------------------------------- what the upstream has not

/// The two counts in the header say *how many*; these two sections say *which*,
/// which is what you actually look at before pushing.
#[test]
fn unpushed_and_unpulled_list_the_commits_the_counts_promise() {
    if no_git() {
        return;
    }
    let bare = Temp::new("ahead-bare");
    git(bare.path(), &["init", "-q", "--bare", "."]);
    git(bare.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let origin = bare.path().to_str().unwrap();

    let repo = init("ahead");
    write(repo.path(), "a.txt", "one\n");
    commit_all(repo.path(), "first");
    git(repo.path(), &["remote", "add", "origin", origin]);
    zemacs_git::push_upstream(repo.path(), "origin").unwrap();
    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.upstream.as_deref(), Some("origin/main"));
    // Level with the remote: no sections, and no `git log` was run for them.
    assert!(status.unpushed.is_empty() && status.unpulled.is_empty());

    // A commit here, and another one over there, so both sides have something.
    let clone = Temp::new("ahead-clone");
    git(clone.path(), &["clone", "-q", origin, "."]);
    git(clone.path(), &["config", "user.email", "t@zemacs.invalid"]);
    git(clone.path(), &["config", "user.name", "zemacs test"]);
    write(clone.path(), "b.txt", "two\n");
    commit_all(clone.path(), "theirs");
    git(clone.path(), &["push", "-q"]);

    write(repo.path(), "c.txt", "three\n");
    commit_all(repo.path(), "ours");
    zemacs_git::fetch(repo.path()).unwrap();

    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!((status.ahead, status.behind), (1, 1));
    assert_eq!(status.unpushed.len(), 1);
    assert_eq!(status.unpushed[0].subject, "ours");
    assert_eq!(status.unpulled.len(), 1);
    assert_eq!(status.unpulled[0].subject, "theirs");

    let (text, map, spans) = draw(&status);
    assert_eq!(text.lines().count(), map.len());
    check_spans(&text, &spans);
    assert!(text.contains("Unpulled from origin/main (1)"), "{text}");
    assert!(text.contains("Unmerged into origin/main (1)"), "{text}");
    // Every line of them is a commit, so `TAB`, `A` and `X` work there too.
    let hash = status.unpushed[0].hash.clone();
    assert!(map.contains(&Line::Commit { hash }), "{map:#?}");
}

/// The first push of a branch, and the one that overwrites what is there.
#[test]
fn push_sets_an_upstream_and_force_overwrites_a_rewritten_branch() {
    if no_git() {
        return;
    }
    let bare = Temp::new("force-bare");
    git(bare.path(), &["init", "-q", "--bare", "."]);
    git(bare.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let repo = init("force");
    write(repo.path(), "a.txt", "one\n");
    commit_all(repo.path(), "first");
    // Plain `push` cannot do this: there is no upstream to push to yet.
    assert!(zemacs_git::push(repo.path()).is_err());
    git(
        repo.path(),
        &["remote", "add", "origin", bare.path().to_str().unwrap()],
    );
    zemacs_git::push_upstream(repo.path(), "origin").unwrap();
    assert_eq!(
        zemacs_git::status(repo.path()).unwrap().upstream.as_deref(),
        Some("origin/main")
    );

    // Rewrite the commit that was pushed. A plain push is refused for it.
    zemacs_git::amend(repo.path(), Some("first, reworded")).unwrap();
    assert!(zemacs_git::push(repo.path()).is_err());
    zemacs_git::push_force(repo.path()).unwrap();
    assert_eq!(
        git(bare.path(), &["log", "-1", "--format=%s", "main"]).trim(),
        "first, reworded"
    );
}

// ------------------------------------------------------- opening a commit

#[test]
fn a_commit_opens_its_patch_underneath_and_shuts_again() {
    if no_git() {
        return;
    }
    let repo = three("show");
    let mut view = View::load(repo.path()).unwrap();
    let hash = view.status.recent[0].hash.clone();

    view.toggle_commit(repo.path(), &hash).unwrap();
    let (text, map, spans) = zemacs_git::render(&view);
    assert_eq!(text.lines().count(), map.len(), "{text}");
    check_spans(&text, &spans);
    assert!(text.contains("+third"), "{text}");
    // Nothing can be staged out of a commit, so its lines act on nothing.
    let after = map.iter().position(|l| *l == Line::Commit { hash: hash.clone() }).unwrap() + 1;
    assert_eq!(map[after], Line::Text);

    // It survives a refresh, and shuts on the second press.
    view.refresh(repo.path()).unwrap();
    assert!(zemacs_git::render(&view).0.contains("+third"));
    view.toggle_commit(repo.path(), &hash).unwrap();
    assert!(!zemacs_git::render(&view).0.contains("+third"));
}

#[test]
fn the_log_section_grows_when_asked_and_goes_back_to_ten() {
    if no_git() {
        return;
    }
    let repo = init("loglimit");
    for n in 0..12 {
        write(repo.path(), "a.txt", &format!("{n}\n"));
        commit_all(repo.path(), &format!("commit {n}"));
    }
    let mut view = View::load(repo.path()).unwrap();
    assert_eq!(view.status.recent.len(), 10);

    view.log_limit = 100;
    view.refresh(repo.path()).unwrap();
    assert_eq!(view.status.recent.len(), 12);
    assert!(zemacs_git::render(&view).0.contains("Recent commits (12)"));

    view.log_limit = 0;
    view.refresh(repo.path()).unwrap();
    assert_eq!(view.status.recent.len(), 10);
}

#[test]
fn a_commits_whole_message_comes_back_for_rewriting() {
    if no_git() {
        return;
    }
    let repo = init("message");
    write(repo.path(), "a.txt", "a\n");
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "subject", "-m", "a body"]);
    assert_eq!(
        zemacs_git::message_of(repo.path(), "HEAD").unwrap(),
        "subject\n\na body"
    );
    assert!(zemacs_git::show(repo.path(), "HEAD").unwrap().contains("+a"));
}

// ------------------------------------------------------------- conflicts

/// The four operations git spells `--continue` and `--abort` for, driven the
/// way the status buffer drives them: one pair of verbs, and the repository
/// says which of them is running.
#[test]
fn what_is_in_progress_is_continued_or_aborted_without_being_named() {
    if no_git() {
        return;
    }
    let repo = init("sequence");
    write(repo.path(), "a.txt", "base\n");
    commit_all(repo.path(), "first");
    git(repo.path(), &["checkout", "-q", "-b", "other"]);
    write(repo.path(), "a.txt", "theirs\n");
    commit_all(repo.path(), "theirs");
    git(repo.path(), &["checkout", "-q", "main"]);
    write(repo.path(), "a.txt", "ours\n");
    commit_all(repo.path(), "ours");

    // Nothing running is an error rather than a silent success.
    assert!(zemacs_git::sequence_continue(repo.path()).is_err());
    assert!(zemacs_git::sequence_abort(repo.path()).is_err());

    assert!(zemacs_git::merge(repo.path(), "other").is_err(), "conflict");
    assert_eq!(
        zemacs_git::status(repo.path()).unwrap().in_progress,
        Some(InProgress::Merge)
    );
    zemacs_git::sequence_abort(repo.path()).unwrap();
    let status = zemacs_git::status(repo.path()).unwrap();
    assert_eq!(status.in_progress, None);
    assert_eq!(read(repo.path(), "a.txt"), "ours\n");

    // The same merge again, resolved this time by taking one side whole.
    assert!(zemacs_git::merge(repo.path(), "other").is_err());
    zemacs_git::resolve(repo.path(), Path::new("a.txt"), false).unwrap();
    assert_eq!(read(repo.path(), "a.txt"), "theirs\n");
    // Taking a side stages it, which is what marks it resolved.
    let status = zemacs_git::status(repo.path()).unwrap();
    assert!(!status.has_conflicts(), "{status:#?}");
    zemacs_git::sequence_continue(repo.path()).unwrap();
    assert_eq!(zemacs_git::status(repo.path()).unwrap().in_progress, None);
    assert_eq!(log_count(repo.path()), 4);
}

/// A cherry-pick that conflicts leaves the sequencer running, and the same two
/// verbs get out of it — which is the whole reason they do not name a command.
#[test]
fn a_conflicted_cherry_pick_is_aborted_by_the_same_verb_a_merge_is() {
    if no_git() {
        return;
    }
    let repo = init("pick-abort");
    write(repo.path(), "a.txt", "base\n");
    commit_all(repo.path(), "first");
    git(repo.path(), &["checkout", "-q", "-b", "other"]);
    write(repo.path(), "a.txt", "theirs\n");
    commit_all(repo.path(), "theirs");
    git(repo.path(), &["checkout", "-q", "main"]);
    write(repo.path(), "a.txt", "ours\n");
    commit_all(repo.path(), "ours");

    assert!(zemacs_git::cherry_pick(repo.path(), "other").is_err());
    assert_eq!(
        zemacs_git::status(repo.path()).unwrap().in_progress,
        Some(InProgress::CherryPick)
    );
    zemacs_git::sequence_abort(repo.path()).unwrap();
    assert_eq!(zemacs_git::status(repo.path()).unwrap().in_progress, None);
    assert_eq!(read(repo.path(), "a.txt"), "ours\n");
}

/// A merge with nothing in its way is not a conflict and not an error.
#[test]
fn a_clean_merge_just_commits() {
    if no_git() {
        return;
    }
    let repo = init("merge");
    write(repo.path(), "a.txt", "a\n");
    commit_all(repo.path(), "first");
    git(repo.path(), &["checkout", "-q", "-b", "other"]);
    write(repo.path(), "b.txt", "b\n");
    commit_all(repo.path(), "theirs");
    git(repo.path(), &["checkout", "-q", "main"]);

    zemacs_git::merge(repo.path(), "other").unwrap();
    assert!(repo.path().join("b.txt").exists());
    assert_eq!(zemacs_git::status(repo.path()).unwrap().in_progress, None);
}
