//! The project half that needs the filesystem, mirroring [`crate::dired`].
//!
//! `zemacs-project` knows what a project is and how to enumerate one; core
//! knows the verbs and the prompt. This is the seam.
//!
//! The root is resolved from the *current buffer*, not from the process's
//! working directory — an editor open on two repos at once should answer
//! "which project" differently in each window, and the file on screen is the
//! only honest way to tell.

use std::path::{Path, PathBuf};

use zemacs_core::{Editor, EditorCommand, PromptKind};
use zemacs_project as project;

#[derive(Default)]
pub struct Project {
    /// One walk per root, reused across keystrokes. Owned here rather than
    /// globally so it dies with the editor.
    cache: project::Cache,
    /// Set when a verb wants a shell command run; the app drains it, because
    /// spawning belongs to the terminal.
    pub run: Option<project::Command>,
}

impl Project {
    pub fn run_verb(&mut self, editor: &mut Editor, verb: &str) {
        if let Err(e) = self.try_run(editor, verb) {
            editor.apply(EditorCommand::Message(format!("project: {e:#}")));
        }
    }

    fn try_run(&mut self, editor: &mut Editor, verb: &str) -> anyhow::Result<()> {
        // `switch` and `open` are the two verbs that work without a project:
        // their whole job is to get you into one. `switch` offers the ones you
        // have been in, `open` the rest of the filesystem.
        if verb == "switch" {
            return self.switch(editor);
        }
        if verb == "open" {
            self.open_directory(editor);
            return Ok(());
        }
        let Some(found) = self.locate(editor) else {
            editor.apply(EditorCommand::Message(
                "not in a project — no .git, Cargo.toml or the like above this file".into(),
            ));
            return Ok(());
        };
        match verb {
            "find-file" => self.find_file(editor, &found)?,
            "find-dir" => self.find_dir(editor, &found)?,
            "dired" => editor.apply(EditorCommand::OpenFile(found.root.clone())),
            "root" => editor.apply(EditorCommand::Message(format!(
                "{} ({})",
                found.root.display(),
                found.marker.label()
            ))),
            // The cache is what makes completion instant; forgetting is how a
            // file created outside the editor becomes findable at once instead
            // of on the next staleness check.
            "forget" => {
                self.cache.forget(&found.root);
                editor.apply(EditorCommand::Message("project file list refreshed".into()));
            }
            "compile" | "test" => {
                let command = if verb == "compile" {
                    found.build()
                } else {
                    found.test()
                };
                match command {
                    Some(command) => {
                        editor.apply(EditorCommand::Message(format!("run: {}", command.display())));
                        self.run = Some(command);
                    }
                    None => editor.apply(EditorCommand::Message(format!(
                        "no {verb} command for a {} project",
                        found.marker.label()
                    ))),
                }
            }
            other => editor.apply(EditorCommand::Message(format!(
                "unknown project verb: {other}"
            ))),
        }
        Ok(())
    }

    /// Every file in the project, as absolute paths for the prompt to open.
    fn find_file(&mut self, editor: &mut Editor, found: &project::Project) -> anyhow::Result<()> {
        let files = self.cache.files(&found.root)?;
        let truncated = files.truncated;
        let items: Vec<String> = files
            .files
            .iter()
            .map(|p| found.root.join(p).to_string_lossy().into_owned())
            .collect();
        let count = items.len();

        editor.open_prompt(PromptKind::ProjectFile);
        if let Some(p) = editor.prompt.as_mut() {
            p.label = format!("{}: ", found.name());
            p.set_items(items);
        }
        // Never silently: a capped list makes "the file is not there" and "you
        // have too many files" look identical, which is the one thing a file
        // finder must not do.
        if truncated {
            editor.apply(EditorCommand::Message(format!(
                "showing the first {count} files — this project is larger than the cap"
            )));
        }
        Ok(())
    }

    /// Every directory in the project — Emacs' `project-find-dir`.
    ///
    /// The same listing [`Project::find_file`] walks, folded up to the
    /// directories holding those files, so it costs one pass over a list that is
    /// already cached rather than a second walk of the tree. Accepting one opens
    /// it as a directory, which is dired.
    fn find_dir(&mut self, editor: &mut Editor, found: &project::Project) -> anyhow::Result<()> {
        let items: Vec<String> = self
            .cache
            .directories(&found.root)?
            .iter()
            // The root comes back as `.`, and `~/src/thing/.` reads as a typo
            // rather than as the top of the project.
            .map(|p| match p == Path::new(".") {
                true => found.root.clone(),
                false => found.root.join(p),
            })
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        editor.open_prompt(PromptKind::ProjectFile);
        if let Some(p) = editor.prompt.as_mut() {
            p.label = format!("{} directory: ", found.name());
            p.set_items(items);
        }
        Ok(())
    }

    /// A directory *anywhere on the filesystem*, picked by typing its path.
    ///
    /// The hole this fills: [`Project::switch`] can only offer roots that have
    /// been visited and [`Project::find_file`] is scoped to one of them, so a
    /// project you had never opened was unreachable from the project keymap —
    /// which is the only place anyone looks for it. This is the ordinary file
    /// prompt, which the app already completes from the filesystem one directory
    /// at a time, so typing or Tab descends and a leading `/` starts again from
    /// the root.
    ///
    /// Accepting a directory opens dired on it *and* records it as a project,
    /// because the app remembers every directory it opens — so this prompt is
    /// only ever needed for the first visit and `switch` covers the rest.
    ///
    /// ponytail: files are offered alongside directories, because this is
    /// `PromptKind::File` and that is what it lists — picking one opens the
    /// file, which is a reasonable thing to have meant. Directories only would
    /// want a `PromptKind` of its own in core, and one extra variant to suppress
    /// some noise is not a trade worth making.
    fn open_directory(&mut self, editor: &mut Editor) {
        editor.open_prompt(PromptKind::File);
        if let Some(p) = editor.prompt.as_mut() {
            p.label = "Open directory: ".into();
            // Seeded expanded rather than as a literal `~/`: the completions the
            // app pushes back are absolute paths, and the fuzzy filter would
            // match none of them against a leading tilde. Home is where a hunt
            // for a project starts; anywhere else is a `/` and a few characters.
            p.text = crate::expand_tilde("~/");
            p.refilter();
        }
    }

    /// The projects visited before, most recent first. They open as directories,
    /// and a directory opens dired, which is where you would want to land.
    fn switch(&mut self, editor: &mut Editor) -> anyhow::Result<()> {
        let recent = project::recent();
        if recent.is_empty() {
            // Not a dead end any more. With nothing to remember, switching
            // project *is* browsing for one, so say why and hand over.
            editor.apply(EditorCommand::Message(
                "no projects visited yet — type a path to one".into(),
            ));
            self.open_directory(editor);
            return Ok(());
        }
        let items = recent
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        editor.open_prompt(PromptKind::ProjectFile);
        if let Some(p) = editor.prompt.as_mut() {
            p.label = "Switch to project: ".into();
            p.set_items(items);
        }
        Ok(())
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
    pub fn search_root(&self, editor: &Editor) -> PathBuf {
        self.locate(editor)
            .map(|f| f.root)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trap this pins down: the completions the app pushes back are
    /// *absolute* paths, so a prompt seeded with a literal `~/` matches none of
    /// them and the picker opens on an empty list looking broken. It has to
    /// start somewhere the fuzzy filter can already match.
    #[test]
    fn the_directory_picker_starts_somewhere_completable() {
        let mut editor = Editor::new();
        Project::default().run_verb(&mut editor, "open");
        let p = editor.prompt.as_ref().expect("open leaves a prompt up");
        assert_eq!(p.kind, PromptKind::File);
        if std::env::var_os("HOME").is_some() {
            assert!(p.text.starts_with('/'), "not absolute: {}", p.text);
            // Trailing separator, or the listing is of the *parent* and the
            // first keystroke re-reads a different directory.
            assert!(p.text.ends_with('/'), "not a directory: {}", p.text);
        }
    }
}
