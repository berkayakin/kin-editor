//! A document's Git changes: the gutter hunks against HEAD and whether each
//! is staged. One per document (`DocumentStore::diff_for`), so split views of
//! a file share one diff instead of computing it each, as Zed's `BufferDiff`.

use std::{sync::Arc, time::Duration};

use gpui::{Context, Entity, Subscription, Task, WeakEntity};

use crate::{
    document::Document,
    editor::GitFile,
    git::{self, Hunk},
};

/// How long the diff waits after an edit, so typing doesn't diff every key.
const EDIT_DELAY: Duration = Duration::from_millis(120);

pub struct DocumentDiff {
    doc: WeakEntity<Document>,
    /// Where the file lives in its repository (None: outside one).
    git_file: Option<GitFile>,
    /// The file's text at HEAD; changes are shown against it.
    base: Option<Arc<str>>,
    /// The file's staged text, to tell staged changes from unstaged ones.
    index: Option<Arc<str>>,
    hunks: Arc<Vec<Hunk>>,
    /// Per hunk: whether it's fully staged.
    staged: Arc<Vec<bool>>,
    /// The document (id, version) `hunks` were computed for, or are being.
    diffed: Option<(u64, u64)>,
    task: Option<Task<()>>,
    /// Diffs computed, for tests.
    #[cfg(test)]
    pub(crate) runs: usize,
    _observer: Subscription,
}

impl DocumentDiff {
    pub fn new(doc: &Entity<Document>, cx: &mut Context<Self>) -> Self {
        let observer = cx.observe(doc, |diff, _, cx| diff.schedule(EDIT_DELAY, cx));
        Self {
            doc: doc.downgrade(),
            git_file: None,
            base: None,
            index: None,
            hunks: Arc::default(),
            staged: Arc::default(),
            diffed: None,
            task: None,
            #[cfg(test)]
            runs: 0,
            _observer: observer,
        }
    }

    /// Sets the text changes are measured against (HEAD) and the staged
    /// text, or clears them for files outside a repository or not committed yet.
    pub fn set_base(&mut self, git_file: Option<GitFile>, base: Option<String>, index: Option<String>, cx: &mut Context<Self>) {
        self.git_file = git_file;
        self.base = base.map(Arc::from);
        self.index = index.map(Arc::from);
        self.diffed = None;
        self.schedule(Duration::ZERO, cx);
        cx.notify();
    }

    pub fn git_file(&self) -> Option<&GitFile> {
        self.git_file.as_ref()
    }

    /// Whether there's a HEAD text to compare with.
    pub fn has_base(&self) -> bool {
        self.base.is_some()
    }

    pub fn hunks(&self) -> &Arc<Vec<Hunk>> {
        &self.hunks
    }

    pub fn staged(&self) -> &Arc<Vec<bool>> {
        &self.staged
    }

    /// Diffs the document again unless it's what was (or is being) diffed;
    /// an edit or a notify that changed nothing (diagnostics) costs nothing.
    fn schedule(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.upgrade() else { return };
        let Some(base) = self.base.clone() else {
            if !self.hunks.is_empty() {
                self.hunks = Arc::default();
                self.staged = Arc::default();
                cx.notify();
            }
            return;
        };
        let doc = doc.read(cx);
        let at = (doc.id(), doc.version());
        if self.diffed == Some(at) || doc.is_stand_in() {
            return;
        }
        self.diffed = Some(at);
        let rope = doc.rope().clone();
        let index = self.index.clone();
        // The first diff of a document (or of a new base) shows at once.
        let delay = if self.hunks.is_empty() { Duration::ZERO } else { delay };
        self.task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let (hunks, staged) = cx
                .background_executor()
                .spawn(async move {
                    let text = rope.to_string();
                    let hunks = git::diff_hunks(&base, &text);
                    // A change is staged when nothing differs from the index there.
                    let unstaged = index.map(|index| git::diff_hunks(&index, &text));
                    let staged: Vec<bool> = hunks
                        .iter()
                        .map(|h| unstaged.as_ref().is_some_and(|u| !u.iter().any(|u| git::rows_touch(&u.new_rows, &h.new_rows))))
                        .collect();
                    (hunks, staged)
                })
                .await;
            this.update(cx, |diff, cx| {
                #[cfg(test)]
                {
                    diff.runs += 1;
                }
                diff.hunks = Arc::new(hunks);
                diff.staged = Arc::new(staged);
                cx.notify();
            })
            .ok();
        }));
    }
}
