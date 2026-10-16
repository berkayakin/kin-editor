//! Display rows: where long buffer lines break into several rows (soft
//! wrap), which lines are hidden by folds (zero rows), and block rows: rows
//! between lines that show something else than the text (a hunk's deleted
//! lines), which carets skip.
//!
//! Positions are byte offsets into a line's [`DisplayLine`] text (tabs
//! expanded, inlay hints in place: they take room on their rows like text).
//! A break's offset starts a new row, so a cursor exactly on a break shows at
//! the start of the next row. Continuation rows keep the
//! line's indentation, like VS Code.

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    ops::Range,
};

use gpui::{Font, LineFragment, LineWrapperHandle, Pixels, SharedString, TextSystem};
use ropey::Rope;

use crate::{
    document::{InlayHint, LineEdit},
    editor::DisplayLine,
};

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
/// (Edits, folds and inlay hints update the map in place.)
#[derive(Clone, PartialEq)]
pub struct WrapKey {
    /// The width to wrap at; zero without soft wrap (long lines break at a
    /// fixed column then).
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
    /// The hints each line was measured with (see `fingerprint`), to tell
    /// which lines new hints change.
    inlays: Vec<u64>,
    /// The `Inlays::generation` the breaks are for.
    inlay_generation: u64,
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

/// The inlay hints shown in the text, which lines are measured with.
#[derive(Clone, Copy)]
pub struct Inlays<'a> {
    /// Sorted by offset; empty when hints are off.
    pub hints: &'a [InlayHint],
    /// `Document::inlay_generation`, 0 when hints are off: a change means
    /// looking for the lines whose hints changed. (Hints following edits
    /// stay on the edited lines, which are measured again anyway.)
    pub generation: u64,
}

impl<'a> Inlays<'a> {
    pub const NONE: Inlays<'static> = Inlays { hints: &[], generation: 0 };

    /// Line `row`'s start and its hints: from its start to its end, the
    /// newline excluded (as `Editor::display_line` shows them).
    pub fn of_line(&self, rope: &Rope, row: usize) -> (usize, &'a [InlayHint]) {
        let row = row.min(rope.len_lines());
        let start = rope.line_to_byte(row);
        let end = if row + 1 >= rope.len_lines() { rope.len_bytes() } else { rope.line_to_byte(row + 1) - 1 };
        let first = self.hints.partition_point(|h| h.offset < start);
        let count = self.hints[first..].partition_point(|h| h.offset <= end);
        (start, &self.hints[first..first + count])
    }

    /// Every line's `fingerprint`, walking the hints rather than the lines.
    fn fingerprints(&self, rope: &Rope) -> Vec<u64> {
        let mut out = vec![0; rope.len_lines()];
        let shown = &self.hints[..self.hints.partition_point(|h| h.offset <= rope.len_bytes())];
        for line in shown.chunk_by(|a, b| rope.byte_to_line(a.offset) == rope.byte_to_line(b.offset)) {
            let row = rope.byte_to_line(line[0].offset);
            out[row] = fingerprint(rope.line_to_byte(row), line);
        }
        out
    }
}

/// A line's hints (starting at `start`) as a number, 0 without any: what its
/// breaks depend on besides its text.
fn fingerprint(start: usize, hints: &[InlayHint]) -> u64 {
    if hints.is_empty() {
        return 0;
    }
    let mut hasher = DefaultHasher::new();
    for h in hints {
        (h.offset - start, &h.label, h.pad_left, h.pad_right).hash(&mut hasher);
    }
    hasher.finish().max(1)
}

/// Finds a line's breaks; only lines that may be wider than the view are
/// shaped.
struct Measure<'a> {
    wrapper: LineWrapperHandle,
    width: Pixels,
    char_width: Pixels,
    max_cols: usize,
    tab_size: usize,
    wrap: bool,
    inlays: Inlays<'a>,
}

impl<'a> Measure<'a> {
    fn new(key: &WrapKey, inlays: Inlays<'a>, text_system: &std::sync::Arc<TextSystem>, font: Font, char_width: Pixels) -> Self {
        Measure {
            wrapper: text_system.line_wrapper(font, key.font_size),
            width: key.width,
            char_width,
            // Lines narrower than this many columns can't wrap, which skips
            // shaping-free measurement for most lines.
            max_cols: (key.width / char_width).floor().max(1.) as usize,
            tab_size: key.tab_size,
            wrap: key.wrap,
            inlays,
        }
    }

    /// Line `row`'s breaks and the `fingerprint` of the hints they're for.
    fn line(&mut self, rope: &Rope, row: usize) -> (Vec<Break>, u64) {
        let (start, hints) = self.inlays.of_line(rope, row);
        (self.breaks(rope, row, start, hints), fingerprint(start, hints))
    }

    fn breaks(&mut self, rope: &Rope, row: usize, start: usize, hints: &[InlayHint]) -> Vec<Break> {
        let line = rope.line(row);
        let long = line.len_bytes() > LONG_LINE;
        if !self.wrap && !long {
            return Vec::new();
        }
        let hint_cols: usize = hints.iter().map(|h| display_columns(h.label.chars(), 1) + h.pad_left as usize + h.pad_right as usize).sum();
        if self.wrap
            && ((hint_cols == 0 && line.len_chars() <= self.max_cols / self.tab_size) || display_columns(line.chars(), self.tab_size) + hint_cols <= self.max_cols)
        {
            return Vec::new();
        }
        // Same text as `Document::line_text`: without the newline.
        let text = line.to_string();
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let inlays: Vec<(usize, &str, bool, bool)> = hints.iter().map(|h| (h.offset - start, h.label.as_str(), h.pad_left, h.pad_right)).collect();
        let dl = DisplayLine::with_inlays(text, self.tab_size, &inlays);
        let at = |ix: usize, indent: u32| Break { ix: ix as u32, byte: dl.to_buffer(ix) as u32, indent };
        if !self.wrap {
            return dl.text.char_indices().map(|(ix, _)| ix).step_by(LONG_LINE_ROW).skip(1).map(|ix| at(ix, 0)).collect();
        }
        // A hint is one piece: rows break before or after it, not inside.
        let mut fragments = Vec::with_capacity(2 * dl.inlays.len() + 1);
        let mut from = 0;
        for span in dl.inlays.iter().filter(|s| !s.whole.is_empty()) {
            if span.whole.start > from {
                fragments.push(LineFragment::text(&dl.text[from..span.whole.start]));
            }
            let width = self.char_width * display_columns(dl.text[span.whole.clone()].chars(), 1) as f32;
            fragments.push(LineFragment::element(width, span.whole.len()));
            from = span.whole.end;
        }
        if from < dl.text.len() {
            fragments.push(LineFragment::text(&dl.text[from..]));
        }
        self.wrapper.wrap_line(&fragments, self.width).map(|b| at(b.ix, b.next_indent)).collect()
    }
}

impl WrapMap {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        key: WrapKey,
        version: u64,
        folds: &[Range<usize>],
        inlays: Inlays,
        rope: &Rope,
        text_system: &std::sync::Arc<TextSystem>,
        font: Font,
        char_width: Pixels,
    ) -> Self {
        let mut measure = Measure::new(&key, inlays, text_system, font, char_width);
        let (breaks, fingerprints) = (0..rope.len_lines()).map(|row| measure.line(rope, row)).unzip();
        let mut map = WrapMap {
            key,
            version,
            folds: folds.to_vec(),
            blocks: Vec::new(),
            breaks,
            inlays: fingerprints,
            inlay_generation: inlays.generation,
            row_starts: Vec::new(),
        };
        map.count_rows(rope, 0);
        map
    }

    /// Catches up with `edits` (from `self.version` to `version`, see
    /// `Document::line_edits_since`), `folds` and `inlays`: only the lines
    /// the edits touched or whose hints changed are measured again, and rows
    /// are counted from the first of them. False when the edits don't fit
    /// this map; then build a new one.
    #[allow(clippy::too_many_arguments)]
    pub fn update<'e>(
        &mut self,
        edits: impl Iterator<Item = &'e LineEdit>,
        version: u64,
        folds: &[Range<usize>],
        inlays: Inlays,
        rope: &Rope,
        text_system: &std::sync::Arc<TextSystem>,
        font: Font,
        char_width: Pixels,
    ) -> bool {
        // Asked every frame; usually nothing changed.
        if version == self.version && self.folds == folds && inlays.generation == self.inlay_generation {
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
            self.inlays.splice(old.clone(), std::iter::repeat_n(0, edit.new_rows));
            dirty.splice(old, std::iter::repeat_n(true, edit.new_rows));
            first = first.min(edit.row);
        }
        if self.breaks.len() != rope.len_lines() {
            return false;
        }
        // New hints: the lines whose hints aren't the ones they were measured
        // with (edited lines are already due).
        if inlays.generation != self.inlay_generation {
            for (row, now) in inlays.fingerprints(rope).into_iter().enumerate() {
                if self.inlays[row] != now && !dirty[row] {
                    dirty[row] = true;
                    first = first.min(row);
                }
            }
            self.inlay_generation = inlays.generation;
        }
        if first < dirty.len() {
            let mut measure = Measure::new(&self.key, inlays, text_system, font, char_width);
            for row in (first..dirty.len()).filter(|&row| dirty[row]) {
                (self.breaks[row], self.inlays[row]) = measure.line(rope, row);
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

    /// The block rows above buffer line `row` (`blocks` is sorted by line).
    fn blocks_above(&self, row: usize) -> usize {
        let first = self.blocks.partition_point(|&(line, _)| line < row);
        self.blocks[first..].iter().take_while(|&&(line, _)| line == row).map(|(_, n)| n).sum()
    }

    /// The block row on display row `drow`: (the line it's above, which of
    /// its block rows, counting all of that line's blocks).
    pub fn block_at(&self, drow: usize) -> Option<(usize, usize)> {
        let lines = self.row_starts.len().saturating_sub(1);
        let total = self.row_count();
        if drow >= total {
            return None;
        }
        // Blocks after the last line end the map.
        let trailing = self.blocks_above(lines);
        if drow >= total - trailing {
            return Some((lines, drow - (total - trailing)));
        }
        let line = self.line_at(drow);
        if line >= lines || self.hidden(line) {
            return None;
        }
        let start = self.row_starts[line];
        (drow < start + self.blocks_above(line)).then(|| (line, drow - start))
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
        // Blocks are sorted by line: walk them alongside the lines.
        let mut block = self.blocks.partition_point(|&(line, _)| line < from);
        for row in from..lines {
            while let Some(&(_, last)) = hidden.get(next).filter(|(first, _)| *first <= row) {
                hidden_to = hidden_to.max(Some(last));
                next += 1;
            }
            let mut above = 0;
            while let Some(&(_, n)) = self.blocks.get(block).filter(|&&(line, _)| line == row) {
                above += n;
                block += 1;
            }
            self.row_starts.push(rows);
            if hidden_to.is_none_or(|to| row > to) {
                rows += above + 1 + self.breaks[row].len();
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

/// Display columns of a line's characters (tabs expanded, wide characters
/// as two), an upper bound for whether it can need wrapping.
fn display_columns(chars: impl Iterator<Item = char>, tab_size: usize) -> usize {
    let mut col = 0;
    for c in chars {
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
        fn inlays(doc: &Document) -> Inlays<'_> {
            Inlays { hints: doc.inlay_hints(), generation: doc.inlay_generation() }
        }
        let text_system = cx.update(|cx| cx.text_system().clone());
        let font = gpui::font("Test");
        let key = |wrap| WrapKey { width: gpui::px(300.), font_family: "Test".into(), font_size: gpui::px(14.), tab_size: 4, wrap };
        let long = "word ".repeat(40);
        let text: String = (0..30).map(|i| if i % 3 == 0 { format!("{i} {long}\n") } else { format!("line {i}\n") }).collect();
        let line = |doc: &Document, row: usize| doc.line_start(row);
        // Hints at (line, column): a type in a long line, one too wide for
        // its short line, parameter names.
        let hints = |d: &Document, at: &[(usize, usize, &str)]| -> Vec<InlayHint> {
            at.iter().map(|&(row, col, label)| InlayHint { offset: line(d, row) + col, label: label.into(), pad_left: true, pad_right: false }).collect()
        };
        for wrap in [true, false] {
            let mut doc = Document::from_text(text.clone(), None);
            doc.set_inlay_hints(hints(&doc, &[(0, 1, ": Vec<String>"), (4, 6, &long), (8, 2, "name: ")]));
            let mut folds = crate::fold::Folds::default();
            folds.fold_rows(&doc, 10, 12);
            let mut map = WrapMap::new(key(wrap), doc.version(), folds.ranges(), inlays(&doc), doc.rope(), &text_system, font.clone(), gpui::px(7.));
            assert_eq!(wrap, map.breaks.iter().any(|b| !b.is_empty()), "long lines wrap");
            assert_eq!(wrap, !map.breaks(4).is_empty(), "a short line wraps with a wide hint");
            let steps: Vec<Box<dyn Fn(&mut Document)>> = vec![
                // Typing in a long line, then several carets adding lines.
                Box::new(|d| drop(d.edit(vec![(line(d, 3) + 5..line(d, 3) + 5, "more ".repeat(20))], &[], EditKind::Other))),
                // New hints: the wide one moves to another line, one more.
                Box::new(|d| d.set_inlay_hints(hints(d, &[(0, 1, ": Vec<String>"), (7, 6, &long), (8, 2, "name: "), (9, 0, "p: ")]))),
                Box::new(|d| drop(d.edit(vec![(line(d, 1)..line(d, 1), "a\nb\n".into()), (line(d, 20)..line(d, 20), long.clone())], &[], EditKind::Other))),
                // Typing where a hint is (it stays after the text), deleting
                // text around one (it goes).
                Box::new(|d| {
                    let at = d.inlay_hints()[2].offset;
                    drop(d.edit(vec![(at..at, "abc".into())], &[], EditKind::Other));
                }),
                Box::new(|d| drop(d.edit(vec![(line(d, 0)..line(d, 0) + 3, String::new())], &[], EditKind::Other))),
                // Joining lines, an edit that opens the fold, undo and redo.
                Box::new(|d| drop(d.edit(vec![(line(d, 5) - 1..line(d, 7), String::new())], &[], EditKind::Other))),
                Box::new(|d| drop(d.edit(vec![(line(d, 11) + 1..line(d, 11) + 1, "x".into())], &[], EditKind::Other))),
                Box::new(|d| drop(d.undo())),
                Box::new(|d| drop(d.redo())),
                // An edit and new hints between two frames, then no hints.
                Box::new(|d| {
                    drop(d.edit(vec![(line(d, 2)..line(d, 2), long.clone())], &[], EditKind::Other));
                    d.set_inlay_hints(hints(d, &[(2, 3, &long), (15, 0, "x: ")]));
                }),
                Box::new(|d| d.set_inlay_hints(Vec::new())),
            ];
            for step in steps {
                // Two versions at a time, as when frames are skipped.
                let before = doc.version();
                step(&mut doc);
                step(&mut doc);
                folds.follow(&doc, before);
                let edits = doc.line_edits_since(map.version).expect("kept");
                assert!(map.update(edits, doc.version(), folds.ranges(), inlays(&doc), doc.rope(), &text_system, font.clone(), gpui::px(7.)));
                let fresh = WrapMap::new(key(wrap), doc.version(), folds.ranges(), inlays(&doc), doc.rope(), &text_system, font.clone(), gpui::px(7.));
                assert_eq!((&map.breaks, &map.inlays, &map.row_starts), (&fresh.breaks, &fresh.inlays, &fresh.row_starts));
            }
            // Hints switched off and on again.
            doc.set_inlay_hints(hints(&doc, &[(4, 2, &long)]));
            for shown in [Inlays::NONE, inlays(&doc)] {
                let edits = doc.line_edits_since(map.version).expect("kept");
                assert!(map.update(edits, doc.version(), folds.ranges(), shown, doc.rope(), &text_system, font.clone(), gpui::px(7.)));
                let fresh = WrapMap::new(key(wrap), doc.version(), folds.ranges(), shown, doc.rope(), &text_system, font.clone(), gpui::px(7.));
                assert_eq!((&map.breaks, &map.inlays, &map.row_starts), (&fresh.breaks, &fresh.inlays, &fresh.row_starts));
            }
        }
    }

    /// A hint takes room on its row and wraps as one piece; buffer columns
    /// map to the rows the display shows them on, before a hint.
    #[gpui::test]
    fn hints_take_room_on_wrapped_rows(cx: &mut gpui::TestAppContext) {
        let text_system = cx.update(|cx| cx.text_system().clone());
        let key = WrapKey { width: gpui::px(300.), font_family: "Test".into(), font_size: gpui::px(14.), tab_size: 4, wrap: true };
        let text = "let value = compute(first, second);";
        let rope = Rope::from_str(text);
        let label = ": Result<Vec<String>, Error>";
        let hints = [InlayHint { offset: 9, label: label.into(), pad_left: false, pad_right: false }];
        let new = |inlays| WrapMap::new(key.clone(), 0, &[], inlays, &rope, &text_system, gpui::font("Test"), gpui::px(7.));
        assert!(new(Inlays::NONE).breaks(0).is_empty(), "fits without its hint");
        let map = new(Inlays { hints: &hints, generation: 1 });
        let breaks = map.breaks(0);
        assert!(!breaks.is_empty(), "wraps with it");
        let dl = DisplayLine::with_inlays(text, 4, &[(9, label, false, false)]);
        let hint = dl.inlays[0].whole.clone();
        for b in breaks {
            assert!(b.ix as usize <= hint.start || b.ix as usize >= hint.end, "not inside the hint: {b:?}");
            assert_eq!(dl.to_buffer(b.ix as usize), b.byte as usize);
        }
        // Every buffer column shows on the row that holds its display column,
        // and a click there comes back to it.
        for col in (0..=text.len()).filter(|&c| text.is_char_boundary(c)) {
            let shown = dl.to_display(col);
            let (range, _) = segment(breaks, segment_of(breaks, shown), dl.text.len());
            assert!(range.contains(&shown) || shown == range.end, "column {col}");
            assert_eq!(dl.to_buffer(shown), col);
        }
        // The hint's column is before the hint; a click inside it goes there.
        assert_eq!(dl.to_display(9), hint.start);
        assert_eq!(dl.to_buffer(hint.start + 3), 9);
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
        let map = WrapMap::new(key.clone(), doc.version(), &[], Inlays::NONE, doc.rope(), &text_system, gpui::font("Test"), gpui::px(7.));
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
        assert_eq!(display_columns(rope.line(0).chars(), 4), 6);
        assert_eq!(display_columns(rope.line(0).chars(), 2), 4);
        assert_eq!(display_columns(rope.line(1).chars(), 4), 4);
    }

    #[test]
    fn maps_display_rows_to_lines() {
        let map = WrapMap {
            blocks: Vec::new(),
            key: WrapKey { width: gpui::px(100.), font_family: "".into(), font_size: gpui::px(14.), tab_size: 4, wrap: true },
            version: 0,
            folds: Vec::new(),
            breaks: vec![vec![], vec![Break { ix: 5, byte: 5, indent: 0 }, Break { ix: 9, byte: 9, indent: 0 }], vec![]],
            inlays: vec![0; 3],
            inlay_generation: 0,
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
        let mut map = cx.update(|cx| WrapMap::new(key, 0, &[], Inlays::NONE, &rope, cx.text_system(), gpui::font("Menlo"), gpui::px(8.)));
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

    /// Lookups by binary search and the walk in `count_rows` agree with
    /// counting every row: several blocks on one line, blocks above folded
    /// lines (no rows) and after the last line.
    #[gpui::test]
    fn block_lookups_match_counting_rows(cx: &mut gpui::TestAppContext) {
        let rope = Rope::from_str(&"x\n".repeat(12));
        let key = WrapKey { width: gpui::px(0.), font_family: "Menlo".into(), font_size: gpui::px(13.), tab_size: 4, wrap: false };
        // Lines 5 and 6 folded away (the fold covers them whole).
        let fold = rope.line_to_byte(5)..rope.line_to_byte(7) - 1;
        let mut map = cx.update(|cx| WrapMap::new(key, 0, std::slice::from_ref(&fold), Inlays::NONE, &rope, cx.text_system(), gpui::font("Menlo"), gpui::px(8.)));
        let blocks = [(0, 1), (2, 2), (2, 1), (5, 3), (9, 1), (13, 2)];
        map.set_blocks(&blocks, &rope);
        // Every display row by hand: (line, Some(block row)) or (line, None)
        // for its text; one past the last line holds the blocks after it.
        let lines = rope.len_lines();
        let mut rows: Vec<(usize, Option<usize>)> = Vec::new();
        for line in (0..=lines).filter(|line| !(5..=6).contains(line)) {
            let above: usize = blocks.iter().filter(|(l, _)| *l == line).map(|(_, n)| n).sum();
            rows.extend((0..above).map(|i| (line, Some(i))));
            if line < lines {
                rows.push((line, None));
            }
        }
        assert_eq!(map.row_count(), rows.len());
        for (drow, &(line, block)) in rows.iter().enumerate() {
            assert_eq!(map.block_at(drow), block.map(|i| (line, i)), "row {drow}");
            if block.is_none() {
                assert_eq!(map.first_display_row(line), drow, "line {line}");
            }
        }
        assert_eq!(map.block_at(rows.len()), None);
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
            inlays: vec![0; 4],
            inlay_generation: 0,
            row_starts: vec![0, 1, 1, 1, 2],
        };
        assert_eq!(map.row_count(), 2);
        assert!(map.hidden(1) && map.hidden(2) && !map.hidden(0) && !map.hidden(3));
        assert_eq!(map.line_of(1), (3, 0));
        assert_eq!(map.first_display_row(3), 1);
    }
}
