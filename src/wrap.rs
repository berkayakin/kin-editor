//! Soft wrap: where long buffer lines break into several display rows.
//!
//! Positions are byte offsets into a line's [`DisplayLine`] text (tabs
//! expanded). A break's offset starts a new row, so a cursor exactly on a
//! break shows at the start of the next row. Continuation rows keep the
//! line's indentation, like Zed and VS Code.

use gpui::{Font, LineFragment, Pixels, SharedString, TextSystem};
use ropey::Rope;

use crate::editor::DisplayLine;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Break {
    /// Display-text byte offset where the row starts.
    pub ix: u32,
    /// Leading spaces drawn before the row's text.
    pub indent: u32,
}

/// What a wrap map was computed for; any change means recomputing.
#[derive(Clone, PartialEq)]
pub struct WrapKey {
    pub version: u64,
    pub len: usize,
    pub width: Pixels,
    pub font_family: SharedString,
    pub font_size: Pixels,
    pub tab_size: usize,
}

pub struct WrapMap {
    pub key: WrapKey,
    /// Breaks per buffer line; empty for lines that fit.
    breaks: Vec<Vec<Break>>,
    /// First display row of each line, plus the total at the end.
    row_starts: Vec<usize>,
}

impl WrapMap {
    pub fn new(key: WrapKey, rope: &Rope, text_system: &std::sync::Arc<TextSystem>, font: Font, char_width: Pixels) -> Self {
        let (width, tab_size) = (key.width, key.tab_size);
        let mut wrapper = text_system.line_wrapper(font, key.font_size);
        // Lines narrower than this many columns can't wrap, which skips
        // shaping-free measurement for most lines.
        let max_cols = (width / char_width).floor().max(1.) as usize;
        let lines = rope.len_lines();
        let mut breaks = Vec::with_capacity(lines);
        let mut row_starts = Vec::with_capacity(lines + 1);
        let mut rows = 0;
        for row in 0..lines {
            row_starts.push(rows);
            let line = rope.line(row);
            let mut line_breaks = Vec::new();
            if line.len_chars() > max_cols / tab_size && display_columns(line, tab_size) > max_cols {
                // Same text as `Document::line_text`: without the newline.
                let text = line.to_string();
                let text = text.strip_suffix('\n').unwrap_or(&text);
                let dl = DisplayLine::new(text, tab_size);
                let fragments = [LineFragment::text(&dl.text)];
                line_breaks.extend(
                    wrapper
                        .wrap_line(&fragments, width)
                        .map(|b| Break { ix: b.ix as u32, indent: b.next_indent }),
                );
            }
            rows += 1 + line_breaks.len();
            breaks.push(line_breaks);
        }
        row_starts.push(rows);
        Self { key, breaks, row_starts }
    }

    pub fn row_count(&self) -> usize {
        *self.row_starts.last().unwrap_or(&1)
    }

    pub fn breaks(&self, row: usize) -> &[Break] {
        self.breaks.get(row).map_or(&[], Vec::as_slice)
    }

    /// The first display row of buffer line `row`.
    /// The first display row of buffer line `row`; one past the last line
    /// gives the total.
    pub fn first_display_row(&self, row: usize) -> usize {
        self.row_starts.get(row).copied().unwrap_or_else(|| self.row_count())
    }

    /// The buffer line and its segment index shown on display row `drow`.
    pub fn line_of(&self, drow: usize) -> (usize, usize) {
        let lines = self.row_starts.len().saturating_sub(1);
        if lines == 0 {
            return (0, 0);
        }
        let row = self.row_starts[..lines].partition_point(|&s| s <= drow).saturating_sub(1);
        let seg = (drow - self.row_starts[row]).min(self.breaks(row).len());
        (row, seg)
    }
}

/// Display columns of a rope line (tabs expanded, wide characters as two),
/// an upper bound for whether it can need wrapping.
fn display_columns(line: ropey::RopeSlice, tab_size: usize) -> usize {
    let mut col = 0;
    for c in line.chars() {
        col += match c {
            '\t' => tab_size - col % tab_size,
            '\n' => 0,
            c if (c as u32) >= 0x1100 => 2,
            _ => 1,
        };
    }
    col
}

/// The display-text range and indent of segment `seg` of a line with `breaks`.
pub fn segment(breaks: &[Break], seg: usize, len: usize) -> (std::ops::Range<usize>, u32) {
    let start = if seg == 0 { 0 } else { breaks[seg - 1].ix as usize };
    let end = breaks.get(seg).map_or(len, |b| b.ix as usize);
    let indent = if seg == 0 { 0 } else { breaks[seg - 1].indent };
    (start.min(len)..end.min(len).max(start.min(len)), indent)
}

/// The segment containing display column `col`.
pub fn segment_of(breaks: &[Break], col: usize) -> usize {
    breaks.partition_point(|b| b.ix as usize <= col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_cover_the_line() {
        let breaks = [Break { ix: 10, indent: 4 }, Break { ix: 20, indent: 4 }];
        assert_eq!(segment(&breaks, 0, 25), (0..10, 0));
        assert_eq!(segment(&breaks, 1, 25), (10..20, 4));
        assert_eq!(segment(&breaks, 2, 25), (20..25, 4));
        assert_eq!(segment_of(&breaks, 0), 0);
        assert_eq!(segment_of(&breaks, 9), 0);
        assert_eq!(segment_of(&breaks, 10), 1);
        assert_eq!(segment_of(&breaks, 25), 2);
        assert_eq!(segment(&[], 0, 7), (0..7, 0));
    }

    #[test]
    fn counts_display_columns() {
        let rope = Rope::from_str("\tab\n世界\n");
        assert_eq!(display_columns(rope.line(0), 4), 6);
        assert_eq!(display_columns(rope.line(0), 2), 4);
        assert_eq!(display_columns(rope.line(1), 4), 4);
    }

    #[test]
    fn maps_display_rows_to_lines() {
        let map = WrapMap {
            key: WrapKey { version: 0, len: 0, width: gpui::px(100.), font_family: "".into(), font_size: gpui::px(14.), tab_size: 4 },
            breaks: vec![vec![], vec![Break { ix: 5, indent: 0 }, Break { ix: 9, indent: 0 }], vec![]],
            row_starts: vec![0, 1, 4, 5],
        };
        assert_eq!(map.row_count(), 5);
        assert_eq!(map.line_of(0), (0, 0));
        assert_eq!(map.line_of(1), (1, 0));
        assert_eq!(map.line_of(3), (1, 2));
        assert_eq!(map.line_of(4), (2, 0));
        assert_eq!(map.line_of(99), (2, 0));
        assert_eq!(map.first_display_row(2), 4);
    }
}
