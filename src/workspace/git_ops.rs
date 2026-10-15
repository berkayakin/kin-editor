//! Git: running operations, refreshing status, diffs and blame.

use super::*;

impl Workspace {
    // ---- Git -------------------------------------------------------------------------

    pub(super) fn detect_repo(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let project = self.root.clone();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    if !git::is_available() {
                        return None;
                    }
                    let repo = git::discover(&dir)?;
                    let user = git::user_name(&repo);
                    let remote = git::remote_url(&repo);
                    Some((repo, user, remote))
                })
                .await;
            this.update(cx, |ws, cx| {
                let (repo, user, remote) = found.map_or((None, None, None), |(r, u, o)| (Some(r), u, o));
                ws.repo = repo.clone();
                ws.git_user = user;
                ws.git_remote = remote;
                ws.last_commit = None;
                ws.git_status = Arc::new(RepoStatus::default());
                ws.git_paths = Arc::new(HashMap::new());
                // A repository above the project folder: watch its .git too.
                if let (Some(repo), Some(root), Some(watcher)) = (&repo, &ws.root, ws.watcher.as_mut())
                    && !repo.starts_with(root)
                    && let Err(err) = watcher.add(&repo.join(".git"))
                {
                    eprintln!("couldn't watch {}: {err}", repo.display());
                }
                ws.git_log.update(cx, |l, cx| l.set_repo(repo.clone(), cx));
                ws.git_panel.update(cx, |p, cx| {
                    p.set_repo(repo, project, cx);
                    p.set_status(Arc::new(RepoStatus::default()), cx);
                });
                ws.refresh_git(cx);
                for editor in ws.tabs.clone() {
                    ws.load_diff_base(&editor, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-reads `git status` in the background.
    pub(super) fn refresh_git(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        if self.status_task.is_some() {
            // Changes made while git was reading must not be missed.
            self.git_refresh_again = true;
            return;
        }
        self.status_task = Some(cx.spawn(async move |this, cx| {
            let repo2 = repo.clone();
            let (result, last_commit) = cx
                .background_executor()
                .spawn(async move { (git::status(&repo2), git::last_commit(&repo2)) })
                .await;
            this.update(cx, |ws, cx| {
                ws.status_task = None;
                if std::mem::take(&mut ws.git_refresh_again) {
                    ws.refresh_git(cx);
                }
                if ws.last_commit != last_commit {
                    ws.last_commit = last_commit;
                    cx.notify();
                }
                let Ok(status) = result else { return };
                if *ws.git_status == status {
                    return;
                }
                let moved = (&ws.git_status.head_oid, &ws.git_status.branch, ws.git_status.ahead, ws.git_status.behind)
                    != (&status.head_oid, &status.branch, status.ahead, status.behind);
                if moved && ws.shows(BottomPanel::GitLog) {
                    ws.git_log.update(cx, |l, cx| l.reload(cx));
                }
                ws.git_paths = Arc::new(status.path_map(&repo));
                ws.git_status = Arc::new(status);
                let status = ws.git_status.clone();
                ws.git_panel.update(cx, |p, cx| p.set_status(status, cx));
                // HEAD or the index may have moved (commits, staging, checkouts).
                for editor in ws.tabs.clone() {
                    ws.load_diff_base(&editor, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Loads the file's HEAD text so the editor can show changed lines.
    pub(super) fn load_diff_base(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        // A tab not read yet gets it when it is (`file_opened`).
        let Some(path) = editor.read(cx).doc.read(cx).path.clone().filter(|_| editor.read(cx).is_loaded()) else { return };
        // Images have no lines to mark.
        if editor.read(cx).doc.read(cx).image.is_some() {
            return;
        }
        let Ok(rel) = path.strip_prefix(&repo) else {
            editor.update(cx, |e, cx| e.set_diff_base(None, None, None, cx));
            return;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let git_file = GitFile { root: repo.clone(), rel: rel.clone(), user_name: self.git_user.clone() };
        // Compared as the editor holds the file: decoded like it, LF line ends.
        let encoding = editor.read(cx).doc.read(cx).encoding;
        let editor = editor.downgrade();
        cx.spawn(async move |_, cx| {
            let (base, index) = cx
                .background_executor()
                .spawn(async move {
                    let text = |bytes: Option<Vec<u8>>| bytes.map(|b| git::BlobText::new(&b, Some(encoding)).text);
                    (text(git::head_blob(&repo, &rel)), text(git::index_blob(&repo, &rel)))
                })
                .await;
            editor.update(cx, |e, cx| e.set_diff_base(Some(git_file), base, index, cx)).ok();
        })
        .detach();
    }

    /// Picks up on-disk changes (checkout, pull, rollback) in unmodified tabs.
    pub(super) fn reload_changed_files(&mut self, cx: &mut Context<Self>) {
        for editor in self.tabs.clone() {
            editor.update(cx, |e, cx| e.reload_if_changed(cx));
        }
        self.tree.update(cx, |t, cx| t.rebuild(cx));
    }

    pub(super) fn run_git(&mut self, op: GitOp, window: &mut Window, cx: &mut Context<Self>) {
        // Push asks first, like JetBrains: the commits to push, then Push or Cancel.
        if matches!(op, GitOp::Push) {
            self.show_push(window, cx);
            return;
        }
        // Marking a file resolved while it still has conflict markers is
        // usually a mistake, so ask.
        if let (GitOp::Stage(rels), Some(repo)) = (&op, &self.repo) {
            let unresolved: Vec<&String> = rels
                .iter()
                .filter(|rel| self.git_status.conflicted().any(|f| &f.rel == *rel))
                .filter(|rel| std::fs::read_to_string(repo.join(rel)).is_ok_and(|t| !crate::conflict::parse(&t).is_empty()))
                .collect();
            if !unresolved.is_empty() {
                let message = match unresolved.as_slice() {
                    [one] => format!("{one} still has conflict markers"),
                    many => format!("{} files still have conflict markers", many.len()),
                };
                let rx = window.prompt(PromptLevel::Warning, &message, Some("Mark as resolved anyway?"), &["Mark as Resolved", "Cancel"], cx);
                cx.spawn_in(window, async move |this, cx| {
                    if rx.await == Ok(0) {
                        this.update(cx, |ws, cx| ws.execute_git(op, cx)).ok();
                    }
                })
                .detach();
                return;
            }
        }
        if let GitOp::Discard(files) = &op {
            // Rolling back can't be undone, so ask first.
            let message = match files.as_slice() {
                [one] => format!("Roll back changes to {}?", one.rel),
                many => format!("Roll back changes to {} files?", many.len()),
            };
            let untracked = files.iter().any(|f| f.unstaged == Some(Change::Untracked));
            let detail = if untracked {
                "Untracked files will be deleted. This can't be undone."
            } else {
                "Local changes will be lost. This can't be undone."
            };
            let rx = window.prompt(PromptLevel::Warning, &message, Some(detail), &["Roll Back", "Cancel"], cx);
            cx.spawn_in(window, async move |this, cx| {
                if rx.await == Ok(0) {
                    this.update(cx, |ws, cx| ws.execute_git(op, cx)).ok();
                }
            })
            .detach();
            return;
        }
        self.execute_git(op, cx);
    }

    pub(super) fn execute_git(&mut self, op: GitOp, cx: &mut Context<Self>) {
        let dir = match (&op, &self.repo, &self.root) {
            (GitOp::Init, _, Some(root)) => root.clone(),
            (_, Some(repo), _) => repo.clone(),
            _ => return,
        };
        if self.git_busy.is_some() {
            self.toast(ToastKind::Error, "Another Git operation is still running", None, cx);
            return;
        }
        let label = op.busy_label();
        self.git_busy = Some(label);
        self.git_panel.update(cx, |p, cx| p.set_busy(Some(label.into()), cx));
        let status = self.git_status.clone();
        let op2 = op.clone();
        self.git_op_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { perform_git(&dir, op2, &status) }).await;
            this.update(cx, |ws, cx| {
                ws.git_busy = None;
                ws.git_panel.update(cx, |p, cx| p.set_busy(None, cx));
                match result {
                    Ok(message) => {
                        if let Some(message) = message {
                            // A merge or rebase that stopped on conflicts isn't done yet.
                            let kind = if message.contains("conflicts") { ToastKind::Info } else { ToastKind::Success };
                            ws.toast(kind, message, None, cx);
                        }
                        match &op {
                            GitOp::Commit { .. } => ws.git_panel.update(cx, |p, cx| p.committed(cx)),
                            GitOp::Init => {
                                if let Some(root) = ws.root.clone() {
                                    ws.detect_repo(root, cx);
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(err) => {
                        let title = match &op {
                            GitOp::Push | GitOp::PushNow { .. } | GitOp::Commit { push: true, .. } => "Push failed",
                            GitOp::Pull => "Update failed",
                            GitOp::Fetch => "Fetch failed",
                            GitOp::Commit { .. } => "Commit failed",
                            GitOp::Checkout(_) | GitOp::CreateBranch(_) => "Checkout failed",
                            _ => "Git operation failed",
                        };
                        ws.toast(ToastKind::Error, title, Some(err), cx);
                    }
                }
                if matches!(
                    op,
                    GitOp::Discard(_) | GitOp::Pull | GitOp::Checkout(_) | GitOp::Merge(_) | GitOp::Rebase(_) | GitOp::Abort(_) | GitOp::Continue(_)
                ) {
                    ws.reload_changed_files(cx);
                }
                if ws.shows(BottomPanel::GitLog)
                    && matches!(
                        op,
                        GitOp::Commit { .. }
                            | GitOp::Fetch
                            | GitOp::Pull
                            | GitOp::Push
                            | GitOp::PushNow { .. }
                            | GitOp::Checkout(_)
                            | GitOp::CreateBranch(_)
                            | GitOp::Merge(_)
                            | GitOp::Rebase(_)
                            | GitOp::Abort(_)
                            | GitOp::Continue(_)
                    )
                {
                    ws.git_log.update(cx, |l, cx| l.reload(cx));
                }
                // Staging part of a file may leave its status as it was.
                if matches!(op, GitOp::StageLines { .. } | GitOp::UnstageLines { .. }) {
                    for editor in ws.tabs.clone() {
                        ws.load_diff_base(&editor, cx);
                    }
                }
                if let Some(Modal::Diff(view)) = &ws.modal {
                    view.update(cx, |v, cx| v.reload(cx));
                }
                ws.status_task = None;
                ws.refresh_git(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn toast(&mut self, kind: ToastKind, message: impl Into<SharedString>, detail: Option<String>, cx: &mut Context<Self>) {
        let id = self.next_toast;
        self.next_toast += 1;
        self.toasts.push(Toast { id, kind, message: message.into(), detail: detail.map(Into::into) });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
        let seconds = if kind == ToastKind::Error { 9 } else { 4 };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(seconds)).await;
            this.update(cx, |ws, cx| {
                ws.toasts.retain(|t| t.id != id);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub(super) fn git_push(&mut self, _: &GitPush, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(GitOp::Push, window, cx);
    }

    pub(super) fn git_pull(&mut self, _: &GitPull, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(GitOp::Pull, window, cx);
    }

    pub(super) fn git_fetch(&mut self, _: &GitFetch, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(GitOp::Fetch, window, cx);
    }

    pub(super) fn git_branches(&mut self, _: &GitBranches, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            self.toast(ToastKind::Error, "Not a Git repository", None, cx);
            return;
        };
        self.close_modal(window, cx);
        let picker = cx.new(|cx| BranchPicker::new(repo, window, cx));
        self.modal_subscription = Some(cx.subscribe_in(&picker, window, |ws, _, event: &BranchPickerEvent, window, cx| match event {
            BranchPickerEvent::Run(op) => {
                ws.close_modal(window, cx);
                ws.run_git(op.clone(), window, cx);
            }
            BranchPickerEvent::Dismissed => ws.close_modal(window, cx),
        }));
        self.modal = Some(Modal::Branches(picker));
        cx.notify();
    }

    pub(super) fn git_clone(&mut self, _: &GitClone, window: &mut Window, cx: &mut Context<Self>) {
        if !git::is_available() {
            self.toast(ToastKind::Error, "Git isn't installed", Some("Install Git and make sure it's on your PATH.".into()), cx);
            return;
        }
        self.close_modal(window, cx);
        let parent = clone_dialog::default_parent(self.root.as_deref());
        let dialog = cx.new(|cx| CloneDialog::new(parent, window, cx));
        self.modal_subscription = Some(cx.subscribe_in(&dialog, window, |ws, _, event: &CloneDialogEvent, window, cx| match event {
            CloneDialogEvent::Cloned(path) => {
                ws.close_modal(window, cx);
                ws.toast(ToastKind::Success, format!("Cloned {}", folder_name(path)), Some(clone_dialog::display_path(path)), cx);
                ws.open_project(path.clone(), OpenIn::Auto, window, cx);
            }
            CloneDialogEvent::Dismissed => ws.close_modal(window, cx),
        }));
        self.modal = Some(Modal::Clone(dialog));
        cx.notify();
    }

    pub(super) fn git_init(&mut self, _: &GitInit, window: &mut Window, cx: &mut Context<Self>) {
        match (&self.root, &self.repo) {
            (None, _) => self.toast(ToastKind::Error, "Open a folder first", None, cx),
            (Some(_), Some(_)) => self.toast(ToastKind::Error, "This folder is already in a Git repository", None, cx),
            (Some(_), None) => self.run_git(GitOp::Init, window, cx),
        }
    }

    pub(super) fn open_remote(&mut self, _: &OpenRemote, _: &mut Window, cx: &mut Context<Self>) {
        match self.git_remote.as_deref().and_then(git::web_url) {
            Some(url) => cx.open_url(&url),
            None => self.toast(ToastKind::Error, "No browsable remote", Some("This repository has no HTTPS or SSH remote.".into()), cx),
        }
    }

    pub(super) fn remove_recent_project(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.update_recent_projects(|list| list.retain(|p| p != path));
        cx.notify();
    }

    /// Opens a recent project here, or in a new window when `new_window`
    /// (⌘/Ctrl held).
    pub(super) fn open_recent_project(&mut self, path: PathBuf, new_window: bool, window: &mut Window, cx: &mut Context<Self>) {
        let target = if new_window { OpenIn::NewWindow } else { OpenIn::ThisWindow };
        self.open_project(path, target, window, cx);
    }

    pub(super) fn recent_projects(&mut self, _: &RecentProjects, window: &mut Window, cx: &mut Context<Self>) {
        self.recent_projects = SessionState::load().recent_projects;
        self.open_picker(PickerMode::Projects, window, cx);
    }

    pub(super) fn open_diff(&mut self, file: FileStatus, staged: bool, window: &mut Window, cx: &mut Context<Self>) {
        let buffer = self
            .tabs
            .iter()
            .find(|t| t.read(cx).doc.read(cx).path.as_deref() == Some(file.path.as_path()))
            .map(|t| t.read(cx).text(cx));
        self.show_diff(DiffSource::Working { file, staged, buffer }, window, cx);
    }

    pub(super) fn show_push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            self.toast(ToastKind::Error, "Not a Git repository", None, cx);
            return;
        };
        self.close_modal(window, cx);
        self.diff_return = None;
        let dialog = cx.new(|cx| PushDialog::new(repo, window, cx));
        self.open_push_modal(dialog, window, cx);
    }

    pub(super) fn open_push_modal(&mut self, dialog: Entity<PushDialog>, window: &mut Window, cx: &mut Context<Self>) {
        self.modal_subscription = Some(cx.subscribe_in(&dialog, window, |ws, dialog, event: &PushDialogEvent, window, cx| match event {
            PushDialogEvent::Push { force } => {
                ws.close_modal(window, cx);
                ws.run_git(GitOp::PushNow { force: *force }, window, cx);
            }
            PushDialogEvent::OpenDiff(source) => {
                ws.diff_return = Some(dialog.clone());
                ws.show_diff(source.clone(), window, cx);
            }
            PushDialogEvent::Dismissed => ws.close_modal(window, cx),
        }));
        dialog.read(cx).focus(window);
        self.modal = Some(Modal::Push(dialog));
        cx.notify();
    }

    pub(super) fn show_diff(&mut self, source: DiffSource, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        self.close_modal(window, cx);
        let side_by_side = SessionState::load().diff_side_by_side.unwrap_or(true);
        let view = cx.new(|cx| DiffView::new(repo, source, side_by_side, window, cx));
        self.modal_subscription = Some(cx.subscribe_in(&view, window, |ws, _, event: &DiffViewEvent, window, cx| match event {
            DiffViewEvent::Close => {
                ws.close_modal(window, cx);
                // Back to the Push dialog the diff came from.
                if let Some(dialog) = ws.diff_return.take() {
                    ws.open_push_modal(dialog, window, cx);
                }
            }
            DiffViewEvent::OpenFile(path) => {
                ws.diff_return = None;
                ws.close_modal(window, cx);
                ws.open_path(path.clone(), window, cx);
            }
            DiffViewEvent::Run(op) => {
                ws.diff_return = None;
                // Staging a change keeps the diff open (it reloads afterwards).
                if !matches!(op, GitOp::StageLines { .. } | GitOp::UnstageLines { .. }) {
                    ws.close_modal(window, cx);
                }
                ws.run_git(op.clone(), window, cx);
            }
            DiffViewEvent::Layout { side_by_side } => {
                // App-wide, like the font zoom.
                let mut state = SessionState::load();
                state.diff_side_by_side = Some(*side_by_side);
                state.save().ok();
            }
        }));
        self.modal = Some(Modal::Diff(view));
        cx.notify();
    }

    pub(super) fn toggle_problems(&mut self, _: &ToggleProblems, window: &mut Window, cx: &mut Context<Self>) {
        // The terminal it replaces had the keyboard; the file gets it back.
        let had_terminal = self.shows(BottomPanel::Terminal);
        self.toggle_bottom(BottomPanel::Problems);
        if had_terminal && self.shows(BottomPanel::Problems) {
            self.focus_active(window, cx);
        }
        self.save_state();
        cx.notify();
    }
}



/// Runs one Git operation; blocking. Returns a message for a success toast.
pub(super) fn perform_git(dir: &Path, op: GitOp, status: &RepoStatus) -> Result<Option<String>, String> {
    let push = |dir: &Path, force: bool| -> Result<String, String> {
        if !git::has_remote(dir) {
            return Err("No remote is configured. Add one with: git remote add origin <url>".into());
        }
        let branch = git::status(dir).ok().and_then(|s| s.branch).or_else(|| status.branch.clone());
        let has_upstream = git::status(dir).ok().and_then(|s| s.upstream).is_some();
        let verb = if force { "Force-pushed" } else { "Pushed" };
        match (has_upstream, branch) {
            (false, Some(branch)) => git::push(dir, Some(&branch), force).map(|_| format!("{verb} {branch} to origin")),
            _ => git::push(dir, None, force).map(|_| verb.to_string()),
        }
    };
    match op {
        GitOp::Stage(rels) => git::stage(dir, &rels).map(|_| None),
        GitOp::Unstage(rels) => git::unstage(dir, &rels, status.head_oid.is_some()).map(|_| None),
        GitOp::StageLines { rel, text, rows } => git::stage_lines(dir, &rel, &text, rows).map(|_| None),
        GitOp::UnstageLines { rel, text, rows } => git::unstage_lines(dir, &rel, text.as_deref(), rows).map(|_| None),
        GitOp::Merge(branch) => git::merge(dir, &branch).map(|out| {
            Some(if out == "CONFLICT" {
                "Merge stopped on conflicts".into()
            } else if out.contains("Already up to date") {
                "Already up to date".into()
            } else {
                format!("Merged {branch}")
            })
        }),
        GitOp::Rebase(onto) => git::rebase(dir, &onto).map(|out| {
            Some(if out == "CONFLICT" { "Rebase stopped on conflicts".into() } else { format!("Rebased onto {onto}") })
        }),
        GitOp::Abort(state) => git::abort(dir, state).map(|_| Some(format!("{} aborted", state.noun()))),
        GitOp::Continue(state) => git::continue_op(dir, state).map(|_| {
            Some(match git::repo_state(dir) {
                // A rebase can stop again on the next commit.
                Some(_) => "Stopped on more conflicts".into(),
                None => format!("{} finished", state.noun()),
            })
        }),
        GitOp::Discard(files) => {
            let n = files.len();
            git::discard(dir, &files).map(|_| Some(format!("Rolled back {n} file{}", if n == 1 { "" } else { "s" })))
        }
        GitOp::Commit { message, amend, all, push: and_push } => {
            if all {
                git::run(dir, &["add", "--all"])?;
            }
            let summary = git::commit(dir, &message, amend)?;
            if and_push {
                push(dir, false)?;
                Ok(Some(format!("Committed and pushed {summary}")))
            } else {
                Ok(Some(format!("Committed {summary}")))
            }
        }
        // `Push` opens the dialog before it gets here; treat it as a plain push.
        GitOp::Push | GitOp::PushNow { force: false } => push(dir, false).map(Some),
        GitOp::PushNow { force: true } => push(dir, true).map(Some),
        GitOp::Pull => git::pull(dir).map(|out| {
            Some(if out.contains("Already up to date") { "Already up to date".into() } else { "Project updated".into() })
        }),
        GitOp::Fetch => git::fetch(dir).map(|_| Some("Fetched all remotes".into())),
        GitOp::Checkout(branch) => git::checkout(dir, &branch).map(|_| Some(format!("Switched to {}", branch.name))),
        GitOp::CreateBranch(name) => git::create_branch(dir, &name).map(|_| Some(format!("Created and switched to {name}"))),
        GitOp::Init => git::init(dir).map(|_| Some("Initialized a Git repository".into())),
    }
}
