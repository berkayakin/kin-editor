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
        let (mut tree, mut git) = (false, false);
        let mut structural = Vec::new();
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
                    if change.structural {
                        structural.push(change.path.clone());
                    }
                    git |= !ignored(&change.path);
                    paths.insert(change.path);
                }
            }
        }
        if tree {
            self.tree.update(cx, |t, cx| t.rebuild(cx));
            cx.notify();
        }
        if !structural.is_empty() {
            self.update_project(structural, cx);
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
        self.project = Arc::new(snapshot);
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
