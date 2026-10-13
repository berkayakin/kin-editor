//! The gutter's ▶ buttons and Run at caret: runnable functions and tests
//! in the file (see `crate::run`), run by the workspace in a Run tab.

use std::sync::Arc;

use gpui::{Context, Window};

use super::{Editor, EditorLspEvent};
use crate::run::{self, RunTarget};

/// Run this (from a gutter ▶ or Run at caret).
pub struct EditorRunEvent(pub RunTarget);

impl gpui::EventEmitter<EditorRunEvent> for Editor {}

impl Editor {
    /// What can run in this file (cached per version).
    pub fn run_targets(&mut self) -> Arc<Vec<RunTarget>> {
        let version = self.doc.version();
        if let Some((v, targets)) = &self.run_cache
            && *v == version
        {
            return targets.clone();
        }
        let targets = match (self.doc.path.as_deref(), self.doc.syntax().tree()) {
            (Some(path), Some(tree)) => run::file_targets(self.doc.language, path, &self.doc.rope().to_string(), tree),
            _ => Vec::new(),
        };
        let targets = Arc::new(targets);
        self.run_cache = Some((version, targets.clone()));
        targets
    }

    /// The target whose ▶ is on `row`.
    pub(super) fn run_target_at(&mut self, row: usize) -> Option<RunTarget> {
        self.run_targets().iter().find(|t| t.row == Some(row)).cloned()
    }

    /// Run at caret (⌃⇧R / Ctrl+Shift+F10): the innermost test or function
    /// around the caret, else the file's first runnable thing.
    pub(super) fn run_at_caret(&mut self, _: &super::RunAtCaret, _: &mut Window, cx: &mut Context<Self>) {
        let row = self.doc.row_of(self.primary_selection().head);
        let targets = self.run_targets();
        let target = targets
            .iter()
            .filter(|t| t.rows.contains(&row))
            .min_by_key(|t| t.rows.len())
            .or_else(|| targets.first())
            .cloned();
        match target {
            Some(target) => cx.emit(EditorRunEvent(target)),
            None => cx.emit(EditorLspEvent::Message("Nothing to run here".into())),
        }
    }
}
