//! Text storage, undo history and file I/O. Offsets are UTF-8 byte offsets
//! throughout so they line up with tree-sitter and oxc spans.

use std::{
    ops::Range,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use ropey::Rope;
use serde::{Deserialize, Serialize};

use crate::{
    encoding::{self, TextEncoding},
    lint::Diagnostic,
    syntax::{Language, Syntax, input_edit},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn cursor(at: usize) -> Self {
        Self {
            anchor: at,
            head: at,
        }
    }

    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn reversed(&self) -> bool {
        self.head < self.anchor
    }
}

/// A line breakpoint. A `condition` makes it stop only when that expression
/// is true; a `log_message` makes it a logpoint, which logs (with
/// `{expression}`s filled in) instead of stopping.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Breakpoint {
    /// 0-based.
    pub row: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_message: Option<String>,
}

impl Breakpoint {
    pub fn at(row: usize) -> Self {
        Self { row, ..Self::default() }
    }
}

struct EditRecord {
    /// Position at which the edit was applied (in the coordinates of the
    /// buffer at application time).
    start: usize,
    old: String,
    new: String,
}

struct Transaction {
    edits: Vec<EditRecord>,
    before: Vec<Selection>,
    after: Vec<Selection>,
    at: Instant,
    kind: EditKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EditKind {
    Typing,
    Deleting,
    Other,
}

/// Above this, a file opens as plain text without language features.
const LARGE_FILE_SIZE: u64 = 20_000_000;
/// Above this, it doesn't open at all (it would take that much memory twice over).
const MAX_FILE_SIZE: u64 = 500_000_000;

pub struct Document {
    rope: Rope,
    pub path: Option<PathBuf>,
    pub language: Language,
    syntax: Syntax,
    version: u64,
    saved_version: u64,
    undo_stack: Vec<Transaction>,
    redo_stack: Vec<Transaction>,
    /// Merged view of built-in and external diagnostics, sorted by position.
    pub diagnostics: Vec<Diagnostic>,
    builtin_diagnostics: Vec<Diagnostic>,
    external_diagnostics: Vec<Diagnostic>,
    lsp_diagnostics: Vec<Diagnostic>,
    pub uses_crlf: bool,
    /// The file's encoding, used again when saving.
    pub encoding: TextEncoding,
    /// Big enough that highlighting, linting and language servers are off.
    pub large: bool,
    /// Edits since the language server last heard about this document.
    lsp_changes: LspChanges,
    /// Folded (hidden) text: from the start of the first hidden line to the
    /// end of the last. Edits that touch a fold open it.
    folds: Vec<Range<usize>>,
    /// Line breakpoints with the offsets their lines start at (sorted), so
    /// they stay on their lines as text is edited above them.
    breakpoints: Vec<(usize, Breakpoint)>,
    /// The tab stops of a snippet being filled in; they follow edits.
    pub snippet: Option<crate::snippets::SnippetStops>,
    /// Modification time of the file when it was last read or written, to
    /// notice changes made by other programs.
    disk_mtime: Option<SystemTime>,
    /// An image shown as a picture: no text is read, edited or written; its
    /// file is the content (Save As copies it).
    pub image: Option<PathBuf>,
}

impl Document {
    pub fn empty() -> Self {
        Self::from_text(String::new(), None)
    }

    pub fn open(path: &Path) -> std::io::Result<Self> {
        if crate::preview::is_image(path) {
            std::fs::metadata(path)?;
            let mut doc = Self::from_text(String::new(), Some(path.to_path_buf()));
            doc.disk_mtime = mtime(path);
            doc.image = Some(path.to_path_buf());
            return Ok(doc);
        }
        let size = std::fs::metadata(path)?.len();
        if size > MAX_FILE_SIZE {
            return Err(std::io::Error::other(format!("{} MB is too large to open", size / 1_000_000)));
        }
        let bytes = std::fs::read(path)?;
        let (text, encoding) = encoding::decode(&bytes, None);
        let mut doc = Self::from_text(text, Some(path.to_path_buf()));
        doc.encoding = encoding;
        if size > LARGE_FILE_SIZE {
            // Shown as plain text: no parsing, linting or language server.
            doc.large = true;
            doc.language = Language::PlainText;
            doc.syntax = Syntax::new(Language::PlainText, &doc.rope);
        }
        doc.disk_mtime = mtime(path);
        Ok(doc)
    }

    fn from_text(text: String, path: Option<PathBuf>) -> Self {
        // Normalize to LF internally; remember the style for saving.
        let uses_crlf = text.contains("\r\n");
        let text = if uses_crlf {
            text.replace("\r\n", "\n")
        } else {
            text
        };
        let rope = Rope::from_str(&text);
        let language = path
            .as_deref()
            .map(Language::from_path)
            .unwrap_or(Language::PlainText);
        let syntax = Syntax::new(language, &rope);
        Self {
            rope,
            path,
            language,
            syntax,
            version: 0,
            saved_version: 0,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            diagnostics: Vec::new(),
            builtin_diagnostics: Vec::new(),
            external_diagnostics: Vec::new(),
            lsp_diagnostics: Vec::new(),
            uses_crlf,
            encoding: TextEncoding::UTF8,
            large: false,
            lsp_changes: LspChanges::default(),
            folds: Vec::new(),
            breakpoints: Vec::new(),
            snippet: None,
            disk_mtime: None,
            image: None,
        }
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn syntax(&self) -> &Syntax {
        &self.syntax
    }

    pub fn len(&self) -> usize {
        self.rope.len_bytes()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }

    pub fn file_name(&self) -> String {
        self.path
            .as_deref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into())
    }

    pub fn set_path(&mut self, path: PathBuf) {
        let language = Language::from_path(&path);
        self.path = Some(path);
        if language != self.language {
            self.set_language(language);
        }
    }

    /// Parses again as `language` (an extension's grammar can arrive later).
    pub fn set_language(&mut self, language: Language) {
        self.language = language;
        self.syntax = Syntax::new(language, &self.rope);
        self.set_builtin_diagnostics(Vec::new());
        self.set_external_diagnostics(Vec::new());
    }

    /// Replaces the contents with the file on disk. Returns whether anything
    /// changed. Undo history is cleared since it no longer applies.
    pub fn reload_from_disk(&mut self) -> std::io::Result<bool> {
        let Some(path) = self.path.clone() else { return Ok(false) };
        if self.image.is_some() {
            let changed = mtime(&path) != self.disk_mtime;
            self.disk_mtime = mtime(&path);
            return Ok(changed);
        }
        let bytes = std::fs::read(&path)?;
        let (text, encoding) = encoding::decode(&bytes, Some(self.encoding));
        self.encoding = encoding;
        self.disk_mtime = mtime(&path);
        Ok(self.replace_all(text.replace("\r\n", "\n")))
    }

    /// Reads the file again as `encoding` (when it was guessed wrong).
    /// Unsaved edits are lost; callers ask first.
    pub fn reload_with_encoding(&mut self, encoding: TextEncoding) -> std::io::Result<()> {
        let path = self.path.clone().ok_or_else(|| std::io::Error::other("document has no path"))?;
        let bytes = std::fs::read(&path)?;
        self.encoding = encoding;
        self.disk_mtime = mtime(&path);
        self.replace_all(encoding::decode_as(&bytes, encoding).replace("\r\n", "\n"));
        self.saved_version = self.version;
        Ok(())
    }

    /// Saves in another encoding from now on (the next save converts).
    pub fn convert_to(&mut self, encoding: TextEncoding) {
        if self.encoding != encoding {
            self.encoding = encoding;
            self.mark_unsaved();
        }
    }

    /// Saves with Windows (CRLF) or Unix (LF) line endings from now on.
    pub fn set_crlf(&mut self, crlf: bool) {
        if self.uses_crlf != crlf {
            self.uses_crlf = crlf;
            self.mark_unsaved();
        }
    }

    /// A change that isn't an edit (encoding, line endings) still needs a save.
    fn mark_unsaved(&mut self) {
        self.saved_version = u64::MAX;
    }

    /// Replaces all text, as when the file changed on disk. Returns whether
    /// anything changed; undo history goes since it no longer applies.
    fn replace_all(&mut self, text: String) -> bool {
        if text == self.rope.to_string() {
            return false;
        }
        // Breakpoints keep their line numbers across a reload.
        let breakpoints = self.breakpoints();
        self.rope = Rope::from_str(&text);
        self.set_breakpoints(&breakpoints);
        self.syntax = Syntax::new(self.language, &self.rope);
        self.lsp_changes.mark_full();
        self.folds.clear();
        self.snippet = None;
        self.version += 1;
        self.saved_version = self.version;
        self.undo_stack.clear();
        self.redo_stack.clear();
        true
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| std::io::Error::other("document has no path"))?;
        if let Some(source) = self.image.clone() {
            // Never write text over an image; Save As copies the file.
            if source != path {
                std::fs::copy(&source, &path)?;
                self.image = Some(path.clone());
            }
            self.disk_mtime = mtime(&path);
            return Ok(());
        }
        let mut text = self.rope.to_string();
        if self.uses_crlf {
            text = text.replace('\n', "\r\n");
        }
        let bytes = encoding::encode(&text, self.encoding).map_err(std::io::Error::other)?;
        std::fs::write(&path, bytes)?;
        self.disk_mtime = mtime(&path);
        self.saved_version = self.version;
        Ok(())
    }

    /// Whether the file was modified or removed by another program since it
    /// was last read or written. Auto-save skips such files so it never
    /// overwrites someone else's changes or recreates a deleted file.
    pub fn changed_on_disk(&self) -> bool {
        let Some(path) = &self.path else { return false };
        self.disk_mtime.is_some() && mtime(path) != self.disk_mtime
    }

    // ---- Coordinate helpers -------------------------------------------------

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn line_start(&self, row: usize) -> usize {
        self.rope.line_to_byte(row.min(self.rope.len_lines()))
    }

    /// End of the row's content, excluding the newline.
    pub fn line_end(&self, row: usize) -> usize {
        if row + 1 >= self.rope.len_lines() {
            self.rope.len_bytes()
        } else {
            self.rope.line_to_byte(row + 1) - 1
        }
    }

    pub fn row_of(&self, offset: usize) -> usize {
        self.rope.byte_to_line(offset.min(self.rope.len_bytes()))
    }

    /// Text of a row without the trailing newline.
    pub fn line_text(&self, row: usize) -> String {
        let start = self.line_start(row);
        let end = self.line_end(row);
        self.slice(start..end)
    }

    pub fn slice(&self, range: Range<usize>) -> String {
        let s = self.rope.byte_to_char(range.start.min(self.len()));
        let e = self.rope.byte_to_char(range.end.min(self.len()));
        self.rope.slice(s..e).to_string()
    }

    pub fn char_at(&self, offset: usize) -> Option<char> {
        if offset >= self.len() {
            return None;
        }
        Some(self.rope.char(self.rope.byte_to_char(offset)))
    }

    pub fn char_before(&self, offset: usize) -> Option<char> {
        if offset == 0 {
            return None;
        }
        let ci = self.rope.byte_to_char(offset.min(self.len()));
        (ci > 0).then(|| self.rope.char(ci - 1))
    }

    pub fn next_char_boundary(&self, offset: usize) -> usize {
        match self.char_at(offset) {
            Some(c) => offset + c.len_utf8(),
            None => self.len(),
        }
    }

    pub fn prev_char_boundary(&self, offset: usize) -> usize {
        match self.char_before(offset) {
            Some(c) => offset - c.len_utf8(),
            None => 0,
        }
    }

    /// Snaps an arbitrary byte offset to a char boundary at or before it.
    pub fn clip(&self, offset: usize) -> usize {
        let offset = offset.min(self.len());
        self.rope.char_to_byte(self.rope.byte_to_char(offset))
    }

    // ---- Editing --------------------------------------------------------------

    /// Applies non-overlapping edits given in current-buffer coordinates.
    /// Returns, for each edit (in input order), the range the new text occupies
    /// afterwards.
    pub fn edit(
        &mut self,
        mut edits: Vec<(Range<usize>, String)>,
        selections_before: &[Selection],
        kind: EditKind,
    ) -> Vec<Range<usize>> {
        // Images have no text to edit (Replace in Files, paste…).
        if self.image.is_some() {
            return Vec::new();
        }
        let mut order: Vec<usize> = (0..edits.len()).collect();
        order.sort_by_key(|&i| edits[i].0.start);

        // Apply from last to first so earlier ranges remain valid.
        let mut records = Vec::with_capacity(edits.len());
        for &i in order.iter().rev() {
            let (range, text) = &mut edits[i];
            let range = self.clip(range.start)..self.clip(range.end.max(range.start));
            let old = self.slice(range.clone());
            if old.is_empty() && text.is_empty() {
                continue;
            }
            self.raw_replace(range.clone(), text);
            records.push(EditRecord {
                start: range.start,
                old,
                new: std::mem::take(text),
            });
        }

        // Compute post-edit ranges by accumulating deltas front to back.
        let mut result = vec![0..0; edits.len()];
        let mut delta: isize = 0;
        let rec_by_start = records.iter().rev(); // ascending order of start
        for (&i, rec) in order.iter().zip(rec_by_start) {
            let start = (rec.start as isize + delta) as usize;
            result[i] = start..start + rec.new.len();
            delta += rec.new.len() as isize - rec.old.len() as isize;
        }

        if !records.is_empty() {
            self.syntax.reparse(&self.rope);
            self.version += 1;
            self.redo_stack.clear();
            let now = Instant::now();
            let merge = self.undo_stack.last().is_some_and(|t| {
                kind != EditKind::Other
                    && t.kind == kind
                    && now.duration_since(t.at) < Duration::from_millis(800)
            });
            if merge {
                let t = self.undo_stack.last_mut().unwrap();
                t.edits.extend(records);
                t.at = now;
            } else {
                self.undo_stack.push(Transaction {
                    edits: records,
                    before: selections_before.to_vec(),
                    after: Vec::new(),
                    at: now,
                    kind,
                });
            }
        }
        result
    }

    /// Records the selections after the most recent transaction, for redo.
    pub fn set_selections_after(&mut self, selections: &[Selection]) {
        if let Some(t) = self.undo_stack.last_mut() {
            t.after = selections.to_vec();
        }
    }

    /// Breaks typing coalescing so the next edit starts a new undo step.
    pub fn finalize_transaction(&mut self) {
        if let Some(t) = self.undo_stack.last_mut() {
            t.kind = EditKind::Other;
        }
    }

    pub fn undo(&mut self) -> Option<Vec<Selection>> {
        let t = self.undo_stack.pop()?;
        for rec in t.edits.iter().rev() {
            self.raw_replace(rec.start..rec.start + rec.new.len(), &rec.old);
        }
        self.syntax.reparse(&self.rope);
        self.version += 1;
        let selections = t.before.clone();
        self.redo_stack.push(t);
        Some(selections)
    }

    pub fn redo(&mut self) -> Option<Vec<Selection>> {
        let t = self.redo_stack.pop()?;
        for rec in &t.edits {
            self.raw_replace(rec.start..rec.start + rec.old.len(), &rec.new);
        }
        self.syntax.reparse(&self.rope);
        self.version += 1;
        let selections = t.after.clone();
        self.undo_stack.push(t);
        Some(selections)
    }

    fn raw_replace(&mut self, range: Range<usize>, text: &str) {
        self.lsp_changes.record(&self.rope, range.clone(), text);
        let edit = input_edit(&self.rope, range.clone(), text);
        let s = self.rope.byte_to_char(range.start);
        let e = self.rope.byte_to_char(range.end);
        self.rope.remove(s..e);
        self.rope.insert(s, text);
        self.syntax.edit(&edit);
        self.shift_folds(&range, text.len());
        self.shift_breakpoints(&range, text.len());
        if let Some(snippet) = &mut self.snippet
            && !snippet.shift(&range, text.len())
        {
            self.snippet = None;
        }
        self.shift_diagnostics(range, text.len());
    }

    pub fn set_builtin_diagnostics(&mut self, diagnostics: Vec<Diagnostic>) {
        self.builtin_diagnostics = diagnostics;
        self.merge_diagnostics();
    }

    pub fn set_external_diagnostics(&mut self, diagnostics: Vec<Diagnostic>) {
        self.external_diagnostics = diagnostics;
        self.merge_diagnostics();
    }

    pub fn set_lsp_diagnostics(&mut self, diagnostics: Vec<Diagnostic>) {
        self.lsp_diagnostics = diagnostics;
        self.merge_diagnostics();
    }

    pub fn folds(&self) -> &[Range<usize>] {
        &self.folds
    }

    /// Hides rows `start..=end` (byte offsets of whole lines).
    pub fn fold_rows(&mut self, start: usize, end: usize) {
        if start == 0 || start >= self.line_count() {
            return;
        }
        let range = self.line_start(start)..self.line_end(end.min(self.line_count() - 1));
        if !range.is_empty() && !self.folds.contains(&range) {
            self.folds.push(range);
            self.folds.sort_by_key(|f| f.start);
        }
    }

    /// Opens the folds `unfold` matches; returns whether any opened.
    pub fn unfold_where(&mut self, mut unfold: impl FnMut(&Range<usize>) -> bool) -> bool {
        let before = self.folds.len();
        self.folds.retain(|f| !unfold(f));
        self.folds.len() != before
    }

    /// The breakpoints, by row.
    pub fn breakpoints(&self) -> Vec<Breakpoint> {
        self.breakpoints.iter().map(|(_, b)| b.clone()).collect()
    }

    pub fn breakpoint(&self, row: usize) -> Option<&Breakpoint> {
        self.breakpoints.iter().map(|(_, b)| b).find(|b| b.row == row)
    }

    pub fn set_breakpoints(&mut self, breakpoints: &[Breakpoint]) {
        let lines = self.line_count();
        let mut placed: Vec<(usize, Breakpoint)> =
            breakpoints.iter().filter(|b| b.row < lines).map(|b| (self.line_start(b.row), b.clone())).collect();
        placed.sort_by_key(|(start, _)| *start);
        placed.dedup_by_key(|(start, _)| *start);
        self.breakpoints = placed;
    }

    /// Puts `breakpoint` on its row, replacing the one there.
    pub fn set_breakpoint(&mut self, breakpoint: Breakpoint) {
        if breakpoint.row >= self.line_count() {
            return;
        }
        let start = self.line_start(breakpoint.row);
        match self.breakpoints.binary_search_by_key(&start, |(s, _)| *s) {
            Ok(ix) => self.breakpoints[ix].1 = breakpoint,
            Err(ix) => self.breakpoints.insert(ix, (start, breakpoint)),
        }
    }

    /// Adds or removes the breakpoint on `row`; true when one is there now.
    pub fn toggle_breakpoint(&mut self, row: usize) -> bool {
        let start = self.line_start(row);
        match self.breakpoints.binary_search_by_key(&start, |(s, _)| *s) {
            Ok(ix) => {
                self.breakpoints.remove(ix);
                false
            }
            Err(ix) => {
                self.breakpoints.insert(ix, (start, Breakpoint::at(row)));
                true
            }
        }
    }

    /// Breakpoints follow their line's text: an edit before a line moves
    /// it, one that removes the line's start removes it. (Runs after the
    /// rope changed, so lines can be looked up again.)
    fn shift_breakpoints(&mut self, range: &Range<usize>, new_len: usize) {
        if self.breakpoints.is_empty() {
            return;
        }
        let delta = new_len as isize - range.len() as isize;
        let mut moved = Vec::with_capacity(self.breakpoints.len());
        for (offset, breakpoint) in std::mem::take(&mut self.breakpoints) {
            if range.start < offset && offset <= range.end {
                continue;
            }
            let offset = if offset >= range.end { (offset as isize + delta) as usize } else { offset };
            // Typing at the start of a line leaves it where it is; a new
            // line inserted there pushes it down.
            let row = self.row_of(offset.min(self.rope.len_bytes()));
            moved.push((self.line_start(row), Breakpoint { row, ..breakpoint }));
        }
        moved.dedup_by_key(|(start, _)| *start);
        self.breakpoints = moved;
    }

    /// Folds move with edits before them; an edit touching one opens it.
    fn shift_folds(&mut self, range: &Range<usize>, new_len: usize) {
        let delta = new_len as isize - range.len() as isize;
        self.folds.retain_mut(|f| {
            if range.end < f.start {
                f.start = (f.start as isize + delta) as usize;
                f.end = (f.end as isize + delta) as usize;
                true
            } else {
                range.start > f.end
            }
        });
    }

    /// Edits not yet sent to the language server, oldest first; each is
    /// relative to the text after the previous one, as LSP expects.
    pub fn take_lsp_changes(&mut self) -> LspChanges {
        std::mem::take(&mut self.lsp_changes)
    }

    fn merge_diagnostics(&mut self) {
        self.diagnostics = self
            .builtin_diagnostics
            .iter()
            .chain(&self.external_diagnostics)
            .chain(&self.lsp_diagnostics)
            .cloned()
            .collect();
        self.diagnostics
            .sort_by_key(|d| (d.range.start, d.severity));
    }

    /// Keeps stale diagnostics roughly in place until the next lint finishes.
    fn shift_diagnostics(&mut self, range: Range<usize>, new_len: usize) {
        for list in [&mut self.builtin_diagnostics, &mut self.external_diagnostics, &mut self.lsp_diagnostics] {
            shift(list, &range, new_len);
        }
        self.merge_diagnostics();
    }
}

/// One replacement in LSP terms: where it was (as line plus UTF-8 and
/// UTF-16 columns, since servers differ) and the new text.
#[derive(Clone, Debug, PartialEq)]
pub struct LspChange {
    pub start: (u32, u32, u32),
    pub end: (u32, u32, u32),
    pub text: String,
}

#[derive(Default, Debug)]
pub struct LspChanges {
    pub changes: Vec<LspChange>,
    /// Too many changes, or the whole text was replaced: send it all.
    pub full: bool,
}

impl LspChanges {
    /// Past this, sending the whole text is cheaper than the edit list.
    const MAX: usize = 1000;

    fn record(&mut self, rope: &Rope, range: Range<usize>, text: &str) {
        if self.full {
            return;
        }
        if self.changes.len() >= Self::MAX {
            self.mark_full();
            return;
        }
        let point = |offset: usize| {
            let line = rope.byte_to_line(offset);
            let line_start = rope.line_to_byte(line);
            let (c0, c1) = (rope.byte_to_char(line_start), rope.byte_to_char(offset));
            let utf16 = rope.char_to_utf16_cu(c1) - rope.char_to_utf16_cu(c0);
            (line as u32, (offset - line_start) as u32, utf16 as u32)
        };
        self.changes.push(LspChange { start: point(range.start), end: point(range.end), text: text.to_string() });
    }

    fn mark_full(&mut self) {
        self.changes.clear();
        self.full = true;
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && !self.full
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn shift(diagnostics: &mut Vec<Diagnostic>, range: &Range<usize>, new_len: usize) {
    let delta = new_len as isize - range.len() as isize;
    diagnostics.retain_mut(|d| {
        if d.range.end <= range.start {
            true
        } else if d.range.start >= range.end {
            d.range.start = (d.range.start as isize + delta) as usize;
            d.range.end = (d.range.end as isize + delta) as usize;
            true
        } else {
            false
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn image_documents_are_never_edited_or_overwritten() {
        let dir = std::env::temp_dir().join(format!("kin-image-doc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pixel.png");
        let bytes = [0x89u8, b'P', b'N', b'G', 0, 1, 2, 3];
        std::fs::write(&path, bytes).unwrap();
        let mut doc = Document::open(&path).unwrap();
        assert!(doc.image.is_some() && doc.len() == 0);
        assert!(doc.edit(vec![(0..0, "text".into())], &[], EditKind::Other).is_empty());
        assert!(!doc.is_dirty());
        doc.save().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        // Save As copies the picture.
        let copy = dir.join("copy.png");
        doc.set_path(copy.clone());
        doc.save().unwrap();
        assert_eq!(std::fs::read(&copy).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    #[test]
    fn breakpoints_follow_their_lines() {
        let mut doc = Document::from_text("a\nb\nc\nd\n".into(), None);
        let rows = |doc: &Document| doc.breakpoints().iter().map(|b| b.row).collect::<Vec<_>>();
        assert!(doc.toggle_breakpoint(2));
        assert_eq!(rows(&doc), [2]);
        // A line added above moves it down (with its condition); typing at its start doesn't.
        doc.set_breakpoint(Breakpoint { condition: Some("x > 1".into()), ..Breakpoint::at(2) });
        doc.edit(vec![(0..0, "new\n".into())], &[], EditKind::Other);
        assert_eq!(doc.breakpoint(3).and_then(|b| b.condition.as_deref()), Some("x > 1"));
        let c = doc.line_start(3);
        doc.edit(vec![(c..c, "x".into())], &[], EditKind::Other);
        assert_eq!(rows(&doc), [3]);
        // Joining it with the line above removes it.
        doc.edit(vec![(c - 1..c, String::new())], &[], EditKind::Other);
        assert!(doc.breakpoints().is_empty());
        doc.set_breakpoints(&[Breakpoint::at(0), Breakpoint::at(9)]);
        assert_eq!(rows(&doc), [0]);
        assert!(!doc.toggle_breakpoint(0));
    }

    #[test]
    fn folds_follow_edits() {
        let mut doc = Document::from_text("a {\n  b\n  c\n}\nz\n".into(), None);
        doc.fold_rows(1, 3);
        assert_eq!(doc.folds(), [4..13]);
        // An edit above moves the fold.
        doc.edit(vec![(0..0, "x".into())], &[], EditKind::Other);
        assert_eq!(doc.folds(), [5..14]);
        // An edit after it leaves it alone; one inside opens it.
        doc.edit(vec![(15..15, "y".into())], &[], EditKind::Other);
        assert_eq!(doc.folds(), [5..14]);
        doc.edit(vec![(8..8, "!".into())], &[], EditKind::Other);
        assert!(doc.folds().is_empty());
    }

    #[test]
    fn multi_edit_and_undo() {
        let mut doc = Document::from_text("abc\ndef".into(), None);
        let r = doc.edit(
            vec![(4..4, "X".into()), (0..1, "".into())],
            &[],
            EditKind::Other,
        );
        assert_eq!(doc.rope().to_string(), "bc\nXdef");
        assert_eq!(r, vec![3..4, 0..0]);
        doc.undo();
        assert_eq!(doc.rope().to_string(), "abc\ndef");
        doc.redo();
        assert_eq!(doc.rope().to_string(), "bc\nXdef");
    }

    #[test]
    fn notices_changes_made_by_other_programs() {
        let dir = std::env::temp_dir().join(format!("kin-doc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one").unwrap();
        let mut doc = Document::open(&path).unwrap();
        assert!(!doc.changed_on_disk());

        doc.edit(vec![(3..3, "!".into())], &[], EditKind::Other);
        doc.save().unwrap();
        assert!(!doc.changed_on_disk(), "our own save isn't an outside change");

        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
        assert!(doc.changed_on_disk());
        doc.reload_from_disk().unwrap();
        assert!(!doc.changed_on_disk());

        std::fs::remove_file(&path).unwrap();
        assert!(doc.changed_on_disk(), "a deleted file counts as changed");
        std::fs::remove_dir_all(&dir).ok();
    }
}
