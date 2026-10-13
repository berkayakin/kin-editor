//! The Commit tool window (JetBrains ⌘K / ⌘0): changed files with
//! stage/unstage/discard, a commit message box and push/pull controls.
//! Operations are emitted as events; the workspace runs them.

use std::{ops::Range, path::PathBuf, sync::Arc};

use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    IntoElement, Render, SharedString, Stateful, Window, div, prelude::*, px,
};

use crate::{
    assets::Icon,
    editor::{Editor, Submit},
    git::{self, Branch, Change, FileStatus, RepoState, RepoStatus},
    theme::Theme,
    ui::{icon_el, kbd, tooltip_view},
};

#[derive(Clone, Debug)]
pub enum GitOp {
    Stage(Vec<String>),
    Unstage(Vec<String>),
    /// Stage the changes of `text` (the file's current text) touching `rows`.
    StageLines { rel: String, text: String, rows: Range<usize> },
    /// Unstage the staged changes touching `rows`: of `text` when given (an
    /// editor's rows), else of the index version.
    UnstageLines { rel: String, text: Option<String>, rows: Range<usize> },
    Discard(Vec<FileStatus>),
    Commit { message: String, amend: bool, all: bool, push: bool },
    Push,
    Pull,
    Fetch,
    Checkout(Branch),
    CreateBranch(String),
    Init,
    /// Merge a branch into the current one.
    Merge(String),
    /// Rebase the current branch onto another.
    Rebase(String),
    Abort(RepoState),
    Continue(RepoState),
}

impl GitOp {
    /// Progress text shown while the operation runs.
    pub fn busy_label(&self) -> &'static str {
        match self {
            GitOp::Stage(_) => "Staging…",
            GitOp::Unstage(_) => "Unstaging…",
            GitOp::StageLines { .. } => "Staging lines…",
            GitOp::UnstageLines { .. } => "Unstaging lines…",
            GitOp::Merge(_) => "Merging…",
            GitOp::Rebase(_) => "Rebasing…",
            GitOp::Abort(_) => "Aborting…",
            GitOp::Continue(_) => "Continuing…",
            GitOp::Discard(_) => "Discarding…",
            GitOp::Commit { push: true, .. } => "Committing and pushing…",
            GitOp::Commit { .. } => "Committing…",
            GitOp::Push => "Pushing…",
            GitOp::Pull => "Pulling…",
            GitOp::Fetch => "Fetching…",
            GitOp::Checkout(_) => "Checking out…",
            GitOp::CreateBranch(_) => "Creating branch…",
            GitOp::Init => "Initializing…",
        }
    }
}

pub enum GitPanelEvent {
    Run(GitOp),
    OpenDiff { file: FileStatus, staged: bool },
    OpenFile(PathBuf),
}

pub struct GitPanel {
    message: Entity<Editor>,
    amend: bool,
    /// Working-tree root of the repository, if the project is in one.
    repo: Option<PathBuf>,
    /// The open folder (to offer `git init` when it isn't a repository).
    project: Option<PathBuf>,
    status: Arc<RepoStatus>,
    busy: Option<SharedString>,
    selected: Option<(String, bool)>,
}

impl EventEmitter<GitPanelEvent> for GitPanel {}

pub fn change_color(theme: &Theme, change: Change) -> Hsla {
    match change {
        Change::Added | Change::Renamed => theme.git_added,
        Change::Modified => theme.git_modified,
        Change::Deleted => theme.git_deleted,
        Change::Untracked => theme.git_untracked,
        Change::Conflicted => theme.git_conflict,
    }
    .into()
}


impl GitPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let message = cx.new(|cx| Editor::multi_line("Commit message", cx));
        cx.observe(&message, |_, _, cx| cx.notify()).detach();
        Self {
            message,
            amend: false,
            repo: None,
            project: None,
            status: Arc::new(RepoStatus::default()),
            busy: None,
            selected: None,
        }
    }

    pub fn set_repo(&mut self, repo: Option<PathBuf>, project: Option<PathBuf>, cx: &mut Context<Self>) {
        self.repo = repo;
        self.project = project;
        cx.notify();
    }

    pub fn set_status(&mut self, status: Arc<RepoStatus>, cx: &mut Context<Self>) {
        // A merge that just stopped: start from git's merge message, as JetBrains does.
        let merge_started = status.state == Some(RepoState::Merging) && self.status.state != Some(RepoState::Merging);
        if merge_started
            && self.message_text(cx).is_empty()
            && let Some(repo) = self.repo.clone()
        {
            let message = self.message.clone();
            cx.spawn(async move |_, cx| {
                let text = cx.background_executor().spawn(async move { git::merge_message(&repo) }).await;
                if let Some(text) = text {
                    message.update(cx, |m, cx| m.set_text(&text, cx)).ok();
                }
            })
            .detach();
        }
        self.status = status;
        cx.notify();
    }

    pub fn set_busy(&mut self, busy: Option<SharedString>, cx: &mut Context<Self>) {
        self.busy = busy;
        cx.notify();
    }

    pub fn committed(&mut self, cx: &mut Context<Self>) {
        self.amend = false;
        self.message.update(cx, |m, cx| m.set_text("", cx));
        cx.notify();
    }

    pub fn focus_message(&self, window: &mut Window, cx: &App) {
        window.focus(&self.message.focus_handle(cx));
    }

    fn message_text(&self, cx: &App) -> String {
        self.message.read(cx).text().trim().to_string()
    }

    fn can_commit(&self, cx: &App) -> bool {
        self.busy.is_none()
            && !self.message_text(cx).is_empty()
            && (self.amend || !self.status.files.is_empty())
            // Unresolved conflicts would be committed with their markers.
            && self.status.conflicted().next().is_none()
    }

    fn commit(&mut self, push: bool, cx: &mut Context<Self>) {
        if !self.can_commit(cx) {
            return;
        }
        let all = self.status.staged().next().is_none();
        cx.emit(GitPanelEvent::Run(GitOp::Commit {
            message: self.message_text(cx),
            amend: self.amend,
            all,
            push,
        }));
    }

    fn toggle_amend(&mut self, cx: &mut Context<Self>) {
        self.amend = !self.amend;
        // Like JetBrains, amending starts from the previous message.
        if self.amend
            && self.message_text(cx).is_empty()
            && let Some(repo) = self.repo.clone()
        {
            let message = self.message.clone();
            cx.spawn(async move |_, cx| {
                let text = cx
                    .background_executor()
                    .spawn(async move { git::last_commit_message(&repo) })
                    .await;
                if let Some(text) = text {
                    message.update(cx, |m, cx| m.set_text(&text, cx)).ok();
                }
            })
            .detach();
        }
        cx.notify();
    }

    fn on_submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        self.commit(false, cx);
    }

    fn small_button(&self, id: impl Into<gpui::ElementId>, icon: Icon, tip: &'static str, cx: &App) -> Stateful<gpui::Div> {
        let theme = Theme::get(cx);
        div()
            .id(id)
            .size(px(22.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(px(5.))
            .cursor_pointer()
            .hover(|s| s.bg(theme.active))
            .tooltip(move |_, cx| tooltip_view(tip, cx))
            .child(icon_el(icon, px(14.), theme.text_muted.into()))
    }

    fn render_file(&self, file: &FileStatus, staged: bool, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let change = if staged {
            file.staged.unwrap_or(Change::Modified)
        } else if file.staged == Some(Change::Conflicted) {
            Change::Conflicted
        } else {
            file.unstaged.unwrap_or(Change::Modified)
        };
        let color = change_color(&theme, change);
        let icon = crate::ui::file_icon(&file.path, px(15.), &theme);
        let name = file.rel.rsplit('/').next().unwrap_or(&file.rel).to_string();
        let dir = file.rel.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        let is_selected = self.selected.as_ref().is_some_and(|(r, s)| *r == file.rel && *s == staged);
        let group = SharedString::from(format!("git-row-{}-{ix}", staged as u8));
        let f = file.clone();
        let f_open = file.clone();
        let rel = file.rel.clone();
        let deleted = change == Change::Deleted;
        let conflicted = change == Change::Conflicted;

        div().w_full().px_1p5().child(
            div()
                .id((if staged { "staged" } else { "unstaged" }, ix))
                .group(group.clone())
                .w_full()
                .flex()
                .items_center()
                .gap_1p5()
                .h(px(26.))
                .pl_2()
                .pr_1()
                .rounded(px(6.))
                .cursor_pointer()
                .when(is_selected, |el| el.bg(theme.accent_soft))
                .when(!is_selected, |el| el.hover(|s| s.bg(theme.hover)))
                .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                    this.selected = Some((f.rel.clone(), staged));
                    // A conflict is resolved in the editor, not read in a diff.
                    if (e.click_count() >= 2 || conflicted) && !deleted {
                        cx.emit(GitPanelEvent::OpenFile(f.path.clone()));
                    } else {
                        cx.emit(GitPanelEvent::OpenDiff { file: f.clone(), staged });
                    }
                    cx.notify();
                }))
                .child(icon)
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(color)
                        .when(deleted, |el| el.line_through())
                        .child(name),
                )
                .child(div().flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme.text_dim).child(dir))
                // Row actions appear on hover.
                .child(
                    div()
                        .flex()
                        .invisible()
                        .group_hover(group.clone(), |s| s.visible())
                        .when(!staged && !conflicted, |el| {
                            let discard = f_open.clone();
                            el.child(
                                self.small_button(("discard", ix), Icon::Undo, "Rollback Changes", cx)
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(GitPanelEvent::Run(GitOp::Discard(vec![discard.clone()])));
                                    })),
                            )
                        })
                        .when(!deleted, |el| {
                            let path = f_open.path.clone();
                            el.child(
                                self.small_button(("open", (staged as usize) * 100_000 + ix), Icon::File, "Open File", cx)
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(GitPanelEvent::OpenFile(path.clone()));
                                    })),
                            )
                        })
                        .child({
                            let rel = rel.clone();
                            let (icon, tip) = if staged {
                                (Icon::Minus, "Unstage")
                            } else if conflicted {
                                (Icon::Check, "Mark as Resolved")
                            } else {
                                (Icon::Plus, "Stage")
                            };
                            self.small_button(("stage", (staged as usize) * 100_000 + ix), icon, tip, cx)
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.stop_propagation();
                                    let op = if staged {
                                        GitOp::Unstage(vec![rel.clone()])
                                    } else {
                                        GitOp::Stage(vec![rel.clone()])
                                    };
                                    cx.emit(GitPanelEvent::Run(op));
                                }))
                        }),
                )
                .child(
                    div()
                        .w(px(14.))
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color)
                        .child(change.letter()),
                ),
        )
    }

    fn section_header(&self, id: &'static str, title: &str, count: usize, action: Option<(Icon, &'static str, GitOp)>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        div()
            .flex()
            .items_center()
            .gap_1()
            .h(px(28.))
            .px_3()
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text_dim)
            .child(title.to_uppercase())
            .child(div().px_1p5().rounded_full().bg(theme.active).text_color(theme.text_muted).child(count.to_string()))
            .child(div().flex_1())
            .when_some(action, |el, (icon, tip, op)| {
                el.child(self.small_button(id, icon, tip, cx).on_click(cx.listener(move |_, _, _, cx| {
                    cx.emit(GitPanelEvent::Run(op.clone()));
                })))
            })
    }

    /// A stopped merge or rebase: what's left, and Abort / Continue.
    fn render_state_banner(&self, state: RepoState, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let conflicts = self.status.conflicted().count();
        let busy = self.busy.is_some();
        let detail = match conflicts {
            0 => "conflicts resolved".to_string(),
            1 => "1 conflict".to_string(),
            n => format!("{n} conflicts"),
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .m_2()
            .p_2()
            .rounded(px(7.))
            .bg(Hsla::from(theme.git_conflict).alpha(0.1))
            .border_1()
            .border_color(Hsla::from(theme.git_conflict).alpha(0.35))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(icon_el(Icon::GitBranch, px(14.), theme.git_conflict.into()))
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(state.label()))
                    .child(div().text_color(theme.text_muted).child(format!("· {detail}"))),
            )
            .when(conflicts > 0, |el| {
                el.child(div().text_size(px(12.)).text_color(theme.text_muted).child(
                    "Open each conflicted file, pick a side on its conflicts, then mark it resolved.",
                ))
            })
            .child(
                div()
                    .flex()
                    .gap_1p5()
                    .child(crate::ui::text_button("state-abort", "Abort", false, cx).on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(GitPanelEvent::Run(GitOp::Abort(state)));
                    })))
                    .when(conflicts == 0 && !busy, |el| {
                        el.child(crate::ui::text_button("state-continue", if state == RepoState::Merging { "Commit Merge" } else { "Continue" }, true, cx).on_click(
                            cx.listener(move |this, _, _, cx| {
                                // A merge commit takes the message box's text when there is one.
                                let message = this.message_text(cx);
                                let op = if state == RepoState::Merging && !message.is_empty() {
                                    GitOp::Commit { message, amend: false, all: false, push: false }
                                } else {
                                    GitOp::Continue(state)
                                };
                                cx.emit(GitPanelEvent::Run(op));
                            }),
                        ))
                    }),
            )
    }

    fn render_commit_box(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let focused = self.message.read(cx).focus_handle(cx).is_focused(window);
        let can_commit = self.can_commit(cx);
        let nothing_staged = self.status.staged().next().is_none();
        let label = if self.amend {
            "Amend Commit"
        } else if nothing_staged && !self.status.files.is_empty() {
            "Commit All"
        } else {
            "Commit"
        };
        let commit_button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .h(px(28.))
                .px_3()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(7.))
                .font_weight(FontWeight::MEDIUM)
                .when(!can_commit, |el| el.opacity(0.45))
                .when(can_commit, |el| el.cursor_pointer())
                .when(primary, |el| {
                    el.bg(theme.accent_soft)
                        .text_color(theme.accent)
                        .when(can_commit, |el| el.hover(|s| s.bg(Hsla::from(theme.accent).alpha(0.24))))
                })
                .when(!primary, |el| el.bg(theme.hover).text_color(theme.text).when(can_commit, |el| el.hover(|s| s.bg(theme.active))))
                .child(label)
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .border_t_1()
            .border_color(theme.border_subtle)
            .child(
                div()
                    .h(px(96.))
                    .px_2()
                    .py_1p5()
                    .rounded(px(7.))
                    .bg(theme.background)
                    .border_1()
                    .border_color(if focused { Hsla::from(theme.accent).alpha(0.7) } else { theme.border.into() })
                    .overflow_hidden()
                    .child(self.message.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(
                        div()
                            .id("amend")
                            .flex()
                            .items_center()
                            .gap_1()
                            .cursor_pointer()
                            .text_color(theme.text_muted)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_amend(cx)))
                            .child(icon_el(
                                if self.amend { Icon::SquareCheck } else { Icon::Square },
                                px(15.),
                                if self.amend { theme.accent.into() } else { theme.text_dim.into() },
                            ))
                            .child("Amend"),
                    )
                    .child(div().flex_1())
                    .child(
                        commit_button("commit", label, true)
                            .tooltip(|_, cx| tooltip_view(if cfg!(target_os = "macos") { "Commit (⌘↩)" } else { "Commit (Ctrl+Enter)" }, cx))
                            .on_click(cx.listener(|this, _, _, cx| this.commit(false, cx))),
                    )
                    .child(
                        commit_button("commit-push", "Commit and Push", false)
                            .on_click(cx.listener(|this, _, _, cx| this.commit(true, cx))),
                    ),
            )
    }
}

impl Focusable for GitPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.message.focus_handle(cx)
    }
}

impl Render for GitPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let root = div().key_context("GitPanel").on_action(cx.listener(Self::on_submit)).flex().flex_col().size_full();

        if self.repo.is_none() {
            let can_init = self.project.is_some();
            return root
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_4()
                        .text_color(theme.text_muted)
                        .child(if can_init {
                            "This folder isn't a Git repository."
                        } else {
                            "Open a folder to use Git."
                        })
                        .when(can_init, |el| {
                            el.child(
                                crate::ui::text_button("git-init", "Initialize Repository", true, cx)
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(GitPanelEvent::Run(GitOp::Init)))),
                            )
                        }),
                )
                .into_any_element();
        }

        let staged: Vec<FileStatus> = self.status.staged().cloned().collect();
        let unstaged: Vec<FileStatus> = self.status.unstaged().cloned().collect();
        let unstaged_rels: Vec<String> = unstaged.iter().map(|f| f.rel.clone()).collect();
        let staged_rels: Vec<String> = staged.iter().map(|f| f.rel.clone()).collect();

        let mut list = div().id("git-changes").flex_1().min_h_0().overflow_y_scroll().pb_2();
        if staged.is_empty() && unstaged.is_empty() {
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_3()
                    .text_color(theme.text_dim)
                    .child(icon_el(Icon::CircleCheck, px(15.), theme.success.into()))
                    .child("No changes"),
            );
        } else {
            if !staged.is_empty() {
                list = list
                    .child(self.section_header("unstage-all", "Staged", staged.len(), Some((Icon::Minus, "Unstage All", GitOp::Unstage(staged_rels))), cx))
                    .children(staged.iter().enumerate().map(|(i, f)| self.render_file(f, true, i, cx).into_any_element()));
            }
            if !unstaged.is_empty() {
                list = list
                    .child(self.section_header("stage-all", "Changes", unstaged.len(), Some((Icon::Plus, "Stage All", GitOp::Stage(unstaged_rels))), cx))
                    .children(unstaged.iter().enumerate().map(|(i, f)| self.render_file(f, false, i, cx).into_any_element()));
            }
        }

        root.when_some(self.busy.clone(), |el, busy| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .h(px(26.))
                        .text_size(px(12.))
                        .text_color(theme.text_muted)
                        .bg(theme.accent_soft)
                        .child(icon_el(Icon::Refresh, px(12.), theme.accent.into()))
                        .child(busy),
                )
            })
            .when_some(self.status.state, |el, state| el.child(self.render_state_banner(state, cx)))
            .child(list)
            .child(self.render_commit_box(window, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_3()
                    .pb_2()
                    .text_size(px(11.))
                    .text_color(theme.text_dim)
                    .child(kbd(if cfg!(target_os = "macos") { "⌘↩" } else { "Ctrl+Enter" }, cx))
                    .child("to commit"),
            )
            .into_any_element()
    }
}
