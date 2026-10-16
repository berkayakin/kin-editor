//! The window's documents by path, with or without a tab, as Zed's
//! `BufferStore`. Tabs' documents are added as they open; workspace edits
//! and Replace in Files open the others here (one load per path, however
//! many ask) and edit them without a tab. A document edited that way is
//! kept alive until it's saved or a tab takes it (`keep`), and listed for
//! the user to save (`kept`). Each document's Git diff lives here too, so
//! split views share it.

use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures::{FutureExt, future::Shared};
use gpui::{App, AppContext, Context, Entity, EntityId, EventEmitter, Subscription, Task, WeakEntity};

use crate::{
    document::{Document, EditKind, Selection},
    document_diff::DocumentDiff,
};

/// A load's result; the error is cloned to everyone waiting for it.
pub type Loaded = Result<Entity<Document>, Arc<str>>;

pub enum DocumentStoreEvent {
    /// The documents edited without a tab and not saved yet changed.
    KeptChanged,
}

impl EventEmitter<DocumentStoreEvent> for DocumentStore {}

#[derive(Default)]
pub struct DocumentStore {
    /// By canonical path. Weak: a document goes with its last tab, unless kept.
    documents: HashMap<PathBuf, WeakEntity<Document>>,
    /// Loads under way, by canonical path, so a second request waits for the first.
    loading: HashMap<PathBuf, Shared<Task<Loaded>>>,
    /// Edited without a tab and not saved yet, oldest first; held until saved
    /// (the observer lets go) or a tab shows it (`release`).
    kept: Vec<(Entity<Document>, Subscription)>,
    /// Each document's Git diff, by the document's entity; it goes with the document.
    diffs: HashMap<EntityId, (Entity<DocumentDiff>, Subscription)>,
}

/// The key a path is stored under: its real path (servers may report
/// `/tmp/…` for `/private/tmp/…`), or the path itself for one that doesn't exist.
pub fn key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

impl DocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The live document for `path`, if it's still there (a document that
    /// moved since it was added is found under its new path once re-added).
    pub fn get(&self, path: &Path, cx: &App) -> Option<Entity<Document>> {
        let key = key(path);
        let doc = self.documents.get(&key)?.upgrade()?;
        doc.read(cx).path.as_deref().is_some_and(|p| p == key || self::key(p) == key).then_some(doc)
    }

    /// Tracks a document under its path (again, after it moved or got one).
    /// Documents without a path aren't tracked.
    pub fn add(&mut self, doc: &Entity<Document>, cx: &App) {
        let Some(path) = doc.read(cx).path.as_deref() else { return };
        let key = key(path);
        // A key it was known by before, and documents that are gone.
        self.documents.retain(|k, d| *k != key && d.upgrade().is_some_and(|d| d != *doc));
        self.documents.insert(key, doc.downgrade());
    }

    /// The document for `path`: the open one, else read in the background.
    /// Asked again while it's being read, the same read answers.
    pub fn open(&mut self, path: &Path, cx: &mut Context<Self>) -> Task<Loaded> {
        if let Some(doc) = self.get(path, cx) {
            return Task::ready(Ok(doc));
        }
        let key = key(path);
        let load = self
            .loading
            .entry(key.clone())
            .or_insert_with(|| {
                let path = key.clone();
                cx.spawn(async move |this, cx| {
                    let read = cx.background_executor().spawn({
                        let path = path.clone();
                        async move { Document::open(&path) }
                    });
                    let read = read.await;
                    this.update(cx, |store, cx| {
                        store.loading.remove(&path);
                        // A tab opened it meanwhile: that one.
                        if let Some(doc) = store.get(&path, cx) {
                            return Ok(doc);
                        }
                        let doc = read.map_err(|err| Arc::<str>::from(err.to_string()))?;
                        let doc = cx.new(|_| doc);
                        store.add(&doc, cx);
                        Ok(doc)
                    })
                    .map_err(|_| Arc::<str>::from("the window closed"))?
                })
                .shared()
            })
            .clone();
        cx.spawn(async move |_, _| load.await)
    }

    /// Edits a document that has no tab, as one undo step (undone in a tab
    /// later, the caret goes to the first edit), and keeps it until it's
    /// saved. Returns whether the text changed.
    pub fn edit(&mut self, doc: &Entity<Document>, edits: Vec<(Range<usize>, String)>, cx: &mut Context<Self>) -> bool {
        let Some(first) = (0..edits.len()).min_by_key(|&i| edits[i].0.start) else { return false };
        let at = edits[first].0.start;
        let changed = doc.update(cx, |d, cx| {
            let version = d.version();
            let ranges = d.edit(edits, &[Selection::cursor(at)], EditKind::Other);
            if d.version() == version {
                return false;
            }
            d.set_selections_after(&[Selection::cursor(ranges[first].end)]);
            d.finalize_transaction();
            cx.notify();
            true
        });
        if changed {
            self.keep(doc, cx);
        }
        changed
    }

    /// Keeps a document edited without a tab alive until it's saved (or
    /// edited back to its saved text), or a tab shows it (`release`).
    pub fn keep(&mut self, doc: &Entity<Document>, cx: &mut Context<Self>) {
        if self.kept.iter().any(|(d, _)| d == doc) || !doc.read(cx).is_dirty() {
            return;
        }
        let observer = cx.observe(doc, |store, doc, cx| {
            if !doc.read(cx).is_dirty() {
                store.release(&doc, cx);
            }
        });
        self.kept.push((doc.clone(), observer));
        cx.emit(DocumentStoreEvent::KeptChanged);
    }

    /// Stops keeping a document alive: a tab holds it now, or its edits were dropped.
    pub fn release(&mut self, doc: &Entity<Document>, cx: &mut Context<Self>) {
        let before = self.kept.len();
        self.kept.retain(|(d, _)| d != doc);
        if self.kept.len() != before {
            cx.emit(DocumentStoreEvent::KeptChanged);
        }
    }

    /// The documents edited without a tab and not saved yet, oldest first
    /// (one saved just now is let go once its notify is handled).
    pub fn kept(&self, cx: &App) -> Vec<Entity<Document>> {
        self.kept.iter().map(|(d, _)| d.clone()).filter(|d| d.read(cx).is_dirty()).collect()
    }

    /// The document's Git diff, made the first time it's asked for.
    pub fn diff_for(&mut self, doc: &Entity<Document>, cx: &mut Context<Self>) -> Entity<DocumentDiff> {
        if let Some((diff, _)) = self.diffs.get(&doc.entity_id()) {
            return diff.clone();
        }
        let diff = cx.new(|cx| DocumentDiff::new(doc, cx));
        let id = doc.entity_id();
        let released = cx.observe_release(doc, move |store, _, _| {
            store.diffs.remove(&id);
        });
        self.diffs.insert(id, (diff.clone(), released));
        diff
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::TestAppContext;

    use super::*;
    use crate::editor::Editor;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kuana-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A load's result, once the app has nothing left to do.
    fn done(task: Task<Loaded>, cx: &mut TestAppContext) -> Loaded {
        cx.run_until_parked();
        task.now_or_never().expect("loaded")
    }

    #[gpui::test]
    fn one_load_per_path_and_tabs_share_it(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = temp_dir("dedup");
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let store = cx.new(|_| DocumentStore::new());
        // Two requests while it's being read: one read, one document.
        let (first, second) = store.update(cx, |s, cx| (s.open(&path, cx), s.open(&path, cx)));
        let (first, second) = (done(first, cx).unwrap(), done(second, cx).unwrap());
        assert_eq!(first, second);
        // A path spelled another way (through a symlink, `..`) is the same file.
        let other = dir.join("sub/../a.txt");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert_eq!(store.read_with(cx, |s, cx| s.get(&other, cx)), Some(first.clone()));
        // Asked again once it's open: that one, without reading.
        let again = store.update(cx, |s, cx| s.open(&path, cx));
        assert_eq!(done(again, cx).unwrap(), first);
        // A file that can't be read: everyone waiting hears why.
        let missing = store.update(cx, |s, cx| s.open(&dir.join("missing.txt"), cx));
        assert!(done(missing, cx).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn documents_go_with_their_last_holder_unless_kept_unsaved(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = temp_dir("keep");
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let store = cx.new(|_| DocumentStore::new());
        let open = store.update(cx, |s, cx| s.open(&path, cx));
        let doc = done(open, cx).unwrap();
        // Unedited and not shown: it goes.
        let weak = doc.downgrade();
        drop(doc);
        cx.run_until_parked();
        assert!(weak.upgrade().is_none());
        assert!(store.read_with(cx, |s, cx| s.get(&path, cx)).is_none());

        // Edited without a tab: kept, and listed.
        let open = store.update(cx, |s, cx| s.open(&path, cx));
        let doc = done(open, cx).unwrap();
        assert!(store.update(cx, |s, cx| s.edit(&doc, vec![(0..3, "two".into())], cx)));
        let weak = doc.downgrade();
        drop(doc);
        cx.run_until_parked();
        let kept = store.read_with(cx, |s, cx| s.kept(cx));
        assert_eq!(kept.len(), 1);
        assert!(weak.upgrade().is_some());
        // A tab opened for it gets this document, edits and all; undo there
        // takes them back in one step, the caret at the change.
        let found = store.read_with(cx, |s, cx| s.get(&path, cx)).unwrap();
        assert_eq!(found, kept[0]);
        let editor = cx.new(|cx| Editor::with_document(found.clone(), cx));
        assert_eq!(editor.read_with(cx, |e, cx| e.text(cx)), "two\n");
        editor.update(cx, |e, cx| e.undo_step(cx));
        assert_eq!(editor.read_with(cx, |e, cx| (e.text(cx), e.primary_selection().head)), ("one\n".into(), 0));
        editor.update(cx, |e, cx| e.redo_step(cx));
        drop(editor);
        // Saved: no longer held.
        found.update(cx, |d, cx| {
            d.save().unwrap();
            cx.notify();
        });
        cx.run_until_parked();
        assert!(store.read_with(cx, |s, cx| s.kept(cx)).is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two\n");
        drop((found, kept));
        cx.run_until_parked();
        assert!(weak.upgrade().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn split_views_share_one_diff(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let store = cx.new(|_| DocumentStore::new());
        let editor = cx.new(|cx| Editor::new(Document::from_text("a\nb\nc\n".into(), Some(PathBuf::from("/x/a.txt"))), cx));
        let split = cx.new(|cx| Editor::view_of(&editor, cx));
        let doc = editor.read_with(cx, |e, _| e.doc.clone());
        let diff = store.update(cx, |s, cx| s.diff_for(&doc, cx));
        assert_eq!(store.update(cx, |s, cx| s.diff_for(&doc, cx)), diff);
        for e in [&editor, &split] {
            e.update(cx, |e, cx| e.set_git_diff(diff.clone(), cx));
        }
        diff.update(cx, |d, cx| d.set_base(None, Some("a\nB\nc\n".into()), None, cx));
        cx.run_until_parked();
        assert_eq!(diff.read_with(cx, |d, _| d.runs), 1);
        for e in [&editor, &split] {
            assert_eq!(e.read_with(cx, |e, _| e.hunk_rows()), [1..2]);
        }
        // An edit in one view: diffed once, for both.
        editor.update(cx, |e, cx| e.replace_ranges(vec![(0..1, "x\ny".into())], cx));
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        assert_eq!(diff.read_with(cx, |d, _| d.runs), 2);
        for e in [&editor, &split] {
            assert_eq!(e.read_with(cx, |e, _| e.hunk_rows()), [0..3]);
        }
        // Notifies that change no text (diagnostics) don't diff again.
        doc.update(cx, |d, cx| {
            d.set_lsp_diagnostics(Vec::new());
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(diff.read_with(cx, |d, _| d.runs), 2);
    }

    #[gpui::test]
    fn a_diff_goes_with_its_document(cx: &mut TestAppContext) {
        let store = cx.new(|_| DocumentStore::new());
        let doc = cx.new(|_| Document::from_text("a\n".into(), Some(PathBuf::from("/x/a.txt"))));
        let diff = store.update(cx, |s, cx| s.diff_for(&doc, cx));
        drop(doc);
        // Released entities are let go when effects flush.
        cx.update(|_| {});
        assert!(store.read_with(cx, |s, _| s.diffs.is_empty()));
        drop(diff);
    }
}
