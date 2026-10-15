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
    /// Where the line a debug session is paused on starts.
    debug_line: Option<usize>,
    /// The tab stops of a snippet being filled in; they follow edits.
    pub snippet: Option<crate::snippets::SnippetStops>,
    /// The language server's inlay hints (sorted); they follow edits until
    /// the next answer replaces them.
    inlay_hints: Vec<InlayHint>,
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
        // A large file is shown as plain text: no parsing, linting or language
        // server. Decided before building the syntax tree, which would parse it.
        let mut doc = Self::with_text(text, Some(path.to_path_buf()), size > LARGE_FILE_SIZE);
        doc.encoding = encoding;
        doc.disk_mtime = mtime(path);
        Ok(doc)
    }

    fn from_text(text: String, path: Option<PathBuf>) -> Self {
        Self::with_text(text, path, false)
    }

    fn with_text(text: String, path: Option<PathBuf>, large: bool) -> Self {
        // Normalize to LF internally; remember the style for saving.
        let uses_crlf = text.contains("\r\n");
        let text = if uses_crlf {
            text.replace("\r\n", "\n")
        } else {
            text
        };
        let rope = Rope::from_str(&text);
        let language = match path.as_deref() {
            Some(path) if !large => Language::from_path(path),
            _ => Language::PlainText,
        };
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
            large,
            lsp_changes: LspChanges::default(),
            folds: Vec::new(),
            breakpoints: Vec::new(),
            debug_line: None,
            snippet: None,
            inlay_hints: Vec::new(),
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
    /// Large files stay plain text.
    pub fn set_language(&mut self, language: Language) {
        if self.large {
            return;
        }
        self.language = language;
        self.syntax = Syntax::new(language, &self.rope);
        self.set_builtin_diagnostics(Vec::new());
        self.set_external_diagnostics(Vec::new());
    }

    /// Replaces the contents with the file on disk. Returns whether anything
    /// changed. Undo history is cleared since it no longer applies. A file
    /// whose modification time is unchanged isn't read (window focus and Git
    /// operations ask every open tab).
    pub fn reload_from_disk(&mut self) -> std::io::Result<bool> {
        let Some(path) = self.path.clone() else { return Ok(false) };
        if self.disk_mtime.is_some() && !self.changed_on_disk() {
            return Ok(false);
        }
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
        if self.rope == text.as_str() {
            return false;
        }
        // Breakpoints and the debug line keep their line numbers across a reload.
        let (breakpoints, debug_row) = (self.breakpoints(), self.debug_row());
        self.rope = Rope::from_str(&text);
        self.set_breakpoints(&breakpoints);
        self.set_debug_row(debug_row);
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

    /// Applies edits given in current-buffer coordinates. Overlapping edits
    /// are merged into one (their texts joined in order), as two carets'
    /// deletions can overlap (Backspace deleting a bracket pair next to
    /// another caret). Returns, for each edit (in input order), the range its
    /// new text occupies afterwards, including edits that changed nothing.
    pub fn edit(
        &mut self,
        edits: Vec<(Range<usize>, String)>,
        selections_before: &[Selection],
        kind: EditKind,
    ) -> Vec<Range<usize>> {
        // Images have no text to edit (Replace in Files, paste…).
        if self.image.is_some() {
            return Vec::new();
        }
        let count = edits.len();
        let mut edits: Vec<(usize, Range<usize>, String)> = edits
            .into_iter()
            .enumerate()
            .map(|(i, (range, text))| {
                let start = self.clip(range.start);
                (i, start..self.clip(range.end.max(start)), text)
            })
            .collect();
        edits.sort_by_key(|(_, range, _)| (range.start, range.end));

        // Group overlapping edits; each member remembers where its text starts
        // inside the group's text.
        struct Group {
            range: Range<usize>,
            text: String,
            members: Vec<(usize, Range<usize>)>,
        }
        let mut groups: Vec<Group> = Vec::with_capacity(count);
        for (i, range, text) in edits {
            match groups.last_mut() {
                Some(g) if range.start < g.range.end => {
                    g.range.end = g.range.end.max(range.end);
                    g.members.push((i, g.text.len()..g.text.len() + text.len()));
                    g.text.push_str(&text);
                }
                _ => groups.push(Group { members: vec![(i, 0..text.len())], range, text }),
            }
        }

        // Apply from last to first so earlier ranges remain valid.
        let mut records = Vec::with_capacity(groups.len());
        for g in groups.iter().rev() {
            if g.range.is_empty() && g.text.is_empty() {
                continue;
            }
            let old = self.slice(g.range.clone());
            self.raw_replace(g.range.clone(), &g.text);
            records.push(EditRecord { start: g.range.start, old, new: g.text.clone() });
        }
        // Tracked positions move once for all of them.
        let patch = Patch::new(
            groups.iter().filter(|g| !(g.range.is_empty() && g.text.is_empty())).map(|g| (g.range.clone(), g.text.len())).collect(),
        );
        self.shift_tracked(&patch);

        // Compute post-edit ranges by accumulating deltas front to back.
        let mut result = vec![0..0; count];
        let mut delta: isize = 0;
        for g in &groups {
            let start = (g.range.start as isize + delta) as usize;
            for (i, within) in &g.members {
                result[*i] = start + within.start..start + within.end;
            }
            delta += g.text.len() as isize - g.range.len() as isize;
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

    /// Where the undo history is now, for `merge_undo_since`.
    pub fn undo_mark(&self) -> usize {
        self.undo_stack.len()
    }

    /// Makes everything done since `mark` one undo step (a Vim change: the
    /// operator and the text typed after it).
    pub fn merge_undo_since(&mut self, mark: usize) {
        if self.undo_stack.len() <= mark + 1 {
            return;
        }
        let mut merged = self.undo_stack.split_off(mark);
        let mut first = merged.remove(0);
        for t in merged {
            first.edits.extend(t.edits);
            first.after = t.after;
            first.at = t.at;
        }
        first.kind = EditKind::Other;
        self.undo_stack.push(first);
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
            let range = rec.start..rec.start + rec.new.len();
            self.raw_replace(range.clone(), &rec.old);
            self.shift_tracked(&Patch::new(vec![(range, rec.old.len())]));
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
            let range = rec.start..rec.start + rec.old.len();
            self.raw_replace(range.clone(), &rec.new);
            self.shift_tracked(&Patch::new(vec![(range, rec.new.len())]));
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
    }

    /// Moves what follows the text (folds, breakpoints, the debug line, a
    /// snippet's stops, inlay hints, diagnostics) across a transaction's
    /// edits, once for all of them. Runs after the rope changed.
    fn shift_tracked(&mut self, patch: &Patch) {
        if patch.edits.is_empty() {
            return;
        }
        self.shift_folds(patch);
        self.shift_breakpoints(patch);
        self.debug_line = self.debug_line.and_then(|offset| self.shift_line(offset, patch));
        if let Some(snippet) = &mut self.snippet
            && !snippet.shift(patch)
        {
            self.snippet = None;
        }
        self.shift_inlay_hints(patch);
        // The merged list too: shifting keeps its order, so it needn't be
        // built and sorted again.
        for list in [&mut self.builtin_diagnostics, &mut self.external_diagnostics, &mut self.lsp_diagnostics, &mut self.diagnostics] {
            shift_diagnostics(list, patch);
        }
    }

    pub fn inlay_hints(&self) -> &[InlayHint] {
        &self.inlay_hints
    }

    pub fn set_inlay_hints(&mut self, mut hints: Vec<InlayHint>) {
        hints.sort_by_key(|h| h.offset);
        self.inlay_hints = hints;
    }

    /// Hints after an edit move with the text; ones inside replaced text go.
    /// A hint where text is inserted stays after it (a type after a name
    /// being typed).
    fn shift_inlay_hints(&mut self, patch: &Patch) {
        self.inlay_hints.retain_mut(|h| {
            let (mut delta, near) = patch.around(h.offset..h.offset);
            for (range, new_len) in near {
                if h.offset > range.start && h.offset < range.end {
                    return false;
                }
                if h.offset >= range.end {
                    delta += Patch::change(range, *new_len);
                }
            }
            h.offset = moved(h.offset, delta);
            true
        });
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
    fn shift_breakpoints(&mut self, patch: &Patch) {
        if self.breakpoints.is_empty() {
            return;
        }
        let mut moved = Vec::with_capacity(self.breakpoints.len());
        for (offset, breakpoint) in std::mem::take(&mut self.breakpoints) {
            if let Some(start) = self.shift_line(offset, patch) {
                moved.push((start, Breakpoint { row: self.row_of(start), ..breakpoint }));
            }
        }
        moved.dedup_by_key(|(start, _)| *start);
        self.breakpoints = moved;
    }

    /// Where the line starting at `offset` starts after `patch`; None when
    /// the edits removed its start (joined it with the line above).
    fn shift_line(&self, offset: usize, patch: &Patch) -> Option<usize> {
        let (mut delta, near) = patch.around(offset..offset);
        for (range, new_len) in near {
            if range.start < offset && offset <= range.end {
                return None;
            }
            if offset >= range.end {
                delta += Patch::change(range, *new_len);
            }
        }
        // Typing at the start of a line leaves it where it is; a new line
        // inserted there pushes it down.
        let row = self.row_of(moved(offset, delta).min(self.rope.len_bytes()));
        Some(self.line_start(row))
    }

    /// The row a debug session is paused on, following edits.
    pub fn debug_row(&self) -> Option<usize> {
        self.debug_line.map(|offset| self.row_of(offset))
    }

    /// Highlights `row` as where a debug session is paused (None: running).
    pub fn set_debug_row(&mut self, row: Option<usize>) {
        self.debug_line = row.filter(|&r| r < self.line_count()).map(|r| self.line_start(r));
    }

    /// Folds move with edits before them; an edit touching one opens it.
    fn shift_folds(&mut self, patch: &Patch) {
        self.folds.retain_mut(|f| {
            let (mut delta, near) = patch.around(f.clone());
            for (range, new_len) in near {
                if range.end < f.start {
                    delta += Patch::change(range, *new_len);
                } else if range.start <= f.end {
                    return false;
                }
            }
            *f = moved(f.start, delta)..moved(f.end, delta);
            true
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
}

/// A type or parameter name from the language server, shown in the text
/// before `offset`.
#[derive(Clone, Debug, PartialEq)]
pub struct InlayHint {
    pub offset: usize,
    pub label: String,
    pub pad_left: bool,
    pub pad_right: bool,
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

/// One transaction's edits, for moving tracked positions: the replaced
/// ranges (sorted, not overlapping, in the text before the edits) with the
/// length of their new text.
pub struct Patch {
    edits: Vec<(Range<usize>, usize)>,
    /// `before[i]`: the length change of `edits[..i]`.
    before: Vec<isize>,
}

impl Patch {
    pub fn new(edits: Vec<(Range<usize>, usize)>) -> Self {
        let mut before = Vec::with_capacity(edits.len() + 1);
        let mut total = 0;
        before.push(0);
        for (range, new_len) in &edits {
            total += Patch::change(range, *new_len);
            before.push(total);
        }
        Patch { edits, before }
    }

    pub fn change(range: &Range<usize>, new_len: usize) -> isize {
        new_len as isize - range.len() as isize
    }

    /// For something spanning `span`: the length change of the edits that
    /// end before it starts (they move it), and the edits touching or
    /// overlapping it, which each list judges by its own rule (a few at most).
    pub fn around(&self, span: Range<usize>) -> (isize, &[(Range<usize>, usize)]) {
        let first = self.edits.partition_point(|(range, _)| range.end < span.start);
        let last = first + self.edits[first..].partition_point(|(range, _)| range.start <= span.end);
        (self.before[first], &self.edits[first..last])
    }
}

fn moved(offset: usize, delta: isize) -> usize {
    (offset as isize + delta).max(0) as usize
}

/// Diagnostics keep roughly in place until the next lint: ones before or
/// after the edits move, ones they touch go.
fn shift_diagnostics(diagnostics: &mut Vec<Diagnostic>, patch: &Patch) {
    diagnostics.retain_mut(|d| {
        let (mut delta, near) = patch.around(d.range.clone());
        for (range, new_len) in near {
            if d.range.end <= range.start {
                // The edit is after it (an empty one stays put at an insertion).
            } else if d.range.start >= range.end {
                delta += Patch::change(range, *new_len);
            } else {
                return false;
            }
        }
        d.range = moved(d.range.start, delta)..moved(d.range.end, delta);
        true
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
    fn one_transaction_moves_things_like_its_edits_one_by_one() {
        let text = "fn a() {\n    one\n}\nfn b() {\n    two\n}\nlast line\n";
        let setup = || {
            let mut doc = Document::from_text(text.into(), None);
            let diagnostic = |range: Range<usize>| Diagnostic {
                range,
                severity: crate::lint::Severity::Warning,
                message: "m".into(),
                rule: "r".into(),
            };
            doc.set_builtin_diagnostics(vec![diagnostic(13..16), diagnostic(31..34), diagnostic(39..39)]);
            doc.set_lsp_diagnostics(vec![diagnostic(3..4), diagnostic(42..46)]);
            let hint = |offset| InlayHint { offset, label: ": T".into(), pad_left: false, pad_right: false };
            doc.set_inlay_hints(vec![hint(4), hint(16), hint(34)]);
            doc.set_breakpoints(&[Breakpoint::at(1), Breakpoint::at(4), Breakpoint::at(6)]);
            doc.set_debug_row(Some(4));
            doc.fold_rows(4, 5);
            doc
        };
        let snapshot = |doc: &Document| {
            (
                doc.rope().to_string(),
                doc.diagnostics.iter().map(|d| d.range.clone()).collect::<Vec<_>>(),
                doc.inlay_hints().iter().map(|h| h.offset).collect::<Vec<_>>(),
                doc.breakpoints().iter().map(|b| b.row).collect::<Vec<_>>(),
                doc.debug_row(),
                doc.folds().to_vec(),
            )
        };
        // Multi-caret edits: a new line at a line start, typing in a
        // diagnostic, deleting a hint's text, an insertion at the end.
        let edit_sets: [Vec<(Range<usize>, &str)>; 3] = [
            vec![(0..0, "// x\n"), (13..13, "z"), (27..27, "\n")],
            vec![(12..16, ""), (31..34, "three"), (46..46, "!")],
            vec![(9..9, "\n\n"), (22..23, ""), (39..39, "x")],
        ];
        for edits in edit_sets {
            let mut together = setup();
            together.edit(edits.iter().map(|(r, t)| (r.clone(), t.to_string())).collect(), &[], EditKind::Other);
            let mut one_by_one = setup();
            for (range, text) in edits.iter().rev() {
                one_by_one.edit(vec![(range.clone(), text.to_string())], &[], EditKind::Other);
            }
            assert_eq!(snapshot(&together), snapshot(&one_by_one), "{edits:?}");
            // The merged list was moved in place, and matches merging again.
            let merged = together.diagnostics.iter().map(|d| d.range.clone()).collect::<Vec<_>>();
            together.merge_diagnostics();
            assert_eq!(merged, together.diagnostics.iter().map(|d| d.range.clone()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn the_debug_line_follows_edits() {
        let mut doc = Document::from_text("a\nb\nc\n".into(), None);
        doc.set_debug_row(Some(1));
        doc.edit(vec![(0..0, "new\n".into())], &[], EditKind::Other);
        assert_eq!(doc.debug_row(), Some(2));
        doc.undo();
        assert_eq!(doc.debug_row(), Some(1));
    }

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
    fn large_files_are_never_parsed() {
        let text = "{\"a\": 1}\n".repeat(10);
        let doc = Document::with_text(text.clone(), Some("big.json".into()), true);
        assert!(doc.large && doc.language == Language::PlainText);
        assert!(doc.syntax().tree().is_none());
        let mut doc = Document::with_text(text, Some("big.json".into()), true);
        doc.set_path("renamed.rs".into());
        assert!(doc.language == Language::PlainText && doc.syntax().tree().is_none());
        // A small one is.
        let doc = Document::from_text("{}".into(), Some("small.json".into()));
        assert!(doc.syntax().tree().is_some());
    }

    #[test]
    fn edits_that_change_nothing_still_get_their_range() {
        // Delete with carets at 3 and at the end: the second deletes nothing.
        let mut doc = Document::from_text("abc\ndef".into(), None);
        let r = doc.edit(vec![(3..4, String::new()), (7..7, String::new())], &[], EditKind::Deleting);
        assert_eq!(doc.rope().to_string(), "abcdef");
        assert_eq!(r, vec![3..3, 6..6]);

        // Cut with an empty selection between two non-empty ones.
        let mut doc = Document::from_text("one two three".into(), None);
        let r = doc.edit(
            vec![(8..13, String::new()), (0..3, String::new()), (5..5, String::new())],
            &[],
            EditKind::Other,
        );
        assert_eq!(doc.rope().to_string(), " two ");
        assert_eq!(r, vec![5..5, 0..0, 2..2]);
    }

    #[test]
    fn overlapping_edits_are_merged() {
        // Backspace with carets at 1 and 2 in "()a": the first deletes the
        // pair 0..2, the second 1..2. The "a" must survive.
        let mut doc = Document::from_text("()a".into(), None);
        let r = doc.edit(vec![(0..2, String::new()), (1..2, String::new())], &[], EditKind::Deleting);
        assert_eq!(doc.rope().to_string(), "a");
        assert_eq!(r, vec![0..0, 0..0]);
        doc.undo();
        assert_eq!(doc.rope().to_string(), "()a");

        // Overlapping replacements keep both texts, in order.
        let mut doc = Document::from_text("abcdef".into(), None);
        let r = doc.edit(vec![(2..5, "Y".into()), (1..3, "X".into())], &[], EditKind::Other);
        assert_eq!(doc.rope().to_string(), "aXYf");
        assert_eq!(r, vec![2..3, 1..2]);
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

        // Without a newer modification time the file isn't read again.
        let same = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, "other").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(same).unwrap();
        assert!(!doc.reload_from_disk().unwrap());
        assert_eq!(doc.rope().to_string(), "one!");
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(later + Duration::from_secs(5)).unwrap();
        assert!(doc.reload_from_disk().unwrap());
        assert_eq!(doc.rope().to_string(), "other");

        std::fs::remove_file(&path).unwrap();
        assert!(doc.changed_on_disk(), "a deleted file counts as changed");
        std::fs::remove_dir_all(&dir).ok();
    }
}
