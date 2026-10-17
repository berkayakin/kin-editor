//! Watching the project folder: what a change on disk updates.

use super::*;

impl Workspace {
    // ---- Watching the disk --------------------------------------------------------

    /// Watches the project folder so changes made outside Kuana show up right away.
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
        let mut git = false;
        let mut structural = Vec::new();
        // For language servers watching files.
        let mut file_changes = Vec::new();
        let mut paths = HashSet::new();
        // Created, removed or renamed: the tree reads again the folders it
        // has read that they're in.
        let mut tree = Vec::new();
        // The watcher lost events: everything may have changed.
        if changes.iter().any(|c| c.path == root) {
            self.tree.update(cx, |t, cx| t.relist_all(cx));
            self.refresh_file_index(cx);
            self.refresh_git(cx);
            self.reload_documents(|_| true, cx);
            cx.notify();
            return;
        }
        for change in changes {
            match watcher::git_path(&change.path) {
                GitPath::Metadata => git = true,
                GitPath::Internal => {}
                GitPath::WorkTree => {
                    let tree_read = |path: &Path| self.tree.read(cx).has_read(path);
                    if change.structural && (change.path.parent().is_some_and(tree_read) || tree_read(&change.path)) {
                        tree.push(change.path.clone());
                    }
                    if watcher::in_heavy_folder(&root, &change.path) || !change.path.starts_with(&root) {
                        continue;
                    }
                    if change.path.file_name().is_some_and(|n| n == ".gitignore") {
                        self.ignore = project_ignore(&root);
                    }
                    // By the project's rules, or inside what Git reported as
                    // ignored (nested .gitignore files and global excludes
                    // included): build output churning there needs no status.
                    let ignored = |path: &Path| {
                        path.starts_with(&root)
                            && (self.ignore.as_ref().is_some_and(|i| i.matched_path_or_any_parents(path, false).is_ignore())
                                || path.ancestors().skip(1).any(|p| self.git_ignored.contains(p)))
                    };
                    if change.structural {
                        structural.push(change.path.clone());
                    }
                    let kind = match (change.path.exists(), change.structural) {
                        (false, _) => FileChange::Deleted,
                        (true, true) => FileChange::Created,
                        (true, false) => FileChange::Changed,
                    };
                    file_changes.push((change.path.clone(), kind));
                    // Churn in what Git ignores doesn't change its status; a
                    // new ignored file or folder does (the tree dims it).
                    let ignored_known = || change.path.ancestors().any(|p| self.git_ignored.contains(p));
                    git |= !ignored(&change.path) || (kind == FileChange::Created && !ignored_known());
                    paths.insert(change.path);
                }
            }
        }
        if !tree.is_empty() {
            self.tree.update(cx, |t, cx| t.paths_changed(tree.iter().map(PathBuf::as_path), cx));
        }
        if !structural.is_empty() {
            self.update_project(structural, cx);
        }
        if !file_changes.is_empty() {
            self.lsp.read(cx).files_changed(&file_changes);
        }
        if git {
            self.refresh_git(cx);
        }
        self.reload_documents(|path| paths.contains(path), cx);
        for image in self.images() {
            if paths.iter().any(|p| p == image.read(cx).path()) {
                image.update(cx, |i, cx| i.reload_if_changed(cx));
            }
        }
        // An edit from outside (a formatter in the terminal, another editor)
        // that leaves Git's status as it was: the diff of the file shows it.
        if let Some(Modal::Diff(view)) = &self.modals.current
            && view.read(cx).working_tree_file().is_some_and(|p| paths.contains(p))
        {
            view.update(cx, |v, cx| v.reload(cx));
        }
    }

    /// Has the documents whose files `wanted` picks read them again (in the
    /// background), each through one of its tabs: splits share it.
    pub(super) fn reload_documents(&mut self, wanted: impl Fn(&Path) -> bool, cx: &mut Context<Self>) {
        let mut seen = HashSet::new();
        for editor in self.editors.clone() {
            let doc = editor.read(cx).doc.read(cx);
            if editor.read(cx).is_loaded() && doc.path.as_deref().is_some_and(&wanted) && seen.insert(doc.id()) {
                editor.update(cx, |e, cx| e.reload_if_changed(cx));
            }
        }
    }

    /// Scans the project's files in the background (a new project, new
    /// ignore rules); changes waiting meanwhile are applied after.
    pub(super) fn refresh_file_index(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            return;
        };
        self.project_changes.clear();
        self.project_task = Some(cx.spawn(async move |this, cx| {
            let snapshot = cx.background_executor().spawn(async move { ProjectSnapshot::scan(&root) }).await;
            this.update(cx, |ws, cx| ws.project_updated(snapshot, cx)).ok();
        }));
    }

    /// Paths created, removed or renamed on disk: the project's files catch
    /// up in the background, without scanning everything again.
    pub(super) fn update_project(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.project_changes.extend(paths);
        if self.project_task.is_none() {
            self.apply_project_changes(cx);
        }
    }

    fn apply_project_changes(&mut self, cx: &mut Context<Self>) {
        if self.project_changes.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut self.project_changes);
        let base = self.project.clone();
        self.project_task = Some(cx.spawn(async move |this, cx| {
            let (snapshot, applied) = cx
                .background_executor()
                .spawn(async move {
                    let mut snapshot = (*base).clone();
                    let applied = snapshot.apply(paths.iter().map(PathBuf::as_path));
                    (snapshot, applied)
                })
                .await;
            this.update(cx, |ws, cx| match applied {
                Applied::Rescan => ws.refresh_file_index(cx),
                Applied::Changed => ws.project_updated(snapshot, cx),
                Applied::Unchanged => ws.project_updated_unchanged(cx),
            })
            .ok();
        }));
    }

    fn project_updated(&mut self, snapshot: ProjectSnapshot, cx: &mut Context<Self>) {
        // Freeing a big project's paths takes a while: not on the UI thread.
        let old = std::mem::replace(&mut self.project, Arc::new(snapshot));
        cx.background_executor().spawn(async move { drop(old) }).detach();
        if let Some(Modal::Picker(picker)) = &self.modals.current {
            let files = self.project.openable();
            picker.update(cx, |p, cx| p.set_files(files, cx));
        }
        self.project_updated_unchanged(cx);
    }

    /// The running update is done: the next one, if changes came meanwhile.
    fn project_updated_unchanged(&mut self, cx: &mut Context<Self>) {
        // This task is running; let it finish instead of dropping it.
        if let Some(task) = self.project_task.take() {
            task.detach();
        }
        self.apply_project_changes(cx);
    }
}
