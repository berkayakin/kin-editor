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
                    if change.structural && change.path.parent().is_some_and(|p| self.expanded.contains(p)) {
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
            self.rebuild_tree();
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
                if let Some(Modal::Picker(picker)) = &ws.modal {
                    let files = ws.file_index.clone();
                    picker.update(cx, |p, cx| p.set_files(files, cx));
                }
            })
            .ok();
        }));
    }

    pub(super) fn rebuild_tree(&mut self) {
        self.entries.clear();
        if let Some(root) = self.root.clone() {
            self.push_children(&root, 0);
        }
    }

    pub(super) fn push_children(&mut self, dir: &Path, depth: usize) {
        for (path, is_dir, cycle, name) in tree_children(dir) {
            let expanded = is_dir && !cycle && self.expanded.contains(&path);
            self.entries.push(TreeEntry {
                path: path.clone(),
                name: name.into(),
                depth,
                is_dir,
            });
            if expanded {
                self.push_children(&path, depth + 1);
            }
        }
    }

    pub(super) fn click_entry(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix).cloned() else { return };
        // Never `open_path` a folder: that opens it as the project.
        if entry.is_dir || entry.path.is_dir() {
            if !self.expanded.remove(&entry.path) {
                self.expanded.insert(entry.path);
            }
            self.rebuild_tree();
            self.schedule_session_save(cx);
            cx.notify();
        } else {
            self.open_path(entry.path, window, cx);
        }
    }
}
