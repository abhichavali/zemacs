//! The project half that needs the filesystem, mirroring [`crate::dired`].
//!
//! `zemacs-project` knows what a project is and how to enumerate one; core
//! knows the verbs and the prompt. This is the seam.
//!
//! The root is resolved from the *current buffer*, not from the process's
//! working directory — an editor open on two repos at once should answer
//! "which project" differently in each window, and the file on screen is the
//! only honest way to tell.

use std::path::PathBuf;

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
        // `switch` is the one verb that works without a project: its whole job
        // is to get you into one.
        if verb == "switch" {
            return self.switch(editor);
        }
        let Some(found) = self.locate(editor) else {
            editor.apply(EditorCommand::Message(
                "not in a project — no .git, Cargo.toml or the like above this file".into(),
            ));
            return Ok(());
        };
        match verb {
            "find-file" => self.find_file(editor, &found)?,
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

    /// The projects visited before, most recent first. They open as directories,
    /// and a directory opens dired, which is where you would want to land.
    fn switch(&mut self, editor: &mut Editor) -> anyhow::Result<()> {
        let recent = project::recent();
        if recent.is_empty() {
            editor.apply(EditorCommand::Message(
                "no projects visited yet — open a file in one first".into(),
            ));
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
