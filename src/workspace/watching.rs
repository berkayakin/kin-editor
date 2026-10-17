//! Watching the project folder: what a change on disk updates. Each batch
//! of changes is sorted out in the background (`classify`: it stats paths,
//! matches ignore rules and language servers' globs), so churn in build
//! output or a venv costs the UI thread a short summary, not a look at
//! every path.

use ignore::gitignore::Gitignore;

use super::*;
use crate::lsp::{FileWatchers, ServerKey};

impl Workspace {
    // ---- Watching the disk --------------------------------------------------------

    /// Watches the project folder so changes made outside Kuana show up right away.
    pub(super) fn start_watching(&mut self, cx: &mut Context<Self>) {
        self.watcher = None;
        self.watch_task = None;
        let Some(root) = self.root.clone() else { return };
        self.ignore = project_ignore(&root).map(Arc::new);
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
                let Ok(state) = this.update(cx, |ws, cx| ws.watch_state(cx)) else { break };
                let Some(state) = state else { continue };
                let changed = cx.background_executor().spawn(async move { classify(batch, &state) }).await;
                if this.update(cx, |ws, cx| ws.disk_changed(changed, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    /// What sorting out a batch of changes goes by, as it is now.
    fn watch_state(&self, cx: &App) -> Option<WatchState> {
        let root = self.root.clone()?;
        let mut open: HashSet<PathBuf> = self.editors.iter().filter_map(|e| e.read(cx).doc.read(cx).path.clone()).collect();
        open.extend(self.images().iter().map(|i| i.read(cx).path().to_path_buf()));
        if let Some(Modal::Diff(view)) = &self.modals.current
            && let Some(path) = view.read(cx).working_tree_file()
        {
            open.insert(path.to_path_buf());
        }
        Some(WatchState {
            root,
            ignore: self.ignore.clone(),
            git_ignored: self.git_ignored.clone(),
            tree_read: self.tree.read(cx).read_folders(),
            open,
            watchers: self.lsp.read(cx).file_watchers(),
        })
    }

    /// Updates what a batch of changes on disk affects (`classify` sorted
    /// it out): the tree (in folders it has read), Go to File, Git status,
    /// language servers and open files.
    pub(super) fn disk_changed(&mut self, changed: Classified, cx: &mut Context<Self>) {
        if self.root.is_none() {
            return;
        }
        // A venv may have been created or removed.
        self.venv_shown.take();
        // The watcher lost events: everything may have changed.
        if changed.everything {
            self.tree.update(cx, |t, cx| t.relist_all(cx));
            self.refresh_file_index(cx);
            self.refresh_git(cx);
            self.reload_documents(|_| true, cx);
            cx.notify();
            return;
        }
        if let Some(ignore) = changed.ignore {
            self.ignore = ignore;
        }
        if !changed.tree.is_empty() {
            self.tree.update(cx, |t, cx| t.paths_changed(changed.tree.iter().map(PathBuf::as_path), cx));
        }
        if !changed.structural.is_empty() {
            self.update_project(changed.structural, cx);
        }
        if !changed.lsp.is_empty() {
            self.lsp.read(cx).send_file_events(changed.lsp);
        }
        if changed.git {
            self.refresh_git(cx);
        }
        let paths = changed.open;
        if paths.is_empty() {
            return;
        }
        self.reload_documents(|path| paths.contains(path), cx);
        for image in self.images() {
            if paths.contains(image.read(cx).path()) {
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

/// What a batch of changes is sorted out by (`classify`), taken from the
/// workspace when it comes.
pub(super) struct WatchState {
    root: PathBuf,
    /// The project's own ignore rules.
    ignore: Option<Arc<Gitignore>>,
    /// What Git reported as ignored.
    git_ignored: Arc<HashSet<PathBuf>>,
    /// The folders the tree has read (or is reading).
    tree_read: HashSet<PathBuf>,
    /// Open files: tabs, pictures, the diff view's.
    open: HashSet<PathBuf>,
    watchers: FileWatchers,
}

/// What a batch of changes affects.
#[derive(Default)]
pub(super) struct Classified {
    /// The watcher lost track: anything may have changed.
    everything: bool,
    /// Created, removed or renamed in folders the tree has read.
    tree: Vec<PathBuf>,
    /// Created, removed or renamed in the project, for Go to File (which
    /// skips what's in ignored folders itself).
    structural: Vec<PathBuf>,
    /// The project's ignore rules, read again (a .gitignore changed).
    ignore: Option<Option<Arc<Gitignore>>>,
    /// Git status needs reading again.
    git: bool,
    /// Open files that changed.
    open: HashSet<PathBuf>,
    /// Each language server's `didChangeWatchedFiles`, for what it watches.
    lsp: Vec<(ServerKey, serde_json::Value)>,
}

/// Sorts out a batch of changes (blocking: in the background). Paths in
/// `target` and `node_modules` matter to the tree only; churn in what Git
/// ignores needs no status (a new ignored file or folder does: the tree
/// dims it).
pub(super) fn classify(changes: Vec<FsChange>, state: &WatchState) -> Classified {
    let root = &state.root;
    let mut out = Classified::default();
    if changes.iter().any(|c| &c.path == root) {
        out.everything = true;
        return out;
    }
    // New rules count for the whole batch.
    let mut ignore = state.ignore.clone();
    let in_project = |path: &Path| path.starts_with(root) && !watcher::in_heavy_folder(root, path);
    if changes.iter().any(|c| in_project(&c.path) && c.path.file_name().is_some_and(|n| n == ".gitignore") && watcher::git_path(&c.path) == GitPath::WorkTree) {
        ignore = project_ignore(root).map(Arc::new);
        out.ignore = Some(ignore.clone());
    }
    // By the project's rules, or inside what Git reported as ignored
    // (nested .gitignore files and global excludes included).
    let ignored = |path: &Path| {
        ignore.as_ref().is_some_and(|i| i.matched_path_or_any_parents(path, false).is_ignore())
            || path.ancestors().skip(1).any(|p| state.git_ignored.contains(p))
    };
    let mut exists: HashMap<PathBuf, bool> = HashMap::new();
    let mut file_changes = Vec::new();
    for change in changes {
        match watcher::git_path(&change.path) {
            GitPath::Metadata => out.git = true,
            GitPath::Internal => {}
            GitPath::WorkTree => {
                let tree_read = |path: &Path| state.tree_read.contains(path);
                if change.structural && (change.path.parent().is_some_and(tree_read) || tree_read(&change.path)) {
                    out.tree.push(change.path.clone());
                }
                if !in_project(&change.path) {
                    continue;
                }
                if change.structural {
                    out.structural.push(change.path.clone());
                }
                let is_ignored = ignored(&change.path);
                // The kind of change only matters to these.
                let kind = (is_ignored || !state.watchers.is_empty()).then(|| {
                    let there = *exists.entry(change.path.clone()).or_insert_with(|| change.path.exists());
                    match (there, change.structural) {
                        (false, _) => FileChange::Deleted,
                        (true, true) => FileChange::Created,
                        (true, false) => FileChange::Changed,
                    }
                });
                let known = || change.path.ancestors().any(|p| state.git_ignored.contains(p));
                out.git |= !is_ignored || (kind == Some(FileChange::Created) && !known());
                if let Some(kind) = kind.filter(|_| !state.watchers.is_empty()) {
                    file_changes.push((change.path.clone(), kind));
                }
                if state.open.contains(&change.path) {
                    out.open.insert(change.path);
                }
            }
        }
    }
    out.lsp = state.watchers.events(&file_changes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(root: &Path) -> WatchState {
        WatchState {
            root: root.to_path_buf(),
            ignore: project_ignore(root).map(Arc::new),
            git_ignored: Arc::new([root.join("dist")].into_iter().collect()),
            tree_read: [root.to_path_buf(), root.join("src")].into_iter().collect(),
            open: [root.join("src/a.rs")].into_iter().collect(),
            watchers: FileWatchers::default(),
        }
    }

    #[test]
    fn a_batch_is_sorted_out_by_what_it_affects() {
        let root = std::env::temp_dir().join(format!("kuana-classify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["src", "dist/out", ".git", "node_modules/x"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        for file in ["src/a.rs", "src/new.rs", "dist/out/x.js", "debug.log"] {
            std::fs::write(root.join(file), "").unwrap();
        }
        let state = state(&root);
        let change = |rel: &str, structural| FsChange { path: root.join(rel), structural };

        // Build output in an ignored folder: only Go to File hears of it (it
        // skips ignored folders itself).
        let c = classify(vec![change("dist/out/x.js", true), change("dist/out/y.js", true)], &state);
        assert!(!c.git && c.tree.is_empty() && c.open.is_empty());
        assert_eq!(c.structural.len(), 2);
        // An open file written to: it's read again, and so is Git status.
        let c = classify(vec![change("src/a.rs", false)], &state);
        assert!(c.git && c.tree.is_empty() && c.structural.is_empty());
        assert!(c.open.contains(&root.join("src/a.rs")));
        // A new file where the tree has read: the tree hears of it.
        let c = classify(vec![change("src/new.rs", true)], &state);
        assert_eq!(c.tree, [root.join("src/new.rs")]);
        // A log written to is ignored by the project's rules: no status. A
        // new one needs one (the tree dims it once Git says it's ignored).
        assert!(!classify(vec![change("debug.log", false)], &state).git);
        assert!(classify(vec![change("debug.log", true)], &state).git);
        // Dependencies: the tree only (where it has read).
        let c = classify(vec![change("node_modules/x/i.js", true)], &state);
        assert!(!c.git && c.tree.is_empty() && c.structural.is_empty());
        // Git's own: HEAD, the index, refs; not objects.
        assert!(classify(vec![change(".git/index", false)], &state).git);
        assert!(!classify(vec![change(".git/objects/ab/cdef", true)], &state).git);
        // New ignore rules are read again.
        let c = classify(vec![change(".gitignore", false)], &state);
        assert!(c.ignore.is_some() && c.git);
        // The watcher lost track: everything.
        assert!(classify(vec![FsChange { path: root.clone(), structural: true }], &state).everything);
        std::fs::remove_dir_all(&root).ok();
    }
}
