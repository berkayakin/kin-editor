//! Display rows: where long buffer lines break into several rows (soft
//! wrap), which lines are hidden by folds (zero rows), and block rows: rows
//! between lines that show something else than the text (a hunk's deleted
//! lines), which carets skip.
//!
//! Positions are byte offsets into a line's [`DisplayLine`] text (tabs
//! expanded). A break's offset starts a new row, so a cursor exactly on a
//! break shows at the start of the next row. Continuation rows keep the
//! line's indentation, like Zed and VS Code.

use std::ops::Range;

use gpui::{Font, LineFragment, LineWrapperHandle, Pixels, SharedString, TextSystem};
use ropey::Rope;

use crate::{document::LineEdit, editor::DisplayLine};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Break {
    /// Display-text byte offset where the row starts.
    pub ix: u32,
    /// The same place as a byte offset in the line's own text.
    pub byte: u32,
    /// Leading spaces drawn before the row's text.
    pub indent: u32,
}

/// What line breaks depend on; a change means measuring every line again.
/// (Edits and folds update the map in place.)
#[derive(Clone, PartialEq)]
pub struct WrapKey {
    pub width: Pixels,
    pub font_family: SharedString,
    pub font_size: Pixels,
    pub tab_size: usize,
    /// Break lines at the view's width (off when only folds or long lines
    /// need the map).
    pub wrap: bool,
}

pub struct WrapMap {
    pub key: WrapKey,
    /// The document version the breaks are for.
    pub version: u64,
    /// The folded byte ranges whose lines take no rows.
    folds: Vec<Range<usize>>,
    /// Block rows above buffer lines: (line, rows), sorted; a line one past
    /// the last means after it.
    blocks: Vec<(usize, usize)>,
    /// Breaks per buffer line; empty for lines that fit.
    breaks: Vec<Vec<Break>>,
    /// First display row of each line (its block rows, then its text),
    /// plus the total at the end.
    row_starts: Vec<usize>,
}

/// A line longer than this (in bytes) always breaks into rows, soft wrap or
/// not: shaping, highlighting and drawing it whole each frame is too slow
/// (minified code, one-line JSON).
pub const LONG_LINE: usize = 1024;
/// The most display columns on a row of a long line without soft wrap.
const LONG_LINE_ROW: usize = 512;

/// Finds a line's breaks; only lines that may be wider than the view are
/// shaped.
struct Measure {
    wrapper: LineWrapperHandle,
    width: Pixels,
    max_cols: usize,
    tab_size: usize,
    wrap: bool,
}

impl Measure {
    fn new(key: &WrapKey, text_system: &std::sync::Arc<TextSystem>, font: Font, char_width: Pixels) -> Self {
        Measure {
            wrapper: text_system.line_wrapper(font, key.font_size),
            width: key.width,
            // Lines narrower than this many columns can't wrap, which skips
            // shaping-free measurement for most lines.
            max_cols: (key.width / char_width).floor().max(1.) as usize,
            tab_size: key.tab_size,
            wrap: key.wrap,
        }
    }

    fn line(&mut self, rope: &Rope, row: usize) -> Vec<Break> {
        let line = rope.line(row);
        let long = line.len_bytes() > LONG_LINE;
        if !self.wrap && !long {
            return Vec::new();
        }
        if self.wrap && (line.len_chars() <= self.max_cols / self.tab_size || display_columns(line, self.tab_size) <= self.max_cols) {
            return Vec::new();
        }
        // Same text as `Document::line_text`: without the newline.
        let text = line.to_string();
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let dl = DisplayLine::new(text, self.tab_size);
        let at = |ix: usize, indent: u32| Break { ix: ix as u32, byte: dl.to_buffer(ix) as u32, indent };
        if !self.wrap {
            return dl.text.char_indices().map(|(ix, _)| ix).step_by(LONG_LINE_ROW).skip(1).map(|ix| at(ix, 0)).collect();
        }
        let fragments = [LineFragment::text(&dl.text)];
        self.wrapper.wrap_line(&fragments, self.width).map(|b| at(b.ix, b.next_indent)).collect()
    }
}

impl WrapMap {
    pub fn new(
        key: WrapKey,
        version: u64,
        folds: &[Range<usize>],
        rope: &Rope,
        text_system: &std::sync::Arc<TextSystem>,
        font: Font,
        char_width: Pixels,
    ) -> Self {
        let mut measure = Measure::new(&key, text_system, font, char_width);
        let breaks = (0..rope.len_lines()).map(|row| measure.line(rope, row)).collect();
        let mut map = WrapMap { key, version, folds: folds.to_vec(), blocks: Vec::new(), breaks, row_starts: Vec::new() };
        map.count_rows(rope, 0);
        map
    }

    /// Catches up with `edits` (from `self.version` to `version`, see
    /// `Document::line_edits_since`) and `folds`: only the lines the edits
    /// touched are measured again, and rows are counted from the first of
    /// them. False when the edits don't fit this map; then build a new one.
    #[allow(clippy::too_many_arguments)]
    pub fn update<'e>(
        &mut self,
        edits: impl Iterator<Item = &'e LineEdit>,
        version: u64,
        folds: &[Range<usize>],
        rope: &Rope,
        text_system: &std::sync::Arc<TextSystem>,
        font: Font,
        char_width: Pixels,
    ) -> bool {
        // Asked every frame; usually nothing changed.
        if version == self.version && self.folds == folds {
            return true;
        }
        let mut dirty = vec![false; self.breaks.len()];
        let mut first = self.breaks.len();
        for edit in edits {
            let old = edit.row..edit.row + edit.old_rows;
            if old.end > self.breaks.len() {
                return false;
            }
            self.breaks.splice(old.clone(), std::iter::repeat_with(Vec::new).take(edit.new_rows));
            dirty.splice(old, std::iter::repeat_n(true, edit.new_rows));
            first = first.min(edit.row);
        }
        if self.breaks.len() != rope.len_lines() {
            return false;
        }
        if first < dirty.len() {
            let mut measure = Measure::new(&self.key, text_system, font, char_width);
            for row in (first..dirty.len()).filter(|&row| dirty[row]) {
                self.breaks[row] = measure.line(rope, row);
            }
        }
        self.version = version;
        // Folds that moved or opened can hide rows above the first edit.
        if self.folds != folds {
            self.folds = folds.to_vec();
            first = 0;
        }
        self.count_rows(rope, first);
        true
    }

    /// Sets the block rows (see `blocks`); rows are counted again from the
    /// first line whose blocks changed.
    pub fn set_blocks(&mut self, blocks: &[(usize, usize)], rope: &Rope) {
        if self.blocks == blocks {
            return;
        }
        let first = self.blocks.iter().zip(blocks).position(|(a, b)| a != b).unwrap_or(self.blocks.len().min(blocks.len()));
        let from = [self.blocks.get(first), blocks.get(first)].into_iter().flatten().map(|(line, _)| *line).min().unwrap_or(0);
        self.blocks = blocks.to_vec();
        self.count_rows(rope, from);
    }

    /// The first display row of the blocks above `line` (where its text
    /// starts when it has none).
    pub fn block_start(&self, line: usize) -> usize {
        let lines = self.row_starts.len().saturating_sub(1);
        if line < lines { self.row_starts[line] } else { self.row_count() - self.blocks_above(lines) }
    }

    fn blocks_above(&self, row: usize) -> usize {
        self.blocks.iter().filter(|(line, _)| *line == row).map(|(_, n)| n).sum()
    }

    /// The block row on display row `drow`: (the line it's above, which of
    /// its rows).
    pub fn block_at(&self, drow: usize) -> Option<(usize, usize)> {
        let lines = self.row_starts.len().saturating_sub(1);
        self.blocks.iter().find_map(|&(line, rows)| {
            if line < lines && self.hidden(line) {
                return None;
            }
            let start = if line < lines { self.row_starts[line] } else { self.row_count() - rows };
            (start..start + rows).contains(&drow).then(|| (line, drow - start))
        })
    }

    /// Display rows from buffer line `from` on (the ones before it stand).
    fn count_rows(&mut self, rope: &Rope, from: usize) {
        let lines = self.breaks.len();
        let last_line = lines.saturating_sub(1);
        // Rows hidden by folds take no display rows: (first, last) per fold.
        let mut hidden: Vec<(usize, usize)> = self
            .folds
            .iter()
            .map(|f| (rope.byte_to_line(f.start.min(rope.len_bytes())), rope.byte_to_line(f.end.min(rope.len_bytes())).min(last_line)))
            .collect();
        hidden.sort_unstable();
        let from = from.min(self.row_starts.len().saturating_sub(1));
        let mut rows = self.row_starts.get(from).copied().unwrap_or(0);
        self.row_starts.truncate(from);
        let (mut next, mut hidden_to) = (0, None);
        for row in from..lines {
            while let Some(&(_, last)) = hidden.get(next).filter(|(first, _)| *first <= row) {
                hidden_to = hidden_to.max(Some(last));
                next += 1;
            }
            self.row_starts.push(rows);
            if hidden_to.is_none_or(|to| row > to) {
                rows += self.blocks_above(row) + 1 + self.breaks[row].len();
            }
        }
        // Blocks after the last line count in the total.
        self.row_starts.push(rows + self.blocks_above(lines));
    }

    pub fn row_count(&self) -> usize {
        *self.row_starts.last().unwrap_or(&1)
    }

    /// Whether a fold hides buffer line `row`.
    pub fn hidden(&self, row: usize) -> bool {
        if self.row_starts.len() <= row + 1 {
            return false;
        }
        matches!((self.row_starts.get(row), self.row_starts.get(row + 1)), (Some(a), Some(b)) if a == b)
    }

    pub fn breaks(&self, row: usize) -> &[Break] {
        self.breaks.get(row).map_or(&[], Vec::as_slice)
    }

    /// The first display row of buffer line `row`; one past the last line
    /// gives the total.
    pub fn first_display_row(&self, row: usize) -> usize {
        let lines = self.row_starts.len().saturating_sub(1);
        if row >= lines {
            return self.row_count();
        }
        let start = self.row_starts[row];
        if self.hidden(row) { start } else { start + self.blocks_above(row) }
    }

    /// Where buffer line `row`'s rows start: the block rows above it, if it
    /// has any, else its text.
    pub fn rows_start(&self, row: usize) -> usize {
        self.row_starts.get(row).copied().unwrap_or_else(|| self.row_count())
    }

    /// The buffer line whose rows (block rows above it included) hold
    /// display row `drow`.
    pub fn line_at(&self, drow: usize) -> usize {
        let lines = self.row_starts.len().saturating_sub(1);
        self.row_starts[..lines].partition_point(|&s| s <= drow).saturating_sub(1)
    }

    /// The buffer line and its segment index shown on display row `drow`.
    pub fn line_of(&self, drow: usize) -> (usize, usize) {
        let lines = self.row_starts.len().saturating_sub(1);
        if lines == 0 {
            return (0, 0);
        }
        let row = self.row_starts[..lines].partition_point(|&s| s <= drow).saturating_sub(1);
        let text = self.first_display_row(row);
        if drow < text {
            // A block row counts as the end of the shown line above it (the
            // top of the first line for blocks above it).
            return match (0..row).rev().find(|&r| !self.hidden(r)) {
                Some(above) => (above, self.breaks(above).len()),
                None => (row, 0),
            };
        }
        (row, (drow - text).min(self.breaks(row).len()))
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

    #[gpui::test]
    fn edits_update_the_map_like_a_new_one(cx: &mut gpui::TestAppContext) {
        use crate::document::{Document, EditKind};
        let text_system = cx.update(|cx| cx.text_system().clone());
        let font = gpui::font("Test");
        let key = |wrap| WrapKey { width: gpui::px(300.), font_family: "Test".into(), font_size: gpui::px(14.), tab_size: 4, wrap };
        let long = "word ".repeat(40);
        let text: String = (0..30).map(|i| if i % 3 == 0 { format!("{i} {long}\n") } else { format!("line {i}\n") }).collect();
        for wrap in [true, false] {
            let mut doc = Document::from_text(text.clone(), None);
            doc.fold_rows(10, 12);
            let mut map = WrapMap::new(key(wrap), doc.version(), doc.folds(), doc.rope(), &text_system, font.clone(), gpui::px(7.));
            assert_eq!(wrap, map.breaks.iter().any(|b| !b.is_empty()), "long lines wrap");
            let line = |doc: &Document, row: usize| doc.line_start(row);
            let steps: Vec<Box<dyn Fn(&mut Document)>> = vec![
                // Typing in a long line, then several carets adding lines.
                Box::new(|d| drop(d.edit(vec![(line(d, 3) + 5..line(d, 3) + 5, "more ".repeat(20))], &[], EditKind::Other))),
                Box::new(|d| drop(d.edit(vec![(line(d, 1)..line(d, 1), "a\nb\n".into()), (line(d, 20)..line(d, 20), long.clone())], &[], EditKind::Other))),
                // Joining lines, an edit that opens the fold, undo and redo.
                Box::new(|d| drop(d.edit(vec![(line(d, 5) - 1..line(d, 7), String::new())], &[], EditKind::Other))),
                Box::new(|d| drop(d.edit(vec![(line(d, 11) + 1..line(d, 11) + 1, "x".into())], &[], EditKind::Other))),
                Box::new(|d| drop(d.undo())),
                Box::new(|d| drop(d.redo())),
            ];
            for step in steps {
                // Two versions at a time, as when frames are skipped.
                step(&mut doc);
                step(&mut doc);
                let edits = doc.line_edits_since(map.version).expect("kept");
                assert!(map.update(edits, doc.version(), doc.folds(), doc.rope(), &text_system, font.clone(), gpui::px(7.)));
                let fresh = WrapMap::new(key(wrap), doc.version(), doc.folds(), doc.rope(), &text_system, font.clone(), gpui::px(7.));
                assert_eq!((&map.breaks, &map.row_starts), (&fresh.breaks, &fresh.row_starts));
            }
        }
    }

    #[gpui::test]
    fn long_lines_break_without_soft_wrap(cx: &mut gpui::TestAppContext) {
        use crate::document::{Document, EditKind};
        let text_system = cx.update(|cx| cx.text_system().clone());
        let key = WrapKey { width: gpui::px(300.), font_family: "Test".into(), font_size: gpui::px(14.), tab_size: 4, wrap: false };
        // A one-line JSON file of 1300 characters, "é" (two bytes) among them.
        let json = format!("[{}é]", "1,".repeat(649));
        let mut doc = Document::from_text(format!("short\n{json}\n"), None);
        assert!(doc.has_long_lines());
        let map = WrapMap::new(key.clone(), doc.version(), doc.folds(), doc.rope(), &text_system, gpui::font("Test"), gpui::px(7.));
        assert!(map.breaks(0).is_empty());
        assert_eq!(map.breaks(1), [Break { ix: 512, byte: 512, indent: 0 }, Break { ix: 1024, byte: 1024, indent: 0 }]);
        assert_eq!(map.row_count(), 1 + 3 + 1);
        // Shortened below the limit, it's an ordinary line again.
        let start = doc.line_start(1);
        doc.edit(vec![(start + 10..start + 1000, String::new())], &[], EditKind::Other);
        assert!(!doc.has_long_lines());
    }

    #[test]
    fn segments_cover_the_line() {
        let breaks = [Break { ix: 10, byte: 10, indent: 4 }, Break { ix: 20, byte: 20, indent: 4 }];
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
            blocks: Vec::new(),
            key: WrapKey { width: gpui::px(100.), font_family: "".into(), font_size: gpui::px(14.), tab_size: 4, wrap: true },
            version: 0,
            folds: Vec::new(),
            breaks: vec![vec![], vec![Break { ix: 5, byte: 5, indent: 0 }, Break { ix: 9, byte: 9, indent: 0 }], vec![]],
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

    #[gpui::test]
    fn block_rows_sit_between_lines(cx: &mut gpui::TestAppContext) {
        let rope = Rope::from_str("a\nb\nc\n");
        let key = WrapKey { width: gpui::px(400.), font_family: "Menlo".into(), font_size: gpui::px(13.), tab_size: 4, wrap: false };
        let mut map = cx.update(|cx| WrapMap::new(key, 0, &[], &rope, cx.text_system(), gpui::font("Menlo"), gpui::px(8.)));
        // Two rows above line 1 ("b"), one after the last line.
        map.set_blocks(&[(1, 2), (4, 1)], &rope);
        assert_eq!(map.row_count(), 4 + 3);
        assert_eq!(map.first_display_row(1), 3);
        assert_eq!((map.block_at(1), map.block_at(2), map.block_at(3)), (Some((1, 0)), Some((1, 1)), None));
        assert_eq!(map.block_at(6), Some((4, 0)));
        // A block row maps to the line above it, for clicks and carets.
        assert_eq!(map.line_of(2), (0, 0));
        assert_eq!(map.line_of(3), (1, 0));
        map.set_blocks(&[], &rope);
        assert_eq!((map.row_count(), map.first_display_row(1)), (4, 1));
    }

    #[test]
    fn folded_lines_take_no_rows() {
        // Lines 1 and 2 are hidden: row_starts repeats.
        let map = WrapMap {
            blocks: Vec::new(),
            key: WrapKey { width: gpui::px(100.), font_family: "".into(), font_size: gpui::px(14.), tab_size: 4, wrap: false },
            version: 0,
            folds: Vec::new(),
            breaks: vec![vec![], vec![], vec![], vec![]],
            row_starts: vec![0, 1, 1, 1, 2],
        };
        assert_eq!(map.row_count(), 2);
        assert!(map.hidden(1) && map.hidden(2) && !map.hidden(0) && !map.hidden(3));
        assert_eq!(map.line_of(1), (3, 0));
        assert_eq!(map.first_display_row(3), 1);
    }
}
