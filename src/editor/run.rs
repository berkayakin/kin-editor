//! The gutter's ▶ buttons, Run and Debug at caret (runnable functions and
//! tests in the file, see `crate::run`, run by the workspace in a Run tab
//! or a debug session), and the file's breakpoints.

use std::{path::PathBuf, sync::Arc};

use gpui::{Context, Pixels, Point, Window};

use super::{Editor, EditorLspEvent, EditorMode};
use crate::{
    document::Breakpoint,
    run::{self, RunTarget},
    syntax::Language,
};

/// Run or debug this (from a gutter ▶, Run or Debug at caret).
pub struct EditorRunEvent {
    pub target: RunTarget,
    pub debug: bool,
    /// A ▶ was clicked here: offer Run and Debug.
    pub menu_at: Option<Point<Pixels>>,
}

impl gpui::EventEmitter<EditorRunEvent> for Editor {}

/// This file's breakpoints changed (a running debug session needs them).
pub struct EditorBreakpointsChanged;

impl gpui::EventEmitter<EditorBreakpointsChanged> for Editor {}

/// A line number was right-clicked: offer the breakpoint options for `row`.
pub struct EditorBreakpointMenu {
    pub row: usize,
    pub position: Point<Pixels>,
}

impl gpui::EventEmitter<EditorBreakpointMenu> for Editor {}

impl Editor {
    /// What can run in this file (cached per version).
    pub fn run_targets(&mut self) -> Arc<Vec<RunTarget>> {
        let (key, job) = self.run_job();
        self.run_targets.now(key, job)
    }

    /// The targets to draw (gutter ▶): the last ones found (see `Derived`).
    pub(super) fn shown_run_targets(&mut self, cx: &mut Context<Self>) -> Arc<Vec<RunTarget>> {
        let (key, job) = self.run_job();
        self.run_targets.schedule(key, job, |e| &mut e.run_targets, cx);
        self.run_targets.last()
    }

    /// Finding targets reads the disk (the crate, the package), so it runs
    /// in the background for drawing.
    fn run_job(&self) -> ((u64, Option<PathBuf>, Language), impl FnOnce() -> Vec<RunTarget> + Send + 'static) {
        let (path, language) = (self.doc.path.clone(), self.doc.language);
        let (tree, rope) = (self.doc.syntax().tree().cloned(), self.doc.rope().clone());
        let key = (self.doc.version(), path.clone(), language);
        let job = move || match (path, tree) {
            (Some(path), Some(tree)) => run::file_targets(language, &path, &rope.to_string(), &tree),
            _ => Vec::new(),
        };
        (key, job)
    }

    /// The target whose ▶ is on `row`.
    pub(super) fn run_target_at(&mut self, row: usize) -> Option<RunTarget> {
        self.run_targets().iter().find(|t| t.row == Some(row)).cloned()
    }

    /// The innermost test or function around the caret, else the file's
    /// first runnable thing (only debuggable ones when `debug`).
    fn target_at_caret(&mut self, debug: bool) -> Option<RunTarget> {
        let row = self.doc.row_of(self.primary_selection().head);
        let targets = self.run_targets();
        let usable = |t: &&RunTarget| !debug || t.debug.is_some();
        targets
            .iter()
            .filter(usable)
            .filter(|t| t.rows.contains(&row))
            .min_by_key(|t| t.rows.len())
            .or_else(|| targets.iter().find(usable))
            .cloned()
    }

    /// Run at caret (⌃⇧R / Ctrl+Shift+F10).
    pub(super) fn run_at_caret(&mut self, _: &super::RunAtCaret, _: &mut Window, cx: &mut Context<Self>) {
        match self.target_at_caret(false) {
            Some(target) => cx.emit(EditorRunEvent { target, debug: false, menu_at: None }),
            None => cx.emit(EditorLspEvent::Message("Nothing to run here".into())),
        }
    }

    /// Debug at caret (⌃⇧D / Ctrl+Shift+F9).
    pub(super) fn debug_at_caret(&mut self, _: &super::DebugAtCaret, _: &mut Window, cx: &mut Context<Self>) {
        match self.target_at_caret(true) {
            Some(target) => cx.emit(EditorRunEvent { target, debug: true, menu_at: None }),
            None => cx.emit(EditorLspEvent::Message("Nothing to debug here".into())),
        }
    }

    // ---- Breakpoints ---------------------------------------------------------------

    /// Breakpoints only go where a debugger can stop.
    pub fn can_break(&self) -> bool {
        self.mode == EditorMode::Full
            && self.doc.path.is_some()
            && matches!(
                self.doc.language,
                Language::Rust | Language::Go | Language::Python | Language::JavaScript | Language::TypeScript | Language::Tsx
            )
    }

    pub fn toggle_breakpoint_at(&mut self, row: usize, cx: &mut Context<Self>) {
        self.doc.toggle_breakpoint(row);
        cx.emit(EditorBreakpointsChanged);
        cx.notify();
    }

    /// Toggle Line Breakpoint (⌘F8 / F9) on the caret's line.
    pub(super) fn toggle_breakpoint(&mut self, _: &super::ToggleBreakpoint, _: &mut Window, cx: &mut Context<Self>) {
        if !self.can_break() {
            cx.emit(EditorLspEvent::Message("Breakpoints work in Rust, Go, Python, JavaScript and TypeScript files".into()));
            return;
        }
        let mut rows: Vec<usize> = self.selections.iter().map(|s| self.doc.row_of(s.head)).collect();
        rows.dedup();
        for row in rows {
            self.doc.toggle_breakpoint(row);
        }
        cx.emit(EditorBreakpointsChanged);
        cx.notify();
    }

    pub fn breakpoints(&self) -> Vec<Breakpoint> {
        self.doc.breakpoints()
    }

    /// Restores breakpoints (a file opening, a session).
    pub fn set_breakpoints(&mut self, breakpoints: &[Breakpoint], cx: &mut Context<Self>) {
        self.doc.set_breakpoints(breakpoints);
        cx.notify();
    }

    pub fn breakpoint(&self, row: usize) -> Option<Breakpoint> {
        self.doc.breakpoint(row).cloned()
    }

    /// Adds or changes one breakpoint (a condition or log message was edited).
    pub fn set_breakpoint(&mut self, breakpoint: Breakpoint, cx: &mut Context<Self>) {
        self.doc.set_breakpoint(breakpoint);
        cx.emit(EditorBreakpointsChanged);
        cx.notify();
    }

    /// The caret's row.
    pub fn caret_row(&self) -> usize {
        self.doc.row_of(self.primary_selection().head)
    }

    /// Highlights the line a debug session paused on (None: running or done).
    pub fn set_debug_line(&mut self, row: Option<usize>, cx: &mut Context<Self>) {
        if self.doc.debug_row() != row {
            self.doc.set_debug_row(row);
            cx.notify();
        }
    }
}
