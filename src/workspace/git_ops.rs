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
            let (result, last_commit, stashes) = cx
                .background_executor()
                .spawn(async move { (git::status(&repo2), git::last_commit(&repo2), git::stashes(&repo2)) })
                .await;
            this.update(cx, |ws, cx| {
                ws.status_task = None;
                if std::mem::take(&mut ws.git_refresh_again) {
                    ws.refresh_git(cx);
                }
                ws.git_panel.update(cx, |p, cx| p.set_stashes(stashes, cx));
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
                let confirmed = ask(window, &message, "Mark as resolved anyway?", "Mark as Resolved", true, cx);
                cx.spawn_in(window, async move |this, cx| {
                    if confirmed.await {
                        this.update_in(cx, |ws, window, cx| ws.execute_git(op, window, cx)).ok();
                    }
                })
                .detach();
                return;
            }
        }
        if let Some((message, detail, button, undoable)) = confirmation(&op) {
            self.confirm_git(op, &message, detail, button, undoable, window, cx);
            return;
        }
        // Deleting a branch whose commits aren't merged loses them, so look first.
        if let (GitOp::DeleteBranch { name, force: false }, Some(repo)) = (&op, self.repo.clone()) {
            let name = name.clone();
            cx.spawn_in(window, async move |this, cx| {
                let check = name.clone();
                let merged = cx.background_executor().spawn(async move { git::is_merged(&repo, &check) }).await;
                this.update_in(cx, |ws, window, cx| {
                    let (detail, force) = if merged {
                        ("Its commits are all in the current branch.", false)
                    } else {
                        ("It has commits that aren't in the current branch; they'll be lost unless another branch has them.", true)
                    };
                    let op = GitOp::DeleteBranch { name: name.clone(), force };
                    ws.confirm_git(op, &format!("Delete branch {name}?"), detail, "Delete", !force, window, cx);
                })
                .ok();
            })
            .detach();
            return;
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
            self.confirm_git(op, &message, detail, "Roll Back", false, window, cx);
            return;
        }
        // Untracked files a stash holds that exist again would leave it half
        // applied (Git restores the rest and keeps the stash), so look first.
        if let (&GitOp::Unstash { index, pop }, Some(repo)) = (&op, self.repo.clone()) {
            cx.spawn_in(window, async move |this, cx| {
                let blockers = cx.background_executor().spawn(async move { git::stash_blockers(&repo, index) }).await;
                this.update_in(cx, |ws, window, cx| {
                    if blockers.is_empty() {
                        ws.execute_git(op, window, cx);
                    } else {
                        ws.clear_stash_blockers(blockers, index, pop, window, cx);
                    }
                })
                .ok();
            })
            .detach();
            return;
        }
        self.execute_git(op, window, cx);
    }

    /// Asks before `op`, then runs it.
    #[allow(clippy::too_many_arguments)]
    fn confirm_git(&mut self, op: GitOp, message: &str, detail: &str, button: &str, undoable: bool, window: &mut Window, cx: &mut Context<Self>) {
        let confirmed = ask(window, message, detail, button, undoable, cx);
        cx.spawn_in(window, async move |this, cx| {
            if confirmed.await {
                this.update_in(cx, |ws, window, cx| ws.execute_git(op, window, cx)).ok();
            }
        })
        .detach();
    }

    /// Untracked files of a stash exist in the working tree again: offers to
    /// move them to the Trash so the whole stash applies, instead of Git
    /// applying the rest and keeping the stash.
    fn clear_stash_blockers(&mut self, blockers: Vec<String>, index: usize, pop: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        let verb = if pop { "Pop" } else { "Apply" };
        let (message, detail) = match blockers.as_slice() {
            [one] => (
                format!("“{one}” already exists"),
                format!(
                    "The stash has its own copy, and Git won't replace the file: it would apply the rest of the stash and keep it. Move the existing file to the {}, then {} the stash?",
                    trash_name(),
                    verb.to_lowercase(),
                ),
            ),
            many => (
                format!("{} files from the stash already exist", many.len()),
                format!(
                    "{}\n\nThe stash has its own copies, and Git won't replace the files: it would apply the rest of the stash and keep it. Move the existing files to the {}, then {} the stash?",
                    list_some(many),
                    trash_name(),
                    verb.to_lowercase(),
                ),
            ),
        };
        let confirmed = ask(window, &message, &detail, &format!("Move to {} and {verb}", trash_name()), true, cx);
        cx.spawn_in(window, async move |this, cx| {
            if !confirmed.await {
                return;
            }
            this.update_in(cx, |ws, window, cx| {
                for rel in &blockers {
                    if let Err(err) = crate::file_history::move_to_trash(&repo.join(rel)) {
                        ws.toast(ToastKind::Error, format!("Couldn't move {rel} to the {}", trash_name()), Some(err.to_string()), cx);
                        return;
                    }
                }
                ws.execute_git(GitOp::Unstash { index, pop }, window, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn execute_git(&mut self, op: GitOp, window: &mut Window, cx: &mut Context<Self>) {
        let dir = match (&op, &self.repo, &self.root) {
            (GitOp::Init, _, Some(root)) => root.clone(),
            (_, Some(repo), _) => repo.clone(),
            _ => return,
        };
        if self.git_busy.is_running() {
            self.toast(ToastKind::Error, "Another Git operation is still running", None, cx);
            return;
        }
        // Without a strategy of its own, a pull takes the setting's.
        let op = match op {
            GitOp::Pull(None) => GitOp::Pull(Settings::get(cx).pull_strategy),
            op => op,
        };
        let label = op.busy_label();
        self.git_busy.set(Some(label.into()), |ws: &mut Self| &mut ws.git_busy, cx);
        self.git_panel.update(cx, |p, cx| p.set_busy(Some(label.into()), cx));
        let status = self.git_status.clone();
        let op2 = op.clone();
        self.git_op_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_executor().spawn(async move { perform_git(&dir, op2, &status) }).await;
            this.update_in(cx, |ws, window, cx| {
                ws.git_busy.set(None, |ws: &mut Self| &mut ws.git_busy, cx);
                ws.git_panel.update(cx, |p, cx| p.set_busy(None, cx));
                match result {
                    Ok(Outcome::Done(message)) => {
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
                    Ok(Outcome::Nothing(message)) => ws.toast(ToastKind::Info, message, None, cx),
                    Ok(Outcome::Diverged { branch, upstream, ahead, behind }) => match &op {
                        // Nothing says how to combine them (the setting or the Git config): ask, once.
                        GitOp::Pull(None) => ws.choose_pull_strategy(&branch, &upstream, ahead, behind, window, cx),
                        _ => {
                            let detail = format!(
                                "{branch} has {} that {upstream} doesn't. Merge or rebase it from Branches…, or change the pull strategy in Settings → Git.",
                                commits(ahead),
                            );
                            ws.toast(ToastKind::Error, format!("Can't fast-forward {branch} to {upstream}"), Some(detail), cx);
                        }
                    },
                    Ok(Outcome::PartlyApplied { not_restored, error }) => {
                        // Git applies the changes to tracked files first, then restores
                        // untracked ones, skipping those that exist (or stopping).
                        let what = match not_restored.as_slice() {
                            [] => format!("Tracked files are updated; restoring untracked ones stopped ({error})."),
                            [one] => format!("{one} wasn't restored: it already exists. The rest is applied."),
                            many => format!("{} untracked files weren't restored: they already exist. The rest is applied.", many.len()),
                        };
                        let detail = format!("{what} The stash is kept: don't apply it again, drop it when you're done.");
                        ws.toast(ToastKind::Error, "The stash was only partly applied", Some(detail), cx);
                    }
                    Err(err) => {
                        let title = match &op {
                            GitOp::Push | GitOp::PushNow { .. } | GitOp::Commit { push: true, .. } => "Push failed",
                            GitOp::Pull(_) => "Update failed",
                            GitOp::Fetch => "Fetch failed",
                            GitOp::Commit { .. } => "Commit failed",
                            GitOp::Checkout(_) | GitOp::CreateBranch(_) | GitOp::CheckoutCommit(_) | GitOp::BranchAt { .. } => {
                                "Checkout failed"
                            }
                            GitOp::CherryPick(_) => "Cherry-pick failed",
                            GitOp::Revert(_) => "Revert failed",
                            GitOp::Reset { .. } => "Reset failed",
                            GitOp::Stash { .. } => "Stash failed",
                            GitOp::Unstash { .. } => "Couldn't apply the stash",
                            _ => "Git operation failed",
                        };
                        ws.toast(ToastKind::Error, title, Some(err), cx);
                    }
                }
                if matches!(
                    op,
                    GitOp::Discard(_)
                        | GitOp::Pull(_)
                        | GitOp::Checkout(_)
                        | GitOp::Merge(_)
                        | GitOp::Rebase(_)
                        | GitOp::Abort(_)
                        | GitOp::Continue(_)
                        | GitOp::Stash { .. }
                        | GitOp::Unstash { .. }
                        | GitOp::CheckoutCommit(_)
                        | GitOp::BranchAt { .. }
                        | GitOp::CherryPick(_)
                        | GitOp::Revert(_)
                        | GitOp::Reset { .. }
                ) {
                    ws.reload_changed_files(cx);
                }
                if ws.shows(BottomPanel::GitLog)
                    && matches!(
                        op,
                        GitOp::Commit { .. }
                            | GitOp::Fetch
                            | GitOp::Pull(_)
                            | GitOp::Push
                            | GitOp::PushNow { .. }
                            | GitOp::Checkout(_)
                            | GitOp::CreateBranch(_)
                            | GitOp::Merge(_)
                            | GitOp::Rebase(_)
                            | GitOp::Abort(_)
                            | GitOp::Continue(_)
                            | GitOp::RemoveRemote(_)
                            | GitOp::EditRemote { .. }
                            | GitOp::DeleteBranch { .. }
                            | GitOp::RenameBranch { .. }
                            | GitOp::Stash { .. }
                            | GitOp::CheckoutCommit(_)
                            | GitOp::BranchAt { .. }
                            | GitOp::TagAt { .. }
                            | GitOp::CherryPick(_)
                            | GitOp::Revert(_)
                            | GitOp::Reset { .. }
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
                match &ws.modals.current {
                    Some(Modal::Diff(view)) => view.update(cx, |v, cx| v.reload(cx)),
                    Some(Modal::Remotes(dialog)) => dialog.update(cx, |d, cx| d.reload(cx)),
                    _ => {}
                }
                if op.changes_remotes()
                    && let Some(repo) = &ws.repo
                {
                    ws.git_remote = git::remote_url(repo);
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
        self.toast_with_undo(kind, message, detail, None, cx);
    }

    /// A toast with an Undo button for what it reports.
    pub(super) fn toast_with_undo(
        &mut self,
        kind: ToastKind,
        message: impl Into<SharedString>,
        detail: Option<String>,
        undo: Option<ToastUndo>,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_toast;
        self.next_toast += 1;
        self.toasts.push(Toast { id, kind, message: message.into(), detail: detail.map(Into::into), undo, leaving: false });
        let shown: Vec<usize> = self.toasts.iter().filter(|t| !t.leaving).map(|t| t.id).collect();
        if let Some(&oldest) = shown.first().filter(|_| shown.len() > 4) {
            self.dismiss_toast(oldest, cx);
        }
        // Long enough to reach for its button.
        let seconds = if kind == ToastKind::Error || undo.is_some() { 9 } else { 4 };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(seconds)).await;
            this.update(cx, |ws, cx| ws.dismiss_toast(id, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    /// Fades a toast out, then removes it.
    pub(super) fn dismiss_toast(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(toast) = self.toasts.iter_mut().find(|t| t.id == id && !t.leaving) else { return };
        if Theme::get(cx).reduce_effects {
            self.toasts.retain(|t| t.id != id);
        } else {
            toast.leaving = true;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(transition::TOAST).await;
                this.update(cx, |ws, cx| {
                    ws.toasts.retain(|t| t.id != id);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    pub(super) fn git_push(&mut self, _: &GitPush, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(GitOp::Push, window, cx);
    }

    pub(super) fn git_pull(&mut self, _: &GitPull, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(GitOp::Pull(None), window, cx);
    }

    /// A pull found commits on both sides and nothing that says how to
    /// combine them: asks (like JetBrains' first Update Project), saves the
    /// answer as the pull strategy and pulls with it.
    fn choose_pull_strategy(&mut self, branch: &str, upstream: &str, ahead: u32, behind: u32, window: &mut Window, cx: &mut Context<Self>) {
        let message = format!("{branch} and {upstream} both have new commits");
        let detail = format!(
            "{upstream} has {} that {branch} doesn't, and {branch} has {} of its own. Merge them with a merge commit, or rebase yours on top of the incoming ones?\n\nThe answer is kept as the pull strategy (Settings → Git).",
            commits(behind),
            commits(ahead),
        );
        let answer = window.prompt(PromptLevel::Info, &message, Some(&detail), &["Merge", "Rebase", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            let strategy = match answer.await {
                Ok(0) => git::PullStrategy::Merge,
                Ok(1) => git::PullStrategy::Rebase,
                _ => return,
            };
            this.update_in(cx, |ws, window, cx| {
                let mut problems: Vec<String> = Settings::set("pull_strategy", strategy.key().into())
                    .err()
                    .map(|e| format!("settings.json: {e}"))
                    .into_iter()
                    .collect();
                problems.extend(settings::reload_now(cx));
                if !problems.is_empty() {
                    ws.report_config_problems(problems, cx);
                }
                ws.run_git(GitOp::Pull(Some(strategy)), window, cx);
            })
            .ok();
        })
        .detach();
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
        let subscription = cx.subscribe_in(&picker, window, |ws, _, event: &BranchPickerEvent, window, cx| match event {
            BranchPickerEvent::Run(op) => {
                ws.close_modal(window, cx);
                ws.run_git(op.clone(), window, cx);
            }
            BranchPickerEvent::Rename(name) => ws.ask_for_name(PendingInput::Git(GitPrompt::RenameBranch(name.clone())), window, cx),
            BranchPickerEvent::Dismissed => ws.close_modal(window, cx),
        });
        self.modals.open(Modal::Branches(picker), subscription);
        cx.notify();
    }

    pub(super) fn git_stash(&mut self, _: &GitStash, window: &mut Window, cx: &mut Context<Self>) {
        if self.repo.is_none() {
            self.toast(ToastKind::Error, "Not a Git repository", None, cx);
        } else if self.git_status.files.is_empty() {
            self.toast(ToastKind::Info, "No changes to stash", None, cx);
        } else {
            self.ask_for_name(PendingInput::Git(GitPrompt::Stash), window, cx);
        }
    }

    pub(super) fn manage_remotes(&mut self, _: &ManageRemotes, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            self.toast(ToastKind::Error, "Not a Git repository", None, cx);
            return;
        };
        self.close_modal(window, cx);
        let dialog = cx.new(|cx| RemotesDialog::new(repo, window, cx));
        let subscription = cx.subscribe_in(&dialog, window, |ws, _, event: &RemotesEvent, window, cx| match event {
            // The dialog stays open; `execute_git` reloads it.
            RemotesEvent::Run(op) => ws.run_git(op.clone(), window, cx),
            RemotesEvent::Dismissed => ws.close_modal(window, cx),
        });
        self.modals.open(Modal::Remotes(dialog), subscription);
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
        let subscription = cx.subscribe_in(&dialog, window, |ws, _, event: &CloneDialogEvent, window, cx| match event {
            CloneDialogEvent::Cloned(path) => {
                ws.close_modal(window, cx);
                ws.toast(ToastKind::Success, format!("Cloned {}", folder_name(path)), Some(clone_dialog::display_path(path)), cx);
                ws.open_project(path.clone(), OpenIn::Auto, window, cx);
            }
            CloneDialogEvent::Dismissed => ws.close_modal(window, cx),
        });
        self.modals.open(Modal::Clone(dialog), subscription);
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
        let subscription = cx.subscribe_in(&dialog, window, |ws, dialog, event: &PushDialogEvent, window, cx| match event {
            PushDialogEvent::Push { force, remote } => {
                ws.close_modal(window, cx);
                ws.run_git(GitOp::PushNow { force: *force, remote: remote.clone() }, window, cx);
            }
            PushDialogEvent::OpenDiff(source) => {
                ws.diff_return = Some(dialog.clone());
                ws.show_diff(source.clone(), window, cx);
            }
            PushDialogEvent::ManageRemotes => ws.manage_remotes(&ManageRemotes, window, cx),
            PushDialogEvent::Dismissed => ws.close_modal(window, cx),
        });
        dialog.read(cx).focus(window);
        self.modals.open(Modal::Push(dialog), subscription);
        cx.notify();
    }

    pub(super) fn show_diff(&mut self, source: DiffSource, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        self.close_modal(window, cx);
        let side_by_side = SessionState::load().diff_side_by_side.unwrap_or(true);
        let view = cx.new(|cx| DiffView::new(repo, source, side_by_side, window, cx));
        let subscription = cx.subscribe_in(&view, window, |ws, _, event: &DiffViewEvent, window, cx| match event {
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
        });
        self.modals.open(Modal::Diff(view), subscription);
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



/// What to ask before an operation that loses something: message, detail,
/// button, and whether it can be undone (else Cancel is the default).
fn confirmation(op: &GitOp) -> Option<(String, &'static str, &'static str, bool)> {
    match op {
        GitOp::RemoveRemote(name) => Some((
            format!("Remove remote {name}?"),
            "Its remote-tracking branches go too; the remote repository itself isn't touched.",
            "Remove",
            true,
        )),
        GitOp::DropStash(index) => Some((format!("Drop stash@{{{index}}}?"), "Its changes will be lost.", "Drop", false)),
        GitOp::Reset { sha, mode: git::ResetMode::Hard } => Some((
            format!("Hard reset to {}?", &sha[..sha.len().min(7)]),
            "Commits after it leave the branch, and every uncommitted change is lost. This can't be undone.",
            "Reset",
            false,
        )),
        _ => None,
    }
}

/// How a Git operation ended, for the workspace to report.
pub(super) enum Outcome {
    /// Finished; the message for a notification, if any.
    Done(Option<String>),
    /// Nothing to do, and why.
    Nothing(String),
    /// A fast-forward-only pull found commits on both sides; nothing changed.
    Diverged { branch: String, upstream: String, ahead: u32, behind: u32 },
    /// A stash applied in part and kept (see `git::Unstashed::Partly`).
    PartlyApplied { not_restored: Vec<String>, error: String },
}

/// "1 commit", "3 commits".
fn commits(n: impl Into<u64>) -> String {
    let n = n.into();
    format!("{n} commit{}", if n == 1 { "" } else { "s" })
}

/// Up to a few names, one per line, and how many more there are.
fn list_some(names: &[String]) -> String {
    const SHOWN: usize = 6;
    let mut list = names.iter().take(SHOWN).cloned().collect::<Vec<_>>().join("\n");
    if names.len() > SHOWN {
        list.push_str(&format!("\nand {} more", names.len() - SHOWN));
    }
    list
}

/// Runs one Git operation; blocking.
pub(super) fn perform_git(dir: &Path, op: GitOp, status: &RepoStatus) -> Result<Outcome, String> {
    let push = |dir: &Path, force: bool, remote: Option<String>| -> Result<String, String> {
        let Some(remote) = remote.or_else(|| git::default_remote(dir)) else {
            return Err("No remote is configured. Add one in Git → Manage Remotes….".into());
        };
        let current = git::status(dir).ok();
        let branch = current.as_ref().and_then(|s| s.branch.clone()).or_else(|| status.branch.clone());
        let has_upstream = current.and_then(|s| s.upstream).is_some();
        let verb = if force { "Force-pushed" } else { "Pushed" };
        match (has_upstream, branch) {
            (false, Some(branch)) => git::push(dir, Some((&remote, &branch)), force).map(|_| format!("{verb} {branch} to {remote}")),
            _ => git::push(dir, None, force).map(|_| verb.to_string()),
        }
    };
    let short = |sha: &str| sha[..sha.len().min(7)].to_string();
    let stopped = |out: &str, what: &str, done: String| Some(if out == "CONFLICT" { format!("{what} stopped on conflicts") } else { done });
    let message = match op {
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
        GitOp::Commit { message, amend, all, untracked, push: and_push } => {
            // Untracked files only when checked: a commit of "everything"
            // mustn't pick up build output or notes nobody chose.
            if all {
                git::stage_tracked(dir)?;
            }
            if !untracked.is_empty() {
                git::stage(dir, &untracked)?;
            }
            let summary = git::commit(dir, &message, amend)?;
            if and_push {
                push(dir, false, None)?;
                Ok(Some(format!("Committed and pushed {summary}")))
            } else {
                Ok(Some(format!("Committed {summary}")))
            }
        }
        // `Push` opens the dialog before it gets here; treat it as a plain push.
        GitOp::Push => push(dir, false, None).map(Some),
        GitOp::PushNow { force, remote } => push(dir, force, remote).map(Some),
        GitOp::Pull(strategy) => {
            return git::pull(dir, strategy).map(|pulled| match pulled {
                git::Pulled::UpToDate => Outcome::Done(Some("Already up to date".into())),
                git::Pulled::FastForwarded(n) => Outcome::Done(Some(format!("Pulled {}", commits(n as u64)))),
                git::Pulled::Merged(n) => Outcome::Done(Some(format!("Pulled {} and merged", commits(n as u64)))),
                git::Pulled::Rebased(n) => Outcome::Done(Some(format!("Pulled {}, yours rebased on top", commits(n as u64)))),
                git::Pulled::Conflicts => Outcome::Done(Some("Update stopped on conflicts".into())),
                git::Pulled::Diverged { branch, upstream, ahead, behind } => Outcome::Diverged { branch, upstream, ahead, behind },
            });
        }
        GitOp::Fetch => git::fetch(dir).map(|_| Some("Fetched all remotes".into())),
        GitOp::Checkout(branch) => git::checkout(dir, &branch).map(|_| Some(format!("Switched to {}", branch.name))),
        GitOp::CreateBranch(name) => git::create_branch(dir, &name).map(|_| Some(format!("Created and switched to {name}"))),
        GitOp::Init => git::init(dir).map(|_| Some("Initialized a Git repository".into())),
        GitOp::AddRemote { name, url } => git::add_remote(dir, &name, &url).map(|_| Some(format!("Added remote {name}"))),
        GitOp::RemoveRemote(name) => git::remove_remote(dir, &name).map(|_| Some(format!("Removed remote {name}"))),
        GitOp::EditRemote { name, new_name, url } => {
            git::set_remote_url(dir, &name, &url)?;
            if new_name != name {
                git::rename_remote(dir, &name, &new_name)?;
            }
            Ok(Some(format!("Updated remote {new_name}")))
        }
        GitOp::DeleteBranch { name, force } => git::delete_branch(dir, &name, force).map(|_| Some(format!("Deleted branch {name}"))),
        GitOp::RenameBranch { from, to } => git::rename_branch(dir, &from, &to).map(|_| Some(format!("Renamed {from} to {to}"))),
        GitOp::Stash { message, untracked } => {
            return git::stash(dir, message.as_deref(), untracked).map(|stashed| match stashed {
                true => Outcome::Done(Some("Stashed changes".into())),
                false if untracked => Outcome::Nothing("No changes to stash".into()),
                false => Outcome::Nothing("No changes to stash besides untracked files".into()),
            });
        }
        GitOp::Unstash { index, pop } => {
            return git::unstash(dir, index, pop).map(|unstashed| match unstashed {
                git::Unstashed::Applied => Outcome::Done(Some(if pop { "Popped stash".into() } else { "Applied stash".into() })),
                // Git keeps a stash that didn't apply cleanly.
                git::Unstashed::Conflicts => Outcome::Done(Some("Applying the stash stopped on conflicts; it's kept".into())),
                git::Unstashed::Partly { error, not_restored } => Outcome::PartlyApplied { not_restored, error },
            });
        }
        GitOp::DropStash(index) => git::drop_stash(dir, index).map(|_| Some("Dropped stash".into())),
        GitOp::CheckoutCommit(sha) => git::checkout_commit(dir, &sha).map(|_| Some(format!("Checked out {} (detached HEAD)", short(&sha)))),
        GitOp::BranchAt { name, sha } => git::create_branch_at(dir, &name, &sha).map(|_| Some(format!("Created and switched to {name}"))),
        GitOp::TagAt { name, sha } => git::create_tag(dir, &name, &sha).map(|_| Some(format!("Tagged {} as {name}", short(&sha)))),
        GitOp::CherryPick(sha) => git::cherry_pick(dir, &sha).map(|out| stopped(&out, "Cherry-pick", format!("Cherry-picked {}", short(&sha)))),
        GitOp::Revert(sha) => git::revert(dir, &sha).map(|out| stopped(&out, "Revert", format!("Reverted {}", short(&sha)))),
        GitOp::Reset { sha, mode } => git::reset(dir, &sha, mode).map(|_| Some(format!("Reset to {}", short(&sha)))),
    };
    message.map(Outcome::Done)
}
