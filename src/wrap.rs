//! Display rows: where long buffer lines break into several rows (soft
//! wrap) and which lines are hidden by folds (zero rows).
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
    /// Break long lines (off when only folds need the map).
    pub wrap: bool,
}

pub struct WrapMap {
    pub key: WrapKey,
    /// The document version the breaks are for.
    pub version: u64,
    /// The folded byte ranges whose lines take no rows.
    folds: Vec<Range<usize>>,
    /// Breaks per buffer line; empty for lines that fit.
    breaks: Vec<Vec<Break>>,
    /// First display row of each line, plus the total at the end.
    row_starts: Vec<usize>,
}

/// Finds a line's breaks; only lines that may be wider than the view are
/// shaped.
struct Measure {
    wrapper: LineWrapperHandle,
    width: Pixels,
    max_cols: usize,
    tab_size: usize,
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
        }
    }

    fn line(&mut self, rope: &Rope, row: usize) -> Vec<Break> {
        let line = rope.line(row);
        if line.len_chars() <= self.max_cols / self.tab_size || display_columns(line, self.tab_size) <= self.max_cols {
            return Vec::new();
        }
        // Same text as `Document::line_text`: without the newline.
        let text = line.to_string();
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let dl = DisplayLine::new(text, self.tab_size);
        let fragments = [LineFragment::text(&dl.text)];
        self.wrapper.wrap_line(&fragments, self.width).map(|b| Break { ix: b.ix as u32, indent: b.next_indent }).collect()
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
        let breaks = if key.wrap {
            let mut measure = Measure::new(&key, text_system, font, char_width);
            (0..rope.len_lines()).map(|row| measure.line(rope, row)).collect()
        } else {
            vec![Vec::new(); rope.len_lines()]
        };
        let mut map = WrapMap { key, version, folds: folds.to_vec(), breaks, row_starts: Vec::new() };
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
        if self.key.wrap && first < dirty.len() {
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
                rows += 1 + self.breaks[row].len();
            }
        }
        self.row_starts.push(rows);
    }

    pub fn row_count(&self) -> usize {
        *self.row_starts.last().unwrap_or(&1)
    }

    /// Whether a fold hides buffer line `row`.
    pub fn hidden(&self, row: usize) -> bool {
        matches!((self.row_starts.get(row), self.row_starts.get(row + 1)), (Some(a), Some(b)) if a == b)
    }

    pub fn breaks(&self, row: usize) -> &[Break] {
        self.breaks.get(row).map_or(&[], Vec::as_slice)
    }

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
            key: WrapKey { width: gpui::px(100.), font_family: "".into(), font_size: gpui::px(14.), tab_size: 4, wrap: true },
            version: 0,
            folds: Vec::new(),
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

    #[test]
    fn folded_lines_take_no_rows() {
        // Lines 1 and 2 are hidden: row_starts repeats.
        let map = WrapMap {
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
