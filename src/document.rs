//! Text storage, undo history and file I/O. Offsets are UTF-8 byte offsets
//! throughout so they line up with tree-sitter and oxc spans.

use std::{
    ops::Range,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use ropey::Rope;

use crate::{
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
    pub uses_crlf: bool,
    /// Modification time of the file when it was last read or written, to
    /// notice changes made by other programs.
    disk_mtime: Option<SystemTime>,
}

impl Document {
    pub fn empty() -> Self {
        Self::from_text(String::new(), None)
    }

    pub fn open(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let mut doc = Self::from_text(text, Some(path.to_path_buf()));
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
            uses_crlf,
            disk_mtime: None,
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
            self.language = language;
            self.syntax = Syntax::new(language, &self.rope);
            self.set_builtin_diagnostics(Vec::new());
            self.set_external_diagnostics(Vec::new());
        }
    }

    /// Replaces the contents with the file on disk. Returns whether anything
    /// changed. Undo history is cleared since it no longer applies.
    pub fn reload_from_disk(&mut self) -> std::io::Result<bool> {
        let Some(path) = self.path.clone() else { return Ok(false) };
        let bytes = std::fs::read(&path)?;
        let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
        self.disk_mtime = mtime(&path);
        if text == self.rope.to_string() {
            return Ok(false);
        }
        self.rope = Rope::from_str(&text);
        self.syntax = Syntax::new(self.language, &self.rope);
        self.version += 1;
        self.saved_version = self.version;
        self.undo_stack.clear();
        self.redo_stack.clear();
        Ok(true)
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| std::io::Error::other("document has no path"))?;
        let mut text = self.rope.to_string();
        if self.uses_crlf {
            text = text.replace('\n', "\r\n");
        }
        std::fs::write(&path, text)?;
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
        let edit = input_edit(&self.rope, range.clone(), text);
        let s = self.rope.byte_to_char(range.start);
        let e = self.rope.byte_to_char(range.end);
        self.rope.remove(s..e);
        self.rope.insert(s, text);
        self.syntax.edit(&edit);
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

    fn merge_diagnostics(&mut self) {
        self.diagnostics = self
            .builtin_diagnostics
            .iter()
            .chain(&self.external_diagnostics)
            .cloned()
            .collect();
        self.diagnostics
            .sort_by_key(|d| (d.range.start, d.severity));
    }

    /// Keeps stale diagnostics roughly in place until the next lint finishes.
    fn shift_diagnostics(&mut self, range: Range<usize>, new_len: usize) {
        for list in [&mut self.builtin_diagnostics, &mut self.external_diagnostics] {
            shift(list, &range, new_len);
        }
        self.merge_diagnostics();
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
    use super::*;

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
