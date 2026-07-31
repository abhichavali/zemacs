//! The projects visited, most recent first, so that switching to another one is
//! a completing prompt over real history instead of a path typed from memory.
//!
//! One absolute path per line, newest at the top. A line-per-record text file
//! rather than anything structured because a path *is* a line, the file is
//! hand-editable when it goes wrong, and appending a format later costs nothing.
//!
//! Nothing here returns an error. A missing file, an unreadable one, a
//! half-written one and a file full of binary all mean the same thing to a user
//! opening an editor — there is no history — and none of them is worth an error
//! message, let alone one that appears at startup.

use std::path::{Path, PathBuf};

/// Enough to cover everything worked on this month; past that the prompt is
/// worse, not better.
const LIMIT: usize = 32;

/// `$XDG_DATA_HOME/zemacs/projects`, or `~/.local/share/zemacs/projects` —
/// beside the recent-files list the editor already keeps. `None` only on a
/// machine with neither variable set, where there is nowhere to put it.
pub fn recents_file() -> Option<PathBuf> {
    let dir = match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local/share"),
    };
    Some(dir.join("zemacs/projects"))
}

/// Project roots, most recently visited first. Empty when there is no history,
/// and empty when the file is unreadable — see the module docs.
pub fn recent() -> Vec<PathBuf> {
    recents_file().map(|f| read_at(&f)).unwrap_or_default()
}

/// Move `root` to the top of the list. Silent about every failure.
pub fn remember(root: &Path) {
    if let Some(file) = recents_file() {
        remember_at(&file, root);
    }
}

fn remember_at(file: &Path, root: &Path) {
    // Canonical, so that the same project reached through `..` or a symlink is
    // one entry rather than three. A root that no longer exists is not history
    // worth writing down.
    let Ok(root) = root.canonicalize() else {
        return;
    };
    let mut list = vec![root.clone()];
    list.extend(read_at(file).into_iter().filter(|p| *p != root));
    list.truncate(LIMIT);
    write_at(file, &list);
}

/// Entries that are no longer directories are dropped on the way out, which is
/// how a deleted project leaves the list without anyone having to prune it.
fn read_at(file: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .collect()
}

/// ponytail: `Display` is lossy, so a project whose path is not UTF-8 is
/// written wrongly and then filtered out on the next read — it silently never
/// joins the history. Writing `OsStr` bytes directly fixes it on unix; the
/// existing recent-*files* list has the same hole, and this matches it rather
/// than diverging.
fn write_at(file: &Path, list: &[PathBuf]) {
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body: String = list.iter().map(|p| format!("{}\n", p.display())).collect();
    let _ = std::fs::write(file, body);
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// Against a real file in a real temporary directory, but *not* against
    /// `recents_file()` — pointing that at a fixture means setting `HOME` for
    /// the whole process, which the rest of the suite runs in parallel with.
    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Temp {
            let path = std::env::temp_dir().join(format!("zemacs-project-recent-{tag}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Temp(path)
        }

        fn dir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(&path).unwrap();
            path.canonicalize().unwrap()
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn remembering_a_project_puts_it_first_and_leaves_no_duplicate() {
        let temp = Temp::new("order");
        let file = temp.0.join("projects");
        let (one, two) = (temp.dir("one"), temp.dir("two"));

        remember_at(&file, &one);
        remember_at(&file, &two);
        assert_eq!(read_at(&file), vec![two.clone(), one.clone()]);

        // Revisiting promotes rather than appends.
        remember_at(&file, &one);
        assert_eq!(read_at(&file), vec![one, two]);
    }

    #[test]
    fn the_list_survives_a_round_trip_through_the_file() {
        let temp = Temp::new("roundtrip");
        let file = temp.0.join("nested/deeper/projects");
        let list = vec![temp.dir("a"), temp.dir("b")];

        // The directory the file lives in need not exist yet.
        write_at(&file, &list);
        assert_eq!(read_at(&file), list);
    }

    #[test]
    fn a_project_that_has_been_deleted_drops_off_the_list() {
        let temp = Temp::new("gone");
        let file = temp.0.join("projects");
        let gone = temp.dir("gone");
        write_at(&file, std::slice::from_ref(&gone));
        fs::remove_dir_all(&gone).unwrap();
        assert!(read_at(&file).is_empty());
    }

    #[test]
    fn a_corrupt_recents_file_reads_as_an_empty_list() {
        let temp = Temp::new("corrupt");
        let file = temp.0.join("projects");

        // Not UTF-8 at all.
        fs::write(&file, [0xff, 0xfe, 0x00, 0x01, 0x80]).unwrap();
        assert!(read_at(&file).is_empty());

        // UTF-8, but not paths.
        fs::write(&file, "{\"projects\":[]}\n\n   \n/nowhere/at/all\n").unwrap();
        assert!(read_at(&file).is_empty());
    }

    #[test]
    fn a_missing_recents_file_reads_as_an_empty_list() {
        let temp = Temp::new("missing");
        assert!(read_at(&temp.0.join("never-written")).is_empty());
    }

    #[test]
    fn remembering_a_root_that_does_not_exist_writes_nothing() {
        let temp = Temp::new("nonexistent");
        let file = temp.0.join("projects");
        remember_at(&file, &temp.0.join("no-such-project"));
        assert!(!file.exists());
    }
}
