//! Code folding regions: by indentation for code and data (works for every
//! language, like VS Code's default), by heading for Markdown. A closing
//! bracket or tag at the region's own indent folds with it, so a folded
//! function reads `fn main() { ⋯ }`.

use std::ops::Range;

use crate::document::{Document, Patch};

/// One editor's folded text (two editors of a file fold apart, as in Zed):
/// from the start of the first hidden line to the end of the last, sorted.
/// Edits move folds; an edit touching one opens it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Folds(Vec<Range<usize>>);

impl Folds {
    pub fn ranges(&self) -> &[Range<usize>] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Hides rows `start..=end` of `doc` (never the first line).
    pub fn fold_rows(&mut self, doc: &Document, start: usize, end: usize) {
        if start == 0 || start >= doc.line_count() {
            return;
        }
        let range = doc.line_start(start)..doc.line_end(end.min(doc.line_count() - 1));
        if !range.is_empty() && !self.0.contains(&range) {
            self.0.push(range);
            self.0.sort_by_key(|f| f.start);
        }
    }

    /// Opens the folds `unfold` matches; returns whether any opened.
    pub fn unfold_where(&mut self, mut unfold: impl FnMut(&Range<usize>) -> bool) -> bool {
        let before = self.0.len();
        self.0.retain(|f| !unfold(f));
        self.0.len() != before
    }

    /// Follows a transaction's edits: folds after them move, ones they touch open.
    pub fn shift(&mut self, patch: &Patch) {
        self.0.retain_mut(|f| {
            let (mut delta, near) = patch.around(f.clone());
            for (range, new_len) in near {
                if range.end < f.start {
                    delta += Patch::change(range, *new_len);
                } else if range.start <= f.end {
                    return false;
                }
            }
            let moved = |offset: usize| (offset as isize + delta).max(0) as usize;
            *f = moved(f.start)..moved(f.end);
            true
        });
    }

    /// Catches up with `doc`'s edits since `version`; all open when those
    /// aren't kept any more.
    pub fn follow(&mut self, doc: &Document, version: u64) {
        match doc.patches_since(version) {
            Some(patches) => patches.for_each(|patch| self.shift(patch)),
            None => self.0.clear(),
        }
    }
}

/// Lines that can fold under a header line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldRegion {
    /// The header row, which stays visible.
    pub start: usize,
    /// The last hidden row.
    pub end: usize,
    /// The folded-in closing line (`}`, `);`, `</div>`), shown after the
    /// placeholder.
    pub closing: Option<String>,
}

/// Fold regions of `text`, sorted by start row. Rows are `\n`-separated lines.
pub fn regions(text: &str, tab_size: usize, markdown: bool) -> Vec<FoldRegion> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = if markdown { heading_regions(&lines) } else { indent_regions(&lines, tab_size) };
    out.sort_by_key(|r| r.start);
    out
}

fn indent_of(line: &str, tab_size: usize) -> usize {
    let mut col = 0;
    for c in line.chars() {
        match c {
            ' ' => col += 1,
            '\t' => col += tab_size - col % tab_size,
            _ => break,
        }
    }
    col
}

fn indent_regions(lines: &[&str], tab_size: usize) -> Vec<FoldRegion> {
    let blank = |row: usize| lines[row].trim().is_empty();
    let mut regions = Vec::new();
    // Open headers: (indent, row).
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut last_nonblank: Option<usize> = None;
    let close = |start: usize, end: Option<usize>, regions: &mut Vec<FoldRegion>| {
        if let Some(end) = end.filter(|&e| e > start) {
            regions.push(FoldRegion { start, end, closing: None });
        }
    };
    for (row, line) in lines.iter().enumerate() {
        if blank(row) {
            continue;
        }
        let indent = indent_of(line, tab_size);
        while let Some(&(open_indent, open_row)) = stack.last() {
            if open_indent < indent {
                break;
            }
            stack.pop();
            close(open_row, last_nonblank, &mut regions);
        }
        stack.push((indent, row));
        last_nonblank = Some(row);
    }
    while let Some((_, open_row)) = stack.pop() {
        close(open_row, last_nonblank, &mut regions);
    }
    // Take in the closing line when it sits at the header's indent.
    for region in &mut regions {
        let header_indent = indent_of(lines[region.start], tab_size);
        let Some(next) = (region.end + 1..lines.len()).find(|&r| !blank(r)) else { continue };
        let text = lines[next].trim();
        let closes = text.starts_with(['}', ']', ')']) || text.starts_with("</");
        if closes && indent_of(lines[next], tab_size) == header_indent {
            region.end = next;
            region.closing = Some(text.to_string());
        }
    }
    regions
}

/// A Markdown section: a heading and everything up to the next heading of
/// the same or a higher level. Headings inside code fences don't count.
fn heading_regions(lines: &[&str]) -> Vec<FoldRegion> {
    let mut headings: Vec<(usize, usize)> = Vec::new(); // (row, level)
    let mut in_fence = false;
    for (row, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let level = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&level) && trimmed[level..].starts_with([' ', '\t']) {
            headings.push((row, level));
        }
    }
    let mut regions = Vec::new();
    for (i, &(row, level)) in headings.iter().enumerate() {
        let next = headings[i + 1..].iter().find(|(_, l)| *l <= level).map_or(lines.len(), |(r, _)| *r);
        // Trailing blank lines stay visible.
        let end = (row + 1..next).rev().find(|&r| !lines[r].trim().is_empty());
        if let Some(end) = end {
            regions.push(FoldRegion { start: row, end, closing: None });
        }
    }
    regions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(text: &str) -> Vec<(usize, usize, Option<String>)> {
        regions(text, 4, false).into_iter().map(|r| (r.start, r.end, r.closing)).collect()
    }

    fn close(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn folds_by_indentation_with_closing_brackets() {
        let rust = "fn main() {\n    let x = 1;\n\n    if x > 0 {\n        print();\n    }\n}\n";
        assert_eq!(spans(rust), [(0, 6, close("}")), (3, 5, close("}"))]);
        let python = "def f():\n    a = 1\n    return a\n\nx = 2\n";
        assert_eq!(spans(python), [(0, 2, None)]);
        let yaml = "jobs:\n  build:\n    runs-on: x\n  test:\n    runs-on: y\n";
        assert_eq!(spans(yaml), [(0, 4, None), (1, 2, None), (3, 4, None)]);
        let html = "<div>\n  <p>hi</p>\n</div>\n";
        assert_eq!(spans(html), [(0, 2, close("</div>"))]);
        // A one-line block has nothing to fold.
        assert!(spans("a {\n}\n").is_empty());
    }

    #[test]
    fn folds_markdown_sections() {
        let md = "# Title\n\nText\n\n## A\n\nmore\n\n```sh\n# not a heading\n```\n\n## B\nend\n";
        let got: Vec<(usize, usize)> = regions(md, 4, true).iter().map(|r| (r.start, r.end)).collect();
        assert_eq!(got, [(0, 13), (4, 10), (12, 13)]);
    }
}
