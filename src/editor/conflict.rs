//! Merge conflicts in the editor: tinted regions, Accept Current / Incoming /
//! Both labels on each `<<<<<<<` line, and the same as actions at the caret.

use std::sync::Arc;

use gpui::{Context, Window};

use super::{AcceptBoth, AcceptCurrent, AcceptIncoming, EditKind, Editor, EditorLspEvent};
use crate::conflict::{self, Conflict, Resolution};

impl Editor {
    /// The file's conflicts (cached per version).
    pub(super) fn conflicts(&mut self) -> Arc<Vec<Conflict>> {
        let version = self.doc.version();
        if let Some((v, conflicts)) = &self.conflict_cache
            && *v == version
        {
            return conflicts.clone();
        }
        let conflicts = Arc::new(conflict::parse(&self.doc.rope().to_string()));
        self.conflict_cache = Some((version, conflicts.clone()));
        conflicts
    }

    /// Replaces the conflict starting at `start` with the side(s) kept.
    pub(super) fn resolve_conflict(&mut self, start: usize, resolution: Resolution, cx: &mut Context<Self>) {
        let Some(c) = self.conflicts().iter().find(|c| c.start == start).cloned() else {
            return;
        };
        let line = |row: usize| self.doc.line_start(row);
        let mut text: String = resolution.kept(&c).into_iter().map(|rows| self.doc.slice(line(rows.start)..line(rows.end))).collect();
        let end = line(c.end + 1);
        // At the end of a file without a final newline, keep it that way.
        if end == self.doc.len() && !self.doc.rope().to_string().ends_with('\n') && text.ends_with('\n') {
            text.pop();
        }
        self.edit_and_remap(vec![(line(c.start)..end, text)], EditKind::Other, cx);
    }

    fn resolve_at_caret(&mut self, resolution: Resolution, cx: &mut Context<Self>) {
        let row = self.doc.row_of(self.primary_selection().head);
        match self.conflicts().iter().find(|c| c.rows().contains(&row)) {
            Some(c) => {
                let start = c.start;
                self.resolve_conflict(start, resolution, cx);
            }
            None => cx.emit(EditorLspEvent::Message("No merge conflict at the caret".into())),
        }
    }

    pub(super) fn accept_current(&mut self, _: &AcceptCurrent, _: &mut Window, cx: &mut Context<Self>) {
        self.resolve_at_caret(Resolution::Current, cx);
    }

    pub(super) fn accept_incoming(&mut self, _: &AcceptIncoming, _: &mut Window, cx: &mut Context<Self>) {
        self.resolve_at_caret(Resolution::Incoming, cx);
    }

    pub(super) fn accept_both(&mut self, _: &AcceptBoth, _: &mut Window, cx: &mut Context<Self>) {
        self.resolve_at_caret(Resolution::Both, cx);
    }
}
