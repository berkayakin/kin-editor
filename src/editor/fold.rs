//! Code folding in the editor: Collapse / Expand actions, the gutter's fold
//! arrows and the `⋯` placeholder. Regions come from `crate::fold`; each
//! editor keeps its own folds (`Folds`), which follow the document's edits.

use std::{ops::Range, sync::Arc};

use gpui::{App, Context, Window};

use super::{Editor, Selection};
use crate::fold::{self, FoldRegion};

impl Editor {
    /// Fold regions for the current text (cached per version).
    pub(super) fn fold_regions(&mut self, cx: &App) -> Arc<Vec<FoldRegion>> {
        let (key, job) = self.fold_job(cx);
        self.fold_regions.now(key, job)
    }

    /// The fold regions to draw: the last ones computed (see `Derived`).
    pub(super) fn shown_fold_regions(&mut self, cx: &mut Context<Self>) -> Arc<Vec<FoldRegion>> {
        let (key, job) = self.fold_job(cx);
        self.fold_regions.schedule(key, job, |e| &mut e.fold_regions, cx);
        self.fold_regions.last()
    }

    fn fold_job(&self, cx: &App) -> ((u64, usize, crate::syntax::Language), impl FnOnce() -> Vec<FoldRegion> + Send + 'static) {
        let (rope, tab_size, language) = (self.doc.read(cx).rope().clone(), self.tab_size, self.doc.read(cx).language);
        let job = move || fold::regions(&rope.to_string(), tab_size, language == crate::syntax::Language::Markdown);
        ((self.doc.read(cx).version(), tab_size, language), job)
    }

    /// Whether a fold hides buffer line `row`.
    pub(super) fn row_hidden(&self, row: usize) -> bool {
        self.wrap.as_ref().is_some_and(|w| w.hidden(row))
    }

    /// The fold whose header is `row`.
    pub(super) fn fold_under(&self, row: usize, cx: &App) -> Option<Range<usize>> {
        let start = self.doc.read(cx).line_start(row + 1);
        // Sorted by start: drawing asks for every line on screen.
        let folds = self.folds.ranges();
        folds.get(folds.partition_point(|f| f.start < start)).filter(|f| f.start == start).cloned()
    }

    fn fold(&mut self, region: &FoldRegion, cx: &mut Context<Self>) {
        self.folds.fold_rows(self.doc.read(cx), region.start + 1, region.end);
        // Carets inside move to the end of the header line.
        let header_end = self.doc.read(cx).line_end(region.start);
        let hidden = self.doc.read(cx).line_start(region.start + 1)..self.doc.read(cx).line_end(region.end);
        let out = |o: usize| if hidden.contains(&o) || o == hidden.end { header_end } else { o };
        let selections: Vec<Selection> =
            self.selections.iter().map(|s| Selection { anchor: out(s.anchor), head: out(s.head) }).collect();
        self.set_selections(selections, self.primary, cx);
        self.after_fold_change(cx);
    }

    fn after_fold_change(&mut self, cx: &mut Context<Self>) {
        self.autoscroll = true;
        cx.notify();
    }

    /// Opens the fold under header `row`, or folds the region starting there.
    pub(super) fn toggle_fold_at(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(fold) = self.fold_under(row, cx) {
            self.folds.unfold_where(|f| *f == fold);
            self.after_fold_change(cx);
        } else if let Some(region) = self.fold_regions(cx).iter().find(|r| r.start == row).cloned() {
            self.fold(&region, cx);
        }
    }

    /// Opens folds a caret moved into (Go to Line, search, arrow keys).
    pub(super) fn unfold_at_carets(&mut self) {
        if self.folds.is_empty() {
            return;
        }
        let offsets: Vec<usize> = self.selections.iter().flat_map(|s| [s.anchor, s.head]).collect();
        self.folds.unfold_where(|f| offsets.iter().any(|o| f.contains(o) || *o == f.end));
    }

    /// Collapse (⌘-): the region at the caret's line, or the innermost open
    /// region around it.
    pub(super) fn collapse_region(&mut self, _: &super::CollapseRegion, _: &mut Window, cx: &mut Context<Self>) {
        let row = self.doc.read(cx).row_of(self.primary_selection().head);
        let regions = self.fold_regions(cx);
        let open = |r: &&FoldRegion| self.fold_under(r.start, cx).is_none();
        let target = regions
            .iter()
            .filter(open)
            .find(|r| r.start == row)
            .or_else(|| regions.iter().filter(open).filter(|r| r.start < row && row <= r.end).min_by_key(|r| r.end - r.start))
            .cloned();
        if let Some(region) = target {
            self.fold(&region, cx);
        }
    }

    /// Expand (⌘=): the fold at the caret's line.
    pub(super) fn expand_region(&mut self, _: &super::ExpandRegion, _: &mut Window, cx: &mut Context<Self>) {
        let row = self.doc.read(cx).row_of(self.primary_selection().head);
        if let Some(fold) = self.fold_under(row, cx) {
            self.folds.unfold_where(|f| *f == fold);
            self.after_fold_change(cx);
        }
    }

    pub(super) fn collapse_all(&mut self, _: &super::CollapseAllRegions, _: &mut Window, cx: &mut Context<Self>) {
        // Inner regions first, so expanding an outer one shows them folded.
        let mut regions: Vec<FoldRegion> = self.fold_regions(cx).iter().cloned().collect();
        regions.sort_by_key(|r| std::cmp::Reverse(r.start));
        for region in &regions {
            self.fold(region, cx);
        }
    }

    pub(super) fn expand_all(&mut self, _: &super::ExpandAllRegions, _: &mut Window, cx: &mut Context<Self>) {
        if self.folds.unfold_where(|_| true) {
            self.after_fold_change(cx);
        }
    }

    /// Folds as (header row, last hidden row), for the session.
    pub fn folded_rows(&self, cx: &App) -> Vec<[usize; 2]> {
        self.folds.ranges().iter().map(|f| [self.doc.read(cx).row_of(f.start).saturating_sub(1), self.doc.read(cx).row_of(f.end)]).collect()
    }

    /// Folds saved rows again, where the file still has those regions.
    pub fn restore_folds(&mut self, rows: &[[usize; 2]], cx: &mut Context<Self>) {
        let regions = self.fold_regions(cx);
        for [start, end] in rows {
            if let Some(region) = regions.iter().find(|r| r.start == *start && r.end == *end).cloned() {
                self.fold(&region, cx);
            }
        }
    }

    /// The placeholder after a folded header: `⋯`, plus a folded-in closing
    /// line (`⋯ }`).
    pub(super) fn fold_placeholder(&self, fold: &Range<usize>, cx: &App) -> String {
        let last = self.doc.read(cx).row_of(fold.end);
        let closing = self.doc.read(cx).line_text(last);
        let closing = closing.trim();
        if closing.starts_with(['}', ']', ')']) || closing.starts_with("</") {
            format!("⋯ {closing}")
        } else {
            "⋯".to_string()
        }
    }
}
