//! Restored tabs that read their file when first shown: a session with many
//! tabs opens without reading, parsing and linting every one of them.

use std::path::PathBuf;

use gpui::Context;
use ropey::Rope;

use super::Editor;
use crate::{
    document::{Document, Selection},
    encoding::TextEncoding,
};

/// What a restored tab brings back once its file is read: the session's
/// caret, folds, encoding and unsaved text, kept as saved until then.
pub struct Unloaded {
    /// The caret, as a byte offset.
    pub cursor: usize,
    /// The whole text, when the buffer had unsaved edits.
    pub unsaved: Option<Rope>,
    /// Folded regions as (header row, last hidden row).
    pub folds: Vec<[usize; 2]>,
    /// An encoding picked last time, rather than the guess.
    pub encoding: Option<TextEncoding>,
}

/// An unloaded editor read its file: the workspace starts what waited for
/// the text (language server, Git changes, breakpoints). `error` when the
/// file couldn't be read and there was no unsaved text to show instead.
pub struct EditorLoaded {
    pub error: Option<String>,
}

impl gpui::EventEmitter<EditorLoaded> for Editor {}

impl Editor {
    /// A tab for `path` (None: untitled) that reads nothing until `load`.
    pub fn new_unloaded(path: Option<PathBuf>, unloaded: Unloaded, cx: &mut Context<Self>) -> Self {
        // The path is set afterwards: settings for it would read `.editorconfig`.
        let mut this = Self::blank(Document::unloaded(None, unloaded.unsaved.is_some()), cx);
        this.doc.as_mut(cx).path = path;
        this.unloaded = Some(unloaded);
        this
    }

    /// The restored state of a tab not read yet.
    pub fn unloaded(&self) -> Option<&Unloaded> {
        self.unloaded.as_ref()
    }

    pub fn is_loaded(&self) -> bool {
        self.unloaded.is_none()
    }

    /// Reads an unloaded tab's file and brings back its unsaved text, folds
    /// and caret. Fails (keeping the empty stand-in) when the file can't be
    /// read and there's no unsaved text.
    pub fn load(&mut self, cx: &mut Context<Self>) -> std::io::Result<()> {
        let Some(unloaded) = self.unloaded.take() else { return Ok(()) };
        let doc = match self.doc.read(cx).path.as_deref().map(Document::open) {
            Some(Ok(mut doc)) => {
                if let Some(encoding) = unloaded.encoding
                    && encoding != doc.encoding
                {
                    let _ = doc.reload_with_encoding(encoding);
                }
                doc
            }
            Some(Err(err)) if unloaded.unsaved.is_none() => {
                self.unloaded = Some(unloaded);
                cx.emit(EditorLoaded { error: Some(err.to_string()) });
                return Err(err);
            }
            // Untitled, or a deleted file with edits: keep the text.
            _ => Document::empty(),
        };
        // A fresh editor for the text, in the same view (tabs and focus refer to it).
        let focus_handle = self.focus_handle.clone();
        *self = Self::new(doc, cx);
        self.focus_handle = focus_handle;
        if let Some(text) = unloaded.unsaved {
            // An edit on top of the file, so undo gets back to the saved text.
            let len = self.doc.read(cx).len();
            self.replace_ranges(vec![(0..len, text.to_string())], cx);
        }
        self.restore_folds(&unloaded.folds, cx);
        let cursor = self.doc.read(cx).clip(unloaded.cursor.min(self.doc.read(cx).len()));
        self.set_selections(vec![Selection::cursor(cursor)], 0, cx);
        self.center_on_next_frame = true;
        cx.emit(EditorLoaded { error: None });
        cx.notify();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext, TestAppContext};

    use super::*;

    fn temp_file(name: &str, text: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kin-unloaded-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn unloaded(cursor: usize, unsaved: Option<&str>) -> Unloaded {
        Unloaded { cursor, unsaved: unsaved.map(Rope::from_str), folds: Vec::new(), encoding: None }
    }

    #[gpui::test]
    fn reads_the_file_on_load(cx: &mut TestAppContext) {
        let path = temp_file("clean.txt", b"one\ntwo\nthree\n");
        let editor = cx.new(|cx| Editor::new_unloaded(Some(path.clone()), unloaded(5, None), cx));
        editor.read_with(cx, |e, cx| {
            assert!(!e.is_loaded());
            assert_eq!(e.text(cx), "");
            assert!(!e.doc.read(cx).is_dirty());
            assert_eq!(e.doc.read(cx).path.as_deref(), Some(path.as_path()));
        });
        editor.update(cx, |e, cx| e.load(cx)).unwrap();
        editor.read_with(cx, |e, cx| {
            assert!(e.is_loaded());
            assert_eq!(e.text(cx), "one\ntwo\nthree\n");
            assert!(!e.doc.read(cx).is_dirty());
            assert_eq!(e.primary_selection().head, 5);
        });
    }

    #[gpui::test]
    fn unsaved_text_is_an_edit_over_the_file(cx: &mut TestAppContext) {
        let path = temp_file("dirty.txt", b"saved\n");
        let editor = cx.new(|cx| Editor::new_unloaded(Some(path), unloaded(99, Some("edited text\n")), cx));
        // Dirty before it's read, so closing asks and the tab shows it.
        editor.read_with(cx, |e, cx| assert!(e.doc.read(cx).is_dirty()));
        editor.update(cx, |e, cx| e.load(cx)).unwrap();
        editor.update(cx, |e, cx| {
            assert_eq!(e.text(cx), "edited text\n");
            assert!(e.doc.read(cx).is_dirty());
            // The caret is kept inside the text.
            assert_eq!(e.primary_selection().head, e.doc.read(cx).len());
            e.doc.as_mut(cx).undo();
            assert_eq!(e.text(cx), "saved\n");
        });
    }

    #[gpui::test]
    fn keeps_a_chosen_encoding(cx: &mut TestAppContext) {
        // Valid UTF-8 would be guessed as UTF-8; the session's pick wins.
        let path = temp_file("cyrillic.txt", "мир\n".as_bytes());
        let encoding = TextEncoding::from_key("windows-1251");
        let mut state = unloaded(0, None);
        state.encoding = encoding;
        let editor = cx.new(|cx| Editor::new_unloaded(Some(path), state, cx));
        editor.update(cx, |e, cx| e.load(cx)).unwrap();
        editor.read_with(cx, |e, cx| {
            assert_eq!(Some(e.doc.read(cx).encoding), encoding);
            assert!(!e.doc.read(cx).is_dirty());
        });
    }

    #[gpui::test]
    fn a_missing_file_fails_unless_there_is_unsaved_text(cx: &mut TestAppContext) {
        let gone = std::env::temp_dir().join("kin-unloaded-missing/nothing.txt");
        let editor = cx.new(|cx| Editor::new_unloaded(Some(gone.clone()), unloaded(0, None), cx));
        assert!(editor.update(cx, |e, cx| e.load(cx)).is_err());
        editor.read_with(cx, |e, _| assert!(!e.is_loaded()));
        let editor = cx.new(|cx| Editor::new_unloaded(Some(gone), unloaded(0, Some("kept")), cx));
        editor.update(cx, |e, cx| e.load(cx)).unwrap();
        editor.read_with(cx, |e, cx| assert_eq!(e.text(cx), "kept"));
    }
}
