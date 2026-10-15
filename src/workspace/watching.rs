//! Watching the project folder: what a change on disk updates.

use super::*;

impl Workspace {
    // ---- Watching the disk --------------------------------------------------------

    /// Watches the project folder so changes made outside Kin show up right away.
    pub(super) fn start_watching(&mut self, cx: &mut Context<Self>) {
        self.watcher = None;
        self.watch_task = None;
        let Some(root) = self.root.clone() else { return };
        self.ignore = project_ignore(&root);
        let (watcher, mut changes) = match Watcher::new(&root) {
            Ok(watching) => watching,
            Err(err) => {
                eprintln!("couldn't watch {}: {err}", root.display());
                return;
            }
        };
        self.watcher = Some(watcher);
        self.watch_task = Some(cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(mut batch) = changes.next().await {
                // A burst (a checkout, npm install) becomes one update.
                cx.background_executor().timer(Duration::from_millis(150)).await;
                while let Ok(more) = changes.try_recv() {
                    batch.extend(more);
                }
                if this.update(cx, |ws, cx| ws.disk_changed(batch, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    /// Updates what a batch of changes on disk affects: the tree (only for
    /// folders it shows), Go to File, Git status and open files.
    pub(super) fn disk_changed(&mut self, changes: Vec<FsChange>, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        // A venv may have been created or removed.
        self.venv_shown.take();
        let (mut tree, mut index, mut git) = (false, false, false);
        let mut paths = HashSet::new();
        for change in changes {
            match watcher::git_path(&change.path) {
                GitPath::Metadata => git = true,
                GitPath::Internal => {}
                GitPath::WorkTree => {
                    if change.structural && change.path.parent().is_some_and(|p| self.tree.read(cx).is_expanded(p)) {
                        tree = true;
                    }
                    if watcher::in_heavy_folder(&root, &change.path) || !change.path.starts_with(&root) {
                        continue;
                    }
                    if change.path.file_name().is_some_and(|n| n == ".gitignore") {
                        self.ignore = project_ignore(&root);
                    }
                    let ignored = |path: &Path| {
                        path.starts_with(&root)
                            && self.ignore.as_ref().is_some_and(|i| i.matched_path_or_any_parents(path, false).is_ignore())
                    };
                    // Go to File lists ignored files, but not inside ignored folders.
                    index |= change.structural && !change.path.parent().is_some_and(ignored);
                    git |= !ignored(&change.path);
                    paths.insert(change.path);
                }
            }
        }
        if tree {
            self.tree.update(cx, |t, cx| t.rebuild(cx));
            cx.notify();
        }
        if index {
            self.refresh_file_index(cx);
        }
        if git {
            self.refresh_git(cx);
        }
        for editor in self.tabs.clone() {
            if editor.read(cx).doc.read(cx).path.as_ref().is_some_and(|p| paths.contains(p)) {
                editor.update(cx, |e, cx| e.reload_if_changed(cx));
            }
        }
    }

    /// Rescans project files in the background (for Go to File).
    pub(super) fn refresh_file_index(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            return;
        };
        self.index_task = Some(cx.spawn(async move |this, cx| {
            let files = cx.background_executor().spawn(async move { openable_files(&root) }).await;
            this.update(cx, |ws, cx| {
                ws.file_index = Arc::new(files);
                if let Some(Modal::Picker(picker)) = &ws.modals.current {
                    let files = ws.file_index.clone();
                    picker.update(cx, |p, cx| p.set_files(files, cx));
                }
            })
            .ok();
        }));
    }
}
