//! The project half that needs the filesystem, mirroring [`crate::dired`].
//!
//! `zemacs-project` knows how to enumerate a tree; core knows the prompt. This
//! is the seam.
//!
//! # What is left here, and what went to Lisp
//!
//! Every project *verb* is now `runtime/plugins/project.lisp`. `project-root`,
//! `project-dired`, `project-compile` and `project-test` went in wave 2;
//! `project-find-file`, `project-find-dir`, `project-switch` and `project-open`
//! went in this one, once a prompt could be seeded from Lisp — see
//! [`EditorCommand::PromptSource`](zemacs_core::EditorCommand::PromptSource)
//! and the two verbs beside it, whose absence was the entire reason those four
//! could not move before.
//!
//! What stayed is not a verb at all. [`project::Cache`] walks the whole tree and
//! has to answer between two keystrokes, so the *list* is produced here and
//! named from Lisp rather than carried into the image and back out again.
//! [`Project::search_root`] is where a project-scoped ripgrep starts, asked for
//! by `main.rs` on every `OpenAt`. And `forget` is cache management, which is
//! the one thing in this file that is genuinely about the cache rather than
//! about what a project *is*.
//!
//! The root is resolved from the *current buffer*, not from the process's
//! working directory — an editor open on two repos at once should answer
//! "which project" differently in each window, and the file on screen is the
//! only honest way to tell. `%project-at` in Lisp climbs the same tree by the
//! same rule; `crates/lisp/tests/project_plugin.rs` is what holds them together.

use std::path::{Path, PathBuf};

use zemacs_core::{Editor, EditorCommand, PromptKind};
use zemacs_project as project;
use zemacs_tramp as tramp;

#[derive(Default)]
pub struct Project {
    /// One walk per root, reused across keystrokes. Owned here rather than
    /// globally so it dies with the editor. A remote root's listing lives here
    /// too, keyed by its tramp name, once the IO thread has brought it back.
    cache: project::Cache,
}

/// A remote project's listing that has to be fetched before a picker can be
/// filled — what [`Project::fill_prompt`] hands back instead of blocking.
pub struct Fetch {
    pub root: tramp::RemotePath,
    /// `project-dirs` rather than `project-files`: the same listing, folded.
    pub dirs: bool,
    /// The picker that asked. The listing is poured only into *that* one:
    /// a Lisp prompt's kind carries its id, so a picker closed and another
    /// opened while the host was answering is not handed the wrong list.
    pub kind: Option<PromptKind>,
}

impl Project {
    /// `EditorCommand::Project`, which is now one verb.
    ///
    /// It survives the migration because it is about the *cache* and not about
    /// projects: forgetting is how a file created outside the editor becomes
    /// findable at once instead of on the next staleness check. There is nothing
    /// in it a config would want to bend, which is the test everything else here
    /// failed.
    pub fn run_verb(&mut self, editor: &mut Editor, verb: &str) {
        let remote = (editor.buffer.path.as_ref())
            .is_some_and(|p| tramp::parse(&p.to_string_lossy()).is_some());
        let message = match verb {
            // The root of a remote buffer is a round trip away, so forgetting
            // does not look for it: every listing goes, and the local ones
            // cost a few milliseconds to walk again.
            "forget" if remote => {
                self.cache.clear();
                "project file lists refreshed".to_string()
            }
            "forget" => match self.locate(editor) {
                Some(found) => {
                    self.cache.forget(&found.root);
                    "project file list refreshed".to_string()
                }
                None => "not in a project — nothing to forget".to_string(),
            },
            other => format!("unknown project verb: {other}"),
        };
        editor.apply(EditorCommand::Message(message));
    }

    /// Fill the open prompt from `"SOURCE ARGUMENT"` — the app's half of
    /// [`EditorCommand::PromptSource`](zemacs_core::EditorCommand::PromptSource).
    ///
    /// The candidates go straight into the prompt without passing through the
    /// image. That is the whole reason the verb exists: a project's file list is
    /// cached precisely because walking it is too slow to do on demand, and
    /// handing it to Lisp so Lisp could hand it back is two crossings of the
    /// thing the cache exists to avoid producing twice.
    ///
    /// An unknown source is reported rather than ignored. A picker that opens
    /// empty and says nothing is indistinguishable from a project with no files
    /// in it, and one of those is a typo in a config.
    ///
    /// A remote root whose listing is not cached is the one case that cannot
    /// be answered here — it is a round trip — so it comes back as a [`Fetch`]
    /// for the caller to send to the IO thread, and [`Project::listed`] pours
    /// it in when it lands.
    pub fn fill_prompt(&mut self, editor: &mut Editor, spec: &str) -> Option<Fetch> {
        let (source, arg) = spec.split_once(' ').unwrap_or((spec, ""));
        let dirs = match source {
            "project-files" => false,
            "project-dirs" => true,
            // No `project-recent` here: that list is short enough to be a
            // reader, and `project-switch` needs to *see* it to notice it is
            // empty. See `ask_here` in `crates/lisp`.
            other => {
                editor.apply(EditorCommand::Message(format!(
                    "no such prompt source: {other}"
                )));
                return None;
            }
        };
        let root = Path::new(arg);
        // Keyed by the name exactly as Lisp spelled it, which is also what
        // `listed` keys the answer by: `Display` is `parse`'s inverse.
        if let Some(there) = tramp::parse(arg) {
            if self.cache.cached(root).is_none() {
                editor.apply(EditorCommand::Message(format!("listing {there}…")));
                let kind = editor.prompt.as_ref().map(|p| p.kind);
                return Some(Fetch { root: there, dirs, kind });
            }
        }
        self.pour(editor, root, dirs);
        None
    }

    /// A remote listing, back from the IO thread: cached, and poured into the
    /// picker that asked for it if that picker is still the one open.
    pub fn listed(&mut self, editor: &mut Editor, fetch: Fetch, files: Vec<String>, truncated: bool) {
        let root = PathBuf::from(fetch.root.to_string());
        let files = files.into_iter().map(PathBuf::from).collect();
        self.cache.put(project::Files { root: root.clone(), files, truncated });
        if editor.prompt.as_ref().map(|p| p.kind) == fetch.kind && fetch.kind.is_some() {
            self.pour(editor, &root, fetch.dirs);
            editor.touch();
        }
    }

    /// The candidates for `root` into the open prompt, from the cache.
    fn pour(&mut self, editor: &mut Editor, root: &Path, dirs: bool) {
        let found = match dirs {
            false => self.files(root),
            true => self.dirs(root).map(|d| (d, false)),
        };
        let (items, truncated) = match found {
            Ok(found) => found,
            Err(e) => {
                editor.apply(EditorCommand::Message(format!("project: {e:#}")));
                return;
            }
        };
        // Never silently: a capped list makes "the file is not there" and "you
        // have too many files" look identical, which is the one thing a file
        // finder must not do.
        let count = items.len();
        if let Some(p) = editor.prompt.as_mut() {
            p.extend_items(items.into_iter());
        }
        if truncated {
            editor.apply(EditorCommand::Message(format!(
                "showing the first {count} files — this project is larger than the cap"
            )));
        }
    }

    /// Every file in the project as absolute paths, and whether the walk hit its
    /// cap on the way.
    fn files(&mut self, root: &Path) -> anyhow::Result<(Vec<String>, bool)> {
        let files = self.cache.files(root)?;
        let items = files
            .files
            .iter()
            .map(|p| root.join(p).to_string_lossy().into_owned())
            .collect();
        Ok((items, files.truncated))
    }

    /// Every directory in the project — Emacs' `project-find-dir`.
    ///
    /// The same listing [`Project::files`] walks, folded up to the directories
    /// holding those files, so it costs one pass over a list that is already
    /// cached rather than a second walk of the tree.
    fn dirs(&mut self, root: &Path) -> anyhow::Result<Vec<String>> {
        Ok(self
            .cache
            .directories(root)?
            .iter()
            // The root comes back as `.`, and `~/src/thing/.` reads as a typo
            // rather than as the top of the project.
            .map(|p| match p == Path::new(".") {
                true => root.to_path_buf(),
                false => root.join(p),
            })
            .map(|p| p.to_string_lossy().into_owned())
            .collect())
    }

    /// The project the current buffer belongs to.
    pub fn locate(&self, editor: &Editor) -> Option<project::Project> {
        let start = editor
            .buffer
            .path
            .clone()
            .or_else(|| std::env::current_dir().ok())?;
        project::find(&start)
    }

    /// The root to search in, for a project-scoped ripgrep. Falls back to the
    /// working directory so `C-g` outside a project still does something.
    ///
    /// Local only, because it runs on the frame: a remote buffer's root is
    /// [`Roots::of`]'s question, asked from the grep thread.
    pub fn search_root(&self, editor: &Editor) -> PathBuf {
        self.locate(editor)
            .map(|f| f.root)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Where a search from a file starts, for the thread that runs the searches.
///
/// Local roots are walked every time, as [`Project::search_root`] does — it is
/// a few `stat`s. A remote one is a round trip, so it is remembered by the file
/// it was found from: a pattern is typed a character at a time from one file.
///
/// ponytail: never evicted. A session opens tens of files, not millions, and a
/// root does not move under its files; `SPC p F` is not wired to this, and a
/// `.git` created on the host mid-session is seen after a restart.
#[derive(Default)]
pub struct Roots(std::collections::HashMap<PathBuf, PathBuf>);

impl Roots {
    /// **Blocks** for a round trip the first time a remote file asks.
    pub fn of(&mut self, start: &Path) -> PathBuf {
        let Some(there) = tramp::parse(&start.to_string_lossy()) else {
            return project::find(start)
                .map(|f| f.root)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
        };
        if let Some(root) = self.0.get(start) {
            return root.clone();
        }
        // Outside any project the file's own directory, the remote twin of
        // the working-directory fallback; `/ssh:h:` itself for a file at a root.
        let beside = || PathBuf::from(there.parent().unwrap_or_else(|| there.clone()).to_string());
        match project::find_remote(&there) {
            Ok(found) => {
                let root = found.map_or_else(beside, |f| f.root);
                self.0.insert(start.to_path_buf(), root.clone());
                root
            }
            // Not remembered: a host that did not answer this keystroke may
            // answer the next, and the search itself says why it did not.
            Err(_) => beside(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one verb left, and the failure that used to be a silent no-op: a
    /// `forget` outside a project has nothing to forget and has to say so.
    #[test]
    fn forget_reports_when_there_is_no_project() {
        let mut editor = Editor::new();
        editor.buffer.path = Some(PathBuf::from("/"));
        Project::default().run_verb(&mut editor, "forget");
        assert!(!editor.status.is_empty(), "a verb that did nothing said so");
    }

    /// A source nobody has heard of is a typo in someone's config, and a picker
    /// that opened empty and silent is how a typo becomes a bug report about
    /// project detection.
    #[test]
    fn an_unknown_prompt_source_is_reported() {
        let mut editor = Editor::new();
        Project::default().fill_prompt(&mut editor, "nonsense /tmp");
        assert!(editor.status.contains("nonsense"), "{}", editor.status);
    }

    /// A remote root is never listed on the frame: the first ask comes back as
    /// a fetch, and the answer is poured only into the picker that asked — a
    /// picker opened since, while the host was answering, gets nothing.
    #[test]
    fn a_remote_listing_is_fetched_and_poured_into_the_picker_that_asked() {
        let open = |ed: &mut Editor, id| {
            ed.apply(EditorCommand::ReadFromMinibuffer {
                id,
                label: "files: ".into(),
                completing: true,
                previewing: false,
            })
        };
        let root = "/ssh:me@box:/src/app/";
        let mut editor = Editor::new();
        let mut project = Project::default();
        open(&mut editor, 1);
        let fetch = project
            .fill_prompt(&mut editor, &format!("project-files {root}"))
            .expect("a remote root is fetched, not walked");
        assert_eq!(fetch.root.to_string(), root);
        assert!(editor.prompt.as_ref().unwrap().items.is_empty());
        let first = fetch.kind;

        let files = vec!["src/main.rs".to_string(), "Cargo.toml".into()];
        project.listed(&mut editor, fetch, files.clone(), false);
        let items = editor.prompt.as_ref().unwrap().items.clone();
        assert_eq!(items, ["/ssh:me@box:/src/app/Cargo.toml", "/ssh:me@box:/src/app/src/main.rs"]);

        // Cached now, so the next ask pours at once — the directories too.
        open(&mut editor, 2);
        assert!(project.fill_prompt(&mut editor, &format!("project-dirs {root}")).is_none());
        let dirs = editor.prompt.as_ref().unwrap().items.clone();
        assert_eq!(dirs, ["/ssh:me@box:/src/app/", "/ssh:me@box:/src/app/src"]);

        // A late answer for a picker that has since been replaced is kept for
        // next time and shown to nobody.
        open(&mut editor, 3);
        let late = Fetch { root: tramp::parse("/ssh:me@box:/other/").unwrap(), dirs: false, kind: first };
        project.listed(&mut editor, late, files, false);
        assert!(editor.prompt.as_ref().unwrap().items.is_empty());
    }

    /// The app's half of the seam: Lisp named a list and a root, and the
    /// candidates land in the prompt without ever having been a Lisp object.
    ///
    /// `crates/lisp/tests/project_pick.rs` is the other half — that the name and
    /// the root are what leave the image.
    #[test]
    fn a_named_source_pours_real_paths_into_the_open_prompt() {
        let root = std::env::temp_dir().join(format!("zemacs_fill_prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let deep = root.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("main.rs"), "fn main() {}\n").unwrap();

        let mut editor = Editor::new();
        let mut project = Project::default();

        // The picker Lisp opens is a `completing-read`, so that is the kind the
        // items have to be accepted into — `PromptItems` refuses any other, on
        // purpose, and a test against a prompt core opened would pass while the
        // real path silently dropped everything.
        editor.apply(zemacs_core::EditorCommand::ReadFromMinibuffer {
            id: 1,
            label: "files: ".into(),
            completing: true,
            previewing: false,
        });
        project.fill_prompt(&mut editor, &format!("project-files {}", root.display()));
        let items = editor.prompt.as_ref().map(|p| p.items.clone()).unwrap();
        assert!(
            items.iter().any(|i| i.ends_with("src/main.rs")),
            "{items:#?}"
        );
        assert!(items.iter().all(|i| i.starts_with('/')), "{items:#?}");

        // ...and the directories are the same walk folded up, with the root
        // itself spelled as the root rather than as `<root>/.`.
        editor.prompt.as_mut().unwrap().set_items(Vec::new());
        project.fill_prompt(&mut editor, &format!("project-dirs {}", root.display()));
        let dirs = editor.prompt.as_ref().map(|p| p.items.clone()).unwrap();
        assert!(dirs.iter().any(|d| d == &root.display().to_string()), "{dirs:#?}");
        assert!(!dirs.iter().any(|d| d.ends_with("/.")), "{dirs:#?}");

        let _ = std::fs::remove_dir_all(&root);
    }
}
