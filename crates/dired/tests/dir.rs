//! Integration tests. Every one of these builds a real directory tree in a
//! temporary directory and drives the public API against it — nothing is
//! mocked, because the exact behaviour of the filesystem *is* what this crate
//! is. Trees clean themselves up on [`Drop`], including on a panicking test.
//!
//! Two things are asserted over and over, because they are the two ways this
//! crate can hurt someone:
//!
//! * `text.lines().count() == map.len()`, or the cursor row indexes the wrong
//!   entry and `d` flags a file the user never looked at.
//! * [`zemacs_dired::delete`] stays inside what it was pointed at, whatever
//!   symlinks are lying around.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use zemacs_dired::{human_size, Face, Line, Listing, Sort, Span, MARK_DELETE, MARK_SELECT};

// ------------------------------------------------------------- scaffolding

/// A temp directory that deletes itself. Not worth a dependency.
struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Temp {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "zemacs-dired-{}-{}-{tag}",
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

    fn join(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    fn file(&self, rel: &str, body: &str) -> PathBuf {
        let path = self.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, body).unwrap();
        path
    }

    fn dir(&self, rel: &str) -> PathBuf {
        let path = self.join(rel);
        fs::create_dir_all(&path).unwrap();
        path
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        // A test may have left a directory unreadable on purpose; make it
        // removable again rather than leaking it into /tmp forever.
        #[cfg(unix)]
        restore_permissions(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn restore_permissions(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
    let Ok(read) = fs::read_dir(dir) else { return };
    for item in read.flatten() {
        if item.file_type().is_ok_and(|k| k.is_dir()) {
            restore_permissions(&item.path());
        }
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

fn list(dir: &Path) -> Listing {
    zemacs_dired::list(dir, false, Sort::Name).unwrap()
}

fn names(listing: &Listing) -> Vec<String> {
    listing
        .entries
        .iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect()
}

fn entry<'a>(listing: &'a Listing, name: &str) -> &'a zemacs_dired::Entry {
    listing
        .entries
        .iter()
        .find(|e| e.name == OsStr::new(name))
        .unwrap_or_else(|| panic!("no entry named {name} in {:?}", names(listing)))
}

/// The invariants, in one place, since nearly every test wants them.
fn rendered(listing: &Listing, marks: &[Option<char>]) -> (String, Vec<Line>) {
    let (text, map, spans) = zemacs_dired::render(listing, marks);
    assert_eq!(
        text.lines().count(),
        map.len(),
        "text and line map disagree:\n{text}"
    );
    check_spans(&text, &spans);
    (text, map)
}

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
fn coloured(listing: &Listing, marks: &[Option<char>]) -> Vec<(String, Face)> {
    let (text, _, spans) = zemacs_dired::render(listing, marks);
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

// ------------------------------------------------------------------ tests

#[test]
fn hidden_files_appear_only_when_asked_and_dotdot_always_does() {
    let temp = Temp::new("hidden");
    temp.file(".config", "x\n");
    temp.file("visible.txt", "x\n");
    temp.dir(".git");
    temp.dir("src");

    let plain = zemacs_dired::list(temp.path(), false, Sort::Name).unwrap();
    assert_eq!(names(&plain), ["..", "src", "visible.txt"]);
    assert!(!plain.show_hidden);

    let all = zemacs_dired::list(temp.path(), true, Sort::Name).unwrap();
    assert_eq!(
        names(&all),
        [".", "..", ".git", "src", ".config", "visible.txt"]
    );
    assert!(entry(&all, ".config").is_hidden());
    assert!(!entry(&all, "..").is_hidden() && entry(&all, "..").is_dot());
    assert!(!entry(&all, "visible.txt").is_hidden());

    // `..` is a directory that leads out of here, whichever way it is listed.
    for listing in [&plain, &all] {
        let up = entry(listing, "..");
        assert!(up.is_dir, "`..` should be a directory");
        assert_eq!(
            fs::canonicalize(&up.path).unwrap(),
            fs::canonicalize(temp.path().parent().unwrap()).unwrap()
        );
    }
}

#[test]
fn directories_come_before_files_in_every_order() {
    let temp = Temp::new("grouping");
    temp.dir("zzz-dir");
    temp.dir("aaa-dir");
    temp.file("aaa-file.txt", "x");
    temp.file("zzz-file.txt", "x");

    for sort in [Sort::Name, Sort::Size, Sort::Modified, Sort::Extension] {
        let listing = zemacs_dired::list(temp.path(), true, sort).unwrap();
        let names = names(&listing);
        assert_eq!(&names[..2], [".", ".."], "{sort:?}: {names:?}");
        let first_file = listing.entries.iter().position(|e| !e.is_dir).unwrap();
        assert!(
            listing.entries[first_file..].iter().all(|e| !e.is_dir),
            "{sort:?} interleaved directories and files: {names:?}"
        );
        // Rendering never depends on the order chosen.
        rendered(&listing, &[]);
    }
}

#[test]
fn each_sort_order_orders_by_what_it_says() {
    let temp = Temp::new("sorting");
    temp.file("small.txt", "1");
    temp.file("Middle.md", &"x".repeat(500));
    temp.file("large.txt", &"x".repeat(5000));
    // mtimes are set explicitly: writing three files in a row can land inside
    // one filesystem timestamp tick and make "newest first" a coin flip.
    for (name, ago) in [("small.txt", 300), ("Middle.md", 200), ("large.txt", 100)] {
        let when = SystemTime::now() - Duration::from_secs(ago);
        fs::File::open(temp.join(name))
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    let by_name = names(&zemacs_dired::list(temp.path(), false, Sort::Name).unwrap());
    // Case-insensitive, so `Middle.md` sorts as `middle`, not before `large`.
    assert_eq!(by_name, ["..", "large.txt", "Middle.md", "small.txt"]);

    let by_size = names(&zemacs_dired::list(temp.path(), false, Sort::Size).unwrap());
    assert_eq!(by_size, ["..", "large.txt", "Middle.md", "small.txt"]);

    let by_date = names(&zemacs_dired::list(temp.path(), false, Sort::Modified).unwrap());
    assert_eq!(by_date, ["..", "large.txt", "Middle.md", "small.txt"]);

    // Extension groups: .md before .txt, then by name inside the group.
    let by_ext = names(&zemacs_dired::list(temp.path(), false, Sort::Extension).unwrap());
    assert_eq!(by_ext, ["..", "Middle.md", "large.txt", "small.txt"]);
}

#[test]
fn render_text_and_map_always_have_the_same_length() {
    let empty = Temp::new("render-empty");
    let listing = list(empty.path());
    // An "empty" directory still offers the way out.
    assert_eq!(names(&listing), [".."]);
    let (text, map) = rendered(&listing, &[]);
    assert_eq!(map, vec![Line::Header, Line::Blank, Line::Entry(0)]);
    assert!(text.contains("(1 entry, by name)"), "{text}");

    let temp = Temp::new("render-full");
    temp.file("notes.txt", "hello\n");
    temp.file("empty.log", "");
    temp.dir("src");
    temp.file(".hidden", "x");
    #[cfg(unix)]
    symlink(Path::new("src"), &temp.join("link"));

    for show_hidden in [false, true] {
        let listing = zemacs_dired::list(temp.path(), show_hidden, Sort::Name).unwrap();
        // No marks at all, one of each mark, and more marks than entries.
        let none: Vec<Option<char>> = vec![];
        let mixed: Vec<Option<char>> = listing
            .entries
            .iter()
            .enumerate()
            .map(|(i, _)| match i % 3 {
                0 => None,
                1 => Some(MARK_SELECT),
                _ => Some(MARK_DELETE),
            })
            .collect();
        let over = vec![Some(MARK_SELECT); listing.entries.len() + 5];
        for marks in [&none, &mixed, &over] {
            let (text, map) = rendered(&listing, marks);
            assert_eq!(map.len(), listing.entries.len() + 2);
            assert_eq!(text.lines().count(), listing.entries.len() + 2);
        }
        // All three mark states are visible at once.
        let (text, _) = rendered(&listing, &mixed);
        assert!(text.lines().any(|l| l.starts_with("* ")), "{text}");
        assert!(text.lines().any(|l| l.starts_with("D ")), "{text}");
        assert!(text.lines().skip(2).any(|l| l.starts_with("  ")), "{text}");
    }
}

#[test]
fn the_line_map_points_at_the_entry_drawn_on_that_line() {
    let temp = Temp::new("line-map");
    temp.file("a.txt", "a");
    temp.file("b.txt", "bb");
    temp.dir("sub");
    let listing = zemacs_dired::list(temp.path(), true, Sort::Name).unwrap();
    let (text, map) = rendered(&listing, &[]);
    let rows: Vec<&str> = text.lines().collect();

    assert_eq!(map[0], Line::Header);
    assert!(rows[0].contains(&*temp.path().to_string_lossy()), "{text}");
    assert_eq!(map[1], Line::Blank);

    let mut seen = 0;
    for (row, line) in map.iter().enumerate() {
        let Line::Entry(index) = *line else { continue };
        // Rows and entries advance together, in the listing's own order.
        assert_eq!(index, seen);
        seen += 1;
        let name = listing.entries[index].name.to_string_lossy();
        assert!(
            rows[row].ends_with(&*name) || rows[row].ends_with(&format!("{name}/")),
            "row {row} ({:?}) is not entry {index} ({name})",
            rows[row]
        );
    }
    assert_eq!(seen, listing.entries.len());

    // What the UI actually does: cursor on row N, act on that file.
    let row = rows
        .iter()
        .position(|r| r.ends_with("b.txt"))
        .expect("no row for b.txt");
    let Line::Entry(index) = map[row] else {
        panic!("row {row} is not an entry: {:?}", map[row])
    };
    assert_eq!(listing.entries[index].path, temp.join("b.txt"));
    assert_eq!(listing.index_of(OsStr::new("b.txt")), Some(index));
    assert_eq!(listing.index_of(OsStr::new("nope")), None);
}

#[test]
fn a_rendered_row_shows_permissions_size_date_and_kind() {
    let temp = Temp::new("row-shape");
    temp.file("notes.txt", &"x".repeat(4096));
    temp.dir("src");
    #[cfg(unix)]
    symlink(Path::new("notes.txt"), &temp.join("latest"));

    let listing = list(temp.path());
    let (text, map) = rendered(&listing, &[Some(MARK_SELECT); 8]);
    // By index, not by searching the text: `latest -> notes.txt` also contains
    // "notes.txt", and picking rows by substring would compare the wrong line.
    let row = |name: &str| {
        let index = listing.index_of(OsStr::new(name)).unwrap();
        let at = map.iter().position(|l| *l == Line::Entry(index)).unwrap();
        text.lines().nth(at).unwrap().to_string()
    };

    let notes = row("notes.txt");
    assert!(notes.starts_with("* -rw"), "{notes}");
    assert!(notes.contains("4.0K"), "{notes}");
    // A date column, in the fixed shape the name column depends on.
    let date = &notes[notes.find("4.0K").unwrap() + 6..][..16];
    assert_eq!(date.len(), 16);
    assert!(date.starts_with("20") && date.contains(':'), "{date:?}");
    assert!(row("src").contains("src/"), "directories get a slash");
    #[cfg(unix)]
    {
        let link = row("latest");
        assert!(link.contains("latest -> notes.txt"), "{link}");
        assert!(link.contains("* l"), "a symlink's type character: {link}");
        assert!(!link.contains("latest/"), "{link}");
    }
}

#[test]
fn rename_moves_a_file_and_refuses_to_clobber() {
    let temp = Temp::new("rename");
    let from = temp.file("old.txt", "body\n");
    let to = temp.join("new.txt");
    temp.file("occupied.txt", "someone else\n");

    zemacs_dired::rename(&from, &to).unwrap();
    assert!(!from.exists() && to.exists());
    assert_eq!(fs::read_to_string(&to).unwrap(), "body\n");

    // Onto an existing file: refused, and the victim is untouched.
    let err = zemacs_dired::rename(&to, &temp.join("occupied.txt"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(
        fs::read_to_string(temp.join("occupied.txt")).unwrap(),
        "someone else\n"
    );
    assert!(to.exists());

    // A missing source is an error, not a silent success.
    let err = zemacs_dired::rename(&temp.join("ghost"), &temp.join("x"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("does not exist"), "{err}");

    // Directories move too, contents and all.
    temp.file("tree/inner/deep.txt", "deep\n");
    zemacs_dired::rename(&temp.join("tree"), &temp.join("moved")).unwrap();
    assert_eq!(
        fs::read_to_string(temp.join("moved/inner/deep.txt")).unwrap(),
        "deep\n"
    );

    // A dangling symlink counts as an occupied destination: `exists()` would
    // say the path is free and the rename would silently eat the link.
    #[cfg(unix)]
    {
        symlink(Path::new("nowhere"), &temp.join("dangling"));
        let err = zemacs_dired::rename(&to, &temp.join("dangling"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        assert!(fs::symlink_metadata(temp.join("dangling")).is_ok());
    }
}

#[test]
fn copy_duplicates_a_file_and_a_whole_tree() {
    let temp = Temp::new("copy");
    let file = temp.file("a.txt", "contents\n");
    zemacs_dired::copy(&file, &temp.join("b.txt")).unwrap();
    assert_eq!(
        fs::read_to_string(temp.join("b.txt")).unwrap(),
        "contents\n"
    );
    assert!(file.exists(), "copy is not a move");

    // Refuses to overwrite, and leaves the destination as it was.
    let err = zemacs_dired::copy(&file, &temp.join("b.txt"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(
        fs::read_to_string(temp.join("b.txt")).unwrap(),
        "contents\n"
    );

    // A tree, recursively.
    temp.file("tree/top.txt", "top\n");
    temp.file("tree/inner/deep.txt", "deep\n");
    temp.dir("tree/inner/empty");
    #[cfg(unix)]
    symlink(Path::new("../top.txt"), &temp.join("tree/inner/link"));

    let dest = temp.join("clone");
    zemacs_dired::copy(&temp.join("tree"), &dest).unwrap();
    assert_eq!(fs::read_to_string(dest.join("top.txt")).unwrap(), "top\n");
    assert_eq!(
        fs::read_to_string(dest.join("inner/deep.txt")).unwrap(),
        "deep\n"
    );
    assert!(dest.join("inner/empty").is_dir());
    // The original is still whole.
    assert!(temp.join("tree/inner/deep.txt").exists());

    // A symlink is copied as a symlink, not followed and flattened.
    #[cfg(unix)]
    {
        let link = dest.join("inner/link");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("../top.txt"));
    }

    // Copying a missing source is an error rather than an empty file.
    assert!(zemacs_dired::copy(&temp.join("ghost"), &temp.join("g")).is_err());
    assert!(!temp.join("g").exists());
}

#[test]
fn copy_refuses_to_recurse_into_itself() {
    let temp = Temp::new("copy-into-self");
    temp.file("tree/a.txt", "a\n");
    let tree = temp.join("tree");

    for destination in ["tree/copy", "tree/a/b/c"] {
        let err = zemacs_dired::copy(&tree, &temp.join(destination))
            .unwrap_err()
            .to_string();
        assert!(err.contains("into itself"), "{destination}: {err}");
        assert!(!temp.join(destination).exists());
    }

    // Onto itself, which is the degenerate case of the same bug.
    assert!(zemacs_dired::copy(&tree, &tree).is_err());

    // A sibling whose name merely starts the same way is not inside it.
    zemacs_dired::copy(&tree, &temp.join("treeish")).unwrap();
    assert!(temp.join("treeish/a.txt").exists());

    // And through a symlink: `link` *is* `tree`, so this is still self-copy.
    #[cfg(unix)]
    {
        symlink(&tree, &temp.join("link-to-tree"));
        let err = zemacs_dired::copy(&tree, &temp.join("link-to-tree/inside"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("into itself"), "{err}");
        assert!(!tree.join("inside").exists());
    }
    // The source survived every refusal intact.
    assert_eq!(names(&list(&tree)), ["..", "a.txt"]);
}

#[test]
fn delete_removes_a_file_and_a_whole_tree() {
    let temp = Temp::new("delete");
    let file = temp.file("doomed.txt", "x\n");
    zemacs_dired::delete(&file).unwrap();
    assert!(!file.exists());

    // Deleting it twice is an error, not a panic.
    let err = zemacs_dired::delete(&file).unwrap_err().to_string();
    assert!(err.contains("doomed.txt"), "{err}");

    temp.file("tree/a.txt", "a\n");
    temp.file("tree/inner/b.txt", "b\n");
    temp.dir("tree/inner/empty");
    temp.file("keep.txt", "keep\n");

    zemacs_dired::delete(&temp.join("tree")).unwrap();
    assert!(!temp.join("tree").exists());
    // Nothing outside the tree moved.
    assert_eq!(fs::read_to_string(temp.join("keep.txt")).unwrap(), "keep\n");

    // An empty directory is the trivial case of the same walk.
    let empty = temp.dir("empty");
    zemacs_dired::delete(&empty).unwrap();
    assert!(!empty.exists());
}

/// The most dangerous function in the crate, pinned down.
#[cfg(unix)]
#[test]
fn deleting_a_symlink_never_touches_what_it_points_at() {
    let temp = Temp::new("delete-symlink");
    let precious = temp.dir("precious");
    temp.file("precious/photo.jpg", "irreplaceable\n");
    temp.file("precious/nested/more.txt", "also irreplaceable\n");
    let elsewhere = temp.file("elsewhere.txt", "not mine\n");

    // 1. A symlink *to a directory*, deleted directly.
    let link = temp.join("link-to-precious");
    symlink(&precious, &link);
    assert!(
        zemacs_dired::list(&link, false, Sort::Name).is_ok(),
        "setup"
    );
    zemacs_dired::delete(&link).unwrap();
    assert!(
        fs::symlink_metadata(&link).is_err(),
        "the link should be gone"
    );
    assert_eq!(
        fs::read_to_string(precious.join("photo.jpg")).unwrap(),
        "irreplaceable\n"
    );
    assert!(precious.join("nested/more.txt").exists());

    // 2. Links *inside* a tree being deleted recursively: the same rule has to
    //    hold at every level of the walk, not only at the top.
    let victim = temp.dir("victim");
    temp.file("victim/own.txt", "mine\n");
    symlink(&precious, &victim.join("dir-link"));
    symlink(&elsewhere, &victim.join("file-link"));
    symlink(&precious, &temp.dir("victim/deep").join("dir-link"));
    symlink(Path::new("/nonexistent-target"), &victim.join("broken"));
    // A link pointing at the victim's own parent, which is the shape that
    // would delete the entire temp directory if the walk followed links.
    symlink(temp.path(), &victim.join("up-link"));

    zemacs_dired::delete(&victim).unwrap();
    assert!(!victim.exists());
    assert_eq!(
        fs::read_to_string(precious.join("photo.jpg")).unwrap(),
        "irreplaceable\n"
    );
    assert!(precious.join("nested/more.txt").exists());
    assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "not mine\n");
    // The parent survived: the up-link was unlinked, not walked.
    assert!(temp.path().is_dir());

    // 3. A symlink to a *file* is likewise unlinked, target untouched.
    let file_link = temp.join("link-to-file");
    symlink(&elsewhere, &file_link);
    zemacs_dired::delete(&file_link).unwrap();
    assert!(fs::symlink_metadata(&file_link).is_err());
    assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "not mine\n");
}

#[test]
fn create_dir_and_create_file_make_things_and_refuse_bad_names() {
    let temp = Temp::new("create");

    let dir = zemacs_dired::create_dir(temp.path(), "new dir").unwrap();
    assert_eq!(dir, temp.join("new dir"));
    assert!(dir.is_dir());

    let file = zemacs_dired::create_file(temp.path(), "notes.txt").unwrap();
    assert_eq!(file, temp.join("notes.txt"));
    assert_eq!(fs::read_to_string(&file).unwrap(), "");

    // Neither clobbers, and creating over a file never truncates it.
    fs::write(&file, "precious\n").unwrap();
    assert!(zemacs_dired::create_file(temp.path(), "notes.txt").is_err());
    assert!(zemacs_dired::create_dir(temp.path(), "new dir").is_err());
    assert!(zemacs_dired::create_dir(temp.path(), "notes.txt").is_err());
    assert_eq!(fs::read_to_string(&file).unwrap(), "precious\n");

    // A name is one component: nothing a prompt can be answered with escapes.
    for bad in ["", ".", "..", "../escape", "sub/deep.txt", "/etc/passwd"] {
        assert!(
            zemacs_dired::create_file(temp.path(), bad).is_err(),
            "created a file named {bad:?}"
        );
        assert!(
            zemacs_dired::create_dir(temp.path(), bad).is_err(),
            "created a directory named {bad:?}"
        );
    }
    assert!(!temp.path().parent().unwrap().join("escape").exists());

    // Awkward but legal names are legal.
    for good in ["-rf", "a b c", "quo\"te", "café.rs", "$(touch pwned)"] {
        let path = zemacs_dired::create_file(temp.path(), good).unwrap();
        assert!(path.exists(), "{good}");
    }
    assert!(!temp.join("pwned").exists(), "no shell was involved");

    let listing = zemacs_dired::list(temp.path(), false, Sort::Name).unwrap();
    let (text, map) = rendered(&listing, &[]);
    assert!(text.contains("$(touch pwned)"), "{text}");
    assert_eq!(map.len(), listing.entries.len() + 2);
}

/// Non-UTF-8 names are real on unix, which is why [`zemacs_dired::Entry::name`]
/// is an `OsString`. Some filesystems (APFS, HFS+) reject them outright at
/// creation with `EILSEQ`; there the test reports and stops rather than
/// pretending to have proven anything.
#[cfg(unix)]
#[test]
fn a_non_utf8_name_lists_renders_and_renames() {
    use std::os::unix::ffi::OsStrExt;

    let temp = Temp::new("non-utf8");
    let name = OsStr::from_bytes(b"bad\xFFname").to_os_string();
    let path = temp.join("x").with_file_name(&name);

    // The rendering half needs no filesystem, so this much is proven even where
    // the name cannot be created: one line, one map entry, bytes intact.
    let synthetic = Listing {
        dir: temp.path().to_path_buf(),
        entries: vec![zemacs_dired::Entry {
            name: name.clone(),
            path: path.clone(),
            is_dir: false,
            is_symlink: false,
            link_target: None,
            len: 6,
            modified: None,
            readonly: false,
            mode: 0o644,
        }],
        show_hidden: false,
        sort: Sort::Name,
    };
    let (text, map) = rendered(&synthetic, &[]);
    assert_eq!(map, vec![Line::Header, Line::Blank, Line::Entry(0)]);
    // Lossy for display only — U+FFFD stands in for the byte, and the entry the
    // map points at still carries the real one.
    assert!(
        text.lines().nth(2).unwrap().ends_with("bad\u{fffd}name"),
        "{text}"
    );
    assert_eq!(synthetic.entries[0].name.as_bytes(), b"bad\xFFname");

    if fs::write(&path, "bytes\n").is_err() {
        eprintln!(
            "skipping non-UTF-8 name test: {} rejects the name (APFS and HFS+ \
             enforce valid UTF-8; the code path is exercised on ext4/xfs/btrfs)",
            temp.path().display()
        );
        return;
    }
    temp.file("normal.txt", "x\n");

    let listing = list(temp.path());
    let found = listing
        .entries
        .iter()
        .find(|e| e.name == name)
        .expect("the non-UTF-8 name did not survive listing");
    // The bytes are intact, not replaced by U+FFFD.
    assert_eq!(found.name.as_bytes(), b"bad\xFFname");
    assert_eq!(found.path, path);

    // Rendering is lossy on purpose — pixels, not a syscall argument — but it
    // must still be exactly one line, and the map must still point at it.
    let (text, map) = rendered(&listing, &[]);
    let index = listing.index_of(&name).unwrap();
    let row = map.iter().position(|l| *l == Line::Entry(index)).unwrap();
    assert!(text.lines().nth(row).unwrap().contains("bad"), "{text}");

    // And the real path still works, which is the point of keeping it.
    let renamed = temp
        .join("x")
        .with_file_name(OsStr::from_bytes(b"good\xFEname"));
    zemacs_dired::rename(&found.path, &renamed).unwrap();
    assert!(fs::symlink_metadata(&renamed).is_ok());
    assert!(fs::symlink_metadata(&path).is_err());
    let listing = list(temp.path());
    assert!(listing.index_of(renamed.file_name().unwrap()).is_some());
    rendered(&listing, &[]);
    zemacs_dired::delete(&renamed).unwrap();
}

/// A newline in a name would split one entry across two rendered lines and
/// slide every entry after it in the map — the bug that flags the wrong file.
#[test]
fn a_name_containing_a_newline_cannot_slide_the_line_map() {
    let temp = Temp::new("newline-name");
    let hostile = temp.join("two\nlines.txt");
    if fs::write(&hostile, "x\n").is_err() {
        eprintln!("skipping newline name test: this filesystem rejects it");
        return;
    }
    temp.file("after.txt", "x\n");
    temp.file("aaa.txt", "x\n");
    temp.dir("tab\tdir");
    temp.file("carriage\rreturn.txt", "x\n");

    let listing = list(temp.path());
    let (text, map) = rendered(&listing, &[]);
    assert!(text.contains("two?lines.txt"), "{text}");
    assert!(text.contains("tab?dir/"), "{text}");
    assert!(text.contains("carriage?return.txt"), "{text}");
    assert!(!text.contains("two\nlines"), "the newline survived: {text}");

    // Every entry is still exactly one row, in order.
    let rows: Vec<&str> = text.lines().collect();
    let index = listing.index_of(OsStr::new("two\nlines.txt")).unwrap();
    let row = map.iter().position(|l| *l == Line::Entry(index)).unwrap();
    assert!(rows[row].ends_with("two?lines.txt"), "{:?}", rows[row]);

    // Rows and entries stay in step across the hostile name — this is the
    // assertion that fails if a name ever contributes a second line.
    let other = listing.index_of(OsStr::new("after.txt")).unwrap();
    let other_row = map.iter().position(|l| *l == Line::Entry(other)).unwrap();
    assert_eq!(
        other_row as isize - row as isize,
        other as isize - index as isize
    );

    // Acting on that row still acts on that file, hostile name and all.
    let Line::Entry(index) = map[row] else {
        panic!("row {row} is not an entry")
    };
    zemacs_dired::delete(&listing.entries[index].path).unwrap();
    assert!(fs::symlink_metadata(&hostile).is_err());
    assert!(temp.join("after.txt").exists());
}

#[cfg(unix)]
#[test]
fn a_broken_symlink_still_lists_and_renders() {
    let temp = Temp::new("broken-link");
    symlink(Path::new("/nowhere/at/all"), &temp.join("broken"));
    symlink(Path::new("loop"), &temp.join("loop"));
    temp.file("real.txt", "x\n");

    let listing = list(temp.path());
    let broken = entry(&listing, "broken");
    assert!(broken.is_symlink);
    // Not a directory, because opening it lands nowhere.
    assert!(!broken.is_dir);
    assert_eq!(
        broken.link_target.as_deref(),
        Some(Path::new("/nowhere/at/all"))
    );
    // Degraded, not absent: the link's own lstat still answered.
    assert!(broken.modified.is_some());

    // A link pointing at itself resolves to nothing and must not hang.
    let looping = entry(&listing, "loop");
    assert!(looping.is_symlink && !looping.is_dir);

    let (text, _) = rendered(&listing, &[]);
    assert!(text.contains("broken -> /nowhere/at/all"), "{text}");

    // And it can still be deleted, which is usually why you are looking at it.
    zemacs_dired::delete(&broken.path).unwrap();
    assert!(fs::symlink_metadata(temp.join("broken")).is_err());
}

#[test]
fn unreadable_and_missing_directories_are_errors_not_panics() {
    let temp = Temp::new("errors");

    let err = zemacs_dired::list(&temp.join("ghost"), false, Sort::Name)
        .unwrap_err()
        .to_string();
    assert!(err.contains("listing"), "{err}");

    // A file is not a directory.
    let file = temp.file("a.txt", "x\n");
    assert!(zemacs_dired::list(&file, false, Sort::Name).is_err());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = temp.dir("locked");
        fs::write(locked.join("secret.txt"), "x\n").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // Root ignores the mode bits, so there is nothing to assert there.
        if fs::read_dir(&locked).is_ok() {
            eprintln!("skipping unreadable-directory assertion: running as root");
        } else {
            let err = zemacs_dired::list(&locked, false, Sort::Name)
                .unwrap_err()
                .to_string();
            assert!(err.contains("locked"), "{err}");
        }
        // The parent still lists it, with whatever metadata it could get.
        let listing = list(temp.path());
        let entry = entry(&listing, "locked");
        assert!(entry.is_dir);
        rendered(&listing, &[]);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn sizes_are_human_readable_at_the_edges() {
    assert_eq!(human_size(0), "0");
    assert_eq!(human_size(1023), "1023");
    assert_eq!(human_size(1024), "1.0K");
    assert_eq!(human_size(u64::MAX), "16E");

    // And through a real file, so the column is not just unit-tested arithmetic.
    let temp = Temp::new("sizes");
    temp.file("empty", "");
    temp.file("k", &"x".repeat(2048));
    let listing = list(temp.path());
    assert_eq!(entry(&listing, "empty").len, 0);
    assert_eq!(entry(&listing, "k").len, 2048);
    let (text, _) = rendered(&listing, &[]);
    assert!(text.contains("2.0K"), "{text}");
    assert!(
        text.lines()
            .any(|l| l.ends_with("empty") && l.contains(" 0 ")),
        "{text}"
    );
}

/// A whole session: list, mark, act, re-list — the loop the UI runs.
#[test]
fn a_dired_session_survives_a_round_trip() {
    let temp = Temp::new("session");
    temp.file("keep.txt", "keep\n");
    temp.file("doomed.txt", "bye\n");
    temp.dir("src");

    let listing = list(temp.path());
    let mut marks: Vec<Option<char>> = vec![None; listing.entries.len()];
    let doomed = listing.index_of(OsStr::new("doomed.txt")).unwrap();
    marks[doomed] = Some(MARK_DELETE);
    let (text, map) = rendered(&listing, &marks);
    assert!(text.lines().any(|l| l.starts_with("D ")), "{text}");

    // `x`: execute the flags.
    for (index, mark) in marks.iter().enumerate() {
        if *mark == Some(MARK_DELETE) {
            zemacs_dired::delete(&listing.entries[index].path).unwrap();
        }
    }
    // ...and the copy and rename a user would do next.
    zemacs_dired::copy(&temp.join("keep.txt"), &temp.join("src/keep.txt")).unwrap();
    zemacs_dired::rename(&temp.join("keep.txt"), &temp.join("kept.txt")).unwrap();
    let new_dir = zemacs_dired::create_dir(temp.path(), "build").unwrap();
    zemacs_dired::create_file(&new_dir, "log.txt").unwrap();

    let after = zemacs_dired::list(temp.path(), false, Sort::Name).unwrap();
    assert_eq!(names(&after), ["..", "build", "src", "kept.txt"]);
    assert_eq!(names(&list(&new_dir)), ["..", "log.txt"]);
    assert_eq!(
        fs::read_to_string(temp.join("src/keep.txt")).unwrap(),
        "keep\n"
    );
    // The map still lines up after everything moved, and the buffer redrew.
    let (text_after, map_after) = rendered(&after, &[]);
    assert_ne!(text_after, text);
    assert_eq!(map_after.len(), after.entries.len() + 2);
    assert_eq!(
        map_after[map.len() - 1],
        Line::Entry(after.entries.len() - 1)
    );
    assert!(!text_after.contains("doomed.txt"), "{text_after}");
    assert!(
        !text_after.lines().any(|l| l.starts_with("D ")),
        "{text_after}"
    );
}

/// `OsString` names round-trip through the public API without going via `str`.
#[test]
fn entries_keep_their_real_paths() {
    let temp = Temp::new("paths");
    let path = temp.file("café.rs", "x\n");
    let listing = list(temp.path());
    let entry = entry(&listing, "café.rs");
    assert_eq!(entry.path, path);
    assert_eq!(entry.name, OsString::from("café.rs"));
    assert!(!entry.is_dir && !entry.is_symlink && entry.link_target.is_none());
    assert_eq!(entry.len, 2);
    assert!(!entry.readonly);
    #[cfg(unix)]
    assert_ne!(entry.mode & 0o400, 0, "readable by its owner");
}

// ------------------------------------------------------------------ colour

#[test]
fn directory_entries_are_coloured_as_types() {
    let temp = Temp::new("colour-dirs");
    temp.dir("src");
    temp.file("notes.txt", "x\n");
    let faces = coloured(&list(temp.path()), &[]);

    // `..` is navigation but it is still a directory, and the trailing slash is
    // part of the name the entry line shows.
    assert_coloured(&faces, "../", Face::Type);
    assert_coloured(&faces, "src/", Face::Type);
    // A plain file is the buffer's foreground and gets no span at all.
    assert!(
        !faces.iter().any(|(t, _)| t == "notes.txt"),
        "a plain file should not be coloured: {faces:#?}"
    );
}

#[cfg(unix)]
#[test]
fn executables_are_coloured_as_functions() {
    use std::os::unix::fs::PermissionsExt;

    let temp = Temp::new("colour-exec");
    let script = temp.file("run.sh", "#!/bin/sh\n");
    temp.file("plain.sh", "#!/bin/sh\n");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let faces = coloured(&list(temp.path()), &[]);

    assert_coloured(&faces, "run.sh", Face::Function);
    assert!(
        !faces.iter().any(|(t, _)| t == "plain.sh"),
        "a non-executable file should not be coloured: {faces:#?}"
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_is_a_link_and_its_target_is_dimmed() {
    let temp = Temp::new("colour-link");
    let target = temp.dir("builds");
    symlink(&target, &temp.join("latest"));
    let faces = coloured(&list(temp.path()), &[]);

    // The link, not the directory it resolves to — the `l` rule, in colour.
    assert_coloured(&faces, "latest", Face::Link);
    assert_coloured(&faces, " -> ", Face::Punctuation);
    assert_coloured(&faces, &target.display().to_string(), Face::Comment);
}

#[test]
fn marks_permissions_sizes_and_dates_each_get_their_own_face() {
    let temp = Temp::new("colour-columns");
    temp.file("notes.txt", "0123456789\n");
    let listing = list(temp.path());
    let index = listing.index_of(OsStr::new("notes.txt")).unwrap();
    let mut marks = vec![None; listing.entries.len()];
    marks[index] = Some(MARK_DELETE);
    let faces = coloured(&listing, &marks);

    assert_coloured(&faces, &MARK_DELETE.to_string(), Face::Constant);
    assert_coloured(&faces, "11", Face::Number);
    #[cfg(unix)]
    assert_coloured(&faces, "-rw-r--r--", Face::Comment);
    // The mtime is real, so the timestamp column is filled and dimmed.
    assert!(
        faces
            .iter()
            .any(|(t, k)| *k == Face::Comment && t.len() == 16 && t.starts_with("20")),
        "no timestamp span: {faces:#?}"
    );
    // An unmarked entry contributes no mark span; only the one D exists.
    assert_eq!(
        faces.iter().filter(|(_, k)| *k == Face::Constant).count(),
        1,
        "{faces:#?}"
    );
}

#[test]
fn the_header_names_the_directory_and_dims_its_summary() {
    let temp = Temp::new("colour-header");
    temp.file("one.txt", "x\n");
    let listing = list(temp.path());
    let faces = coloured(&listing, &[]);

    assert_coloured(&faces, &temp.path().display().to_string(), Face::Heading1);
    assert_coloured(&faces, "(2 entries, by name)", Face::Comment);
}

/// The whole reason the offsets are characters: a name outside ASCII must not
/// slide every span after it.
#[test]
fn span_offsets_are_characters_not_bytes() {
    let temp = Temp::new("colour-utf8");
    temp.dir("café");
    temp.dir("日本語");
    temp.file("naïve.txt", "x\n");
    let faces = coloured(&list(temp.path()), &[]);

    // `slice` inside `coloured` counts characters, so these only come out whole
    // if `render` counted characters too.
    assert_coloured(&faces, "café/", Face::Type);
    assert_coloured(&faces, "日本語/", Face::Type);
    // And the entry *after* the multi-byte ones is still cut in the right place.
    assert_coloured(&faces, "(4 entries, by name)", Face::Comment);
    assert!(
        !faces.iter().any(|(t, _)| t == "naïve.txt"),
        "a plain file should not be coloured: {faces:#?}"
    );
}

/// A control character folds to `?`, one character for one, so the folding
/// cannot move a span either.
#[test]
fn folded_names_keep_their_spans_aligned() {
    let listing = Listing {
        dir: PathBuf::from("/tmp"),
        entries: vec![],
        show_hidden: false,
        sort: Sort::Name,
    };
    let faces = coloured(&listing, &[]);
    assert_coloured(&faces, "(empty)", Face::Comment);
}
