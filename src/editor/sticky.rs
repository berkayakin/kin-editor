//! Where the caret is in the file's structure: the outline (File Structure's
//! symbols, with their ranges) cached per version, the symbol chain shown as
//! breadcrumbs, and sticky scroll, which keeps the header lines of the scopes
//! scrolled into pinned at the top of the editor.

use std::sync::Arc;

use gpui::{Bounds, Context, Pixels, Point, SharedString, TextRun, fill, point, px, size};

use super::{DisplayLine, Editor, FrameLayout};
use crate::{syntax::Symbol, theme::Theme};

/// At most this many pinned lines (fewer in short editors).
const MAX_STICKY: usize = 4;

impl Editor {
    /// The file's symbols with their ranges, recomputed when the text changes.
    pub(super) fn outline(&mut self) -> Arc<Vec<Symbol>> {
        let version = self.doc.version();
        if let Some((v, symbols)) = &self.outline_cache
            && *v == version
        {
            return symbols.clone();
        }
        let symbols = Arc::new(self.doc.syntax().symbols(self.doc.rope()));
        self.outline_cache = Some((version, symbols.clone()));
        symbols
    }

    /// The definitions around the caret, outermost first: name and where it starts.
    pub fn breadcrumbs(&mut self) -> Vec<(SharedString, usize)> {
        let head = self.primary_selection().head;
        self.outline()
            .iter()
            .filter(|s| s.range.start <= head && head <= s.range.end)
            .map(|s| (SharedString::from(s.name.clone()), s.offset))
            .collect()
    }

    /// The header rows to pin when `top_row` is the first visible row: scopes
    /// that start above it and continue past it, innermost last.
    fn sticky_headers(&mut self, top_row: usize, max: usize) -> Vec<(usize, usize)> {
        let outline = self.outline();
        let mut rows = Vec::new();
        // The pinned lines cover rows of their own, so look below them too.
        for _ in 0..3 {
            let top = top_row + rows.len();
            let mut next: Vec<(usize, usize)> = outline
                .iter()
                .map(|s| (self.doc.row_of(s.offset), s))
                .filter(|(row, s)| *row < top && self.doc.row_of(s.range.end) > top)
                .map(|(row, s)| (row, s.offset))
                .collect();
            next.dedup_by_key(|(row, _)| *row);
            if next.len() > max {
                next.drain(..next.len() - max);
            }
            if next.len() == rows.len() {
                return next;
            }
            rows = next;
        }
        rows
    }

    /// Lays out the pinned lines over the top of the text (backgrounds, text
    /// and line numbers) and remembers where they are for clicks.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn layout_sticky(
        &mut self,
        frame: &mut FrameLayout,
        bounds: Bounds<Pixels>,
        top_row: usize,
        text_left: Pixels,
        number_right: Pixels,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.sticky_rows.clear();
        let line_height = frame.line_height;
        let max = MAX_STICKY.min((bounds.size.height / line_height / 4.).floor() as usize);
        if max == 0 || self.scroll.y <= px(0.) {
            return;
        }
        let headers = self.sticky_headers(top_row, max);
        if headers.is_empty() {
            return;
        }
        let theme = Theme::get(cx).clone();
        let text_system = window.text_system().clone();
        let font = self.editor_font(cx);
        let font_size = self.font_size(cx);
        let width = bounds.size.width - px(super::SCROLLBAR_WIDTH);
        for (i, &(row, offset)) in headers.iter().enumerate() {
            let y = bounds.top() + line_height * i as f32;
            let row_bounds = Bounds::new(point(bounds.left(), y), size(width, line_height));
            frame.sticky_bgs.push(fill(row_bounds, theme.background));
            self.sticky_rows.push((row_bounds, offset));

            let line_start = self.doc.line_start(row);
            let text = self.doc.line_text(row);
            let dl = DisplayLine::new(&text, self.tab_size);
            let highlights = self.doc.syntax().highlights(self.doc.rope(), line_start..line_start + text.len());
            // Highlighted like the line itself (without diagnostics).
            let mut runs = Vec::new();
            let mut at = 0;
            let push = |runs: &mut Vec<TextRun>, at: &mut usize, end: usize, color: gpui::Hsla| {
                let len = dl.to_display(end) - dl.to_display(*at);
                if len > 0 {
                    runs.push(TextRun { len, font: font.clone(), color, background_color: None, underline: None, strikethrough: None });
                }
                *at = end;
            };
            for (range, kind) in &highlights {
                let s = range.start.saturating_sub(line_start).min(text.len());
                let e = range.end.saturating_sub(line_start).min(text.len());
                if s < at {
                    continue;
                }
                push(&mut runs, &mut at, s, theme.text.into());
                push(&mut runs, &mut at, e, theme.highlight_color(*kind));
            }
            push(&mut runs, &mut at, text.len(), theme.text.into());
            let shaped = text_system.shape_line(SharedString::from(dl.text.clone()), font_size, &runs, None);
            frame.sticky_lines.push((point(text_left - self.scroll.x, y), shaped));

            let number = SharedString::from((row + 1).to_string());
            let run = TextRun { len: number.len(), font: font.clone(), color: theme.line_number.into(), background_color: None, underline: None, strikethrough: None };
            let number_line = text_system.shape_line(number, font_size, &[run], None);
            frame.sticky_numbers.push((point(number_right - number_line.width, y), number_line));
        }
        // A thin line under the pinned rows separates them from the scrolling text.
        let bottom = bounds.top() + line_height * headers.len() as f32;
        frame.sticky_bgs.push(fill(Bounds::new(point(bounds.left(), bottom - px(1.)), size(width, px(1.))), theme.border));
    }

    /// The pinned line at `position`, if any: where its definition starts.
    pub(super) fn sticky_at(&self, position: Point<Pixels>) -> Option<usize> {
        self.sticky_rows.iter().find(|(b, _)| b.contains(&position)).map(|(_, offset)| *offset)
    }
}
