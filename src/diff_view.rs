//! Unified diff of one changed file, opened from the Commit panel.

use std::{ops::Range, path::PathBuf, sync::Arc};

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, IntoElement, Render,
    SharedString, UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use crate::{
    assets::Icon,
    editor::Cancel,
    git::{self, Change, DiffLine, DiffLineKind, FileStatus},
    git_panel::{GitOp, change_color, file_icon_for},
    theme::Theme,
    ui::{icon_el, modal_panel, text_button, tooltip_view},
};

pub enum DiffViewEvent {
    Close,
    OpenFile(PathBuf),
    Run(GitOp),
}

pub struct DiffView {
    file: FileStatus,
    staged: bool,
    lines: Option<Arc<Vec<DiffLine>>>,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
}

impl EventEmitter<DiffViewEvent> for DiffView {}

impl DiffView {
    /// `buffer` is the open editor's text, so unsaved edits are included.
    pub fn new(
        repo: PathBuf,
        file: FileStatus,
        staged: bool,
        buffer: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        let f = file.clone();
        cx.spawn(async move |this, cx| {
            let lines = cx
                .background_executor()
                .spawn(async move {
                    // Staged: HEAD → index. Unstaged: index → working tree.
                    let (base, current) = if staged {
                        (git::head_text(&repo, &f.rel).unwrap_or_default(), git::index_text(&repo, &f.rel).unwrap_or_default())
                    } else {
                        let base = if f.unstaged == Some(Change::Untracked) {
                            String::new()
                        } else {
                            git::index_text(&repo, &f.rel).unwrap_or_default()
                        };
                        let current = buffer.or_else(|| std::fs::read_to_string(&f.path).ok()).unwrap_or_default();
                        (base, current)
                    };
                    git::unified_lines(&base, &current, 3)
                })
                .await;
            this.update(cx, |this, cx| {
                this.lines = Some(Arc::new(lines));
                cx.notify();
            })
            .ok();
        })
        .detach();
        Self {
            file,
            staged,
            lines: None,
            focus_handle,
            scroll: UniformListScrollHandle::new(),
        }
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DiffViewEvent::Close);
    }
}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let change = if self.staged { self.file.staged } else { self.file.unstaged }.unwrap_or(Change::Modified);
        let (icon, icon_color) = file_icon_for(&self.file.path, &theme);
        let (added, removed) = self.lines.as_ref().map_or((0, 0), |l| {
            (
                l.iter().filter(|d| d.kind == DiffLineKind::Added).count(),
                l.iter().filter(|d| d.kind == DiffLineKind::Removed).count(),
            )
        });
        let rel = self.file.rel.clone();
        let deleted = change == Change::Deleted;

        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(46.))
            .border_b_1()
            .border_color(theme.border)
            .child(icon_el(Icon::GitCompare, px(15.), theme.text_muted.into()))
            .child(icon_el(icon, px(15.), icon_color))
            .child(div().font_weight(FontWeight::SEMIBOLD).truncate().child(rel.clone()))
            .child(
                div()
                    .px_1p5()
                    .rounded(px(4.))
                    .bg(Hsla::from(change_color(&theme, change)).alpha(0.15))
                    .text_size(px(11.))
                    .text_color(change_color(&theme, change))
                    .child(format!("{} · {}", change.label(), if self.staged { "Staged" } else { "Unstaged" })),
            )
            .child(div().text_size(px(12.)).text_color(theme.git_added).child(format!("+{added}")))
            .child(div().text_size(px(12.)).text_color(theme.git_deleted).child(format!("−{removed}")))
            .child(div().flex_1())
            .when(!self.staged, |el| {
                let file = self.file.clone();
                el.child(
                    text_button("diff-rollback", "Rollback", false, cx)
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(DiffViewEvent::Run(GitOp::Discard(vec![file.clone()]))))),
                )
            })
            .child({
                let staged = self.staged;
                let rel = rel.clone();
                text_button("diff-stage", if staged { "Unstage" } else { "Stage" }, true, cx).on_click(cx.listener(move |_, _, _, cx| {
                    let op = if staged { GitOp::Unstage(vec![rel.clone()]) } else { GitOp::Stage(vec![rel.clone()]) };
                    cx.emit(DiffViewEvent::Run(op));
                }))
            })
            .when(!deleted, |el| {
                let path = self.file.path.clone();
                el.child(
                    div()
                        .id("diff-open")
                        .size(px(28.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.hover))
                        .tooltip(|_, cx| tooltip_view("Open File", cx))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(DiffViewEvent::OpenFile(path.clone()))))
                        .child(icon_el(Icon::File, px(15.), theme.text_muted.into())),
                )
            })
            .child(
                div()
                    .id("diff-close")
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.hover))
                    .tooltip(|_, cx| tooltip_view("Close (Esc)", cx))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DiffViewEvent::Close)))
                    .child(icon_el(Icon::X, px(15.), theme.text_muted.into())),
            );

        let body = match self.lines.clone() {
            None => div().flex_1().p_4().text_color(theme.text_dim).child("Loading diff…").into_any_element(),
            Some(lines) if lines.is_empty() => div().flex_1().p_4().text_color(theme.text_dim).child("No differences").into_any_element(),
            Some(lines) => uniform_list("diff-lines", lines.len(), {
                cx.processor(move |_this, range: Range<usize>, _window, cx| {
                    let theme = Theme::get(cx).clone();
                    range
                        .map(|ix| {
                            let line = &lines[ix];
                            let number = |n: Option<usize>| {
                                div()
                                    .w(px(44.))
                                    .pr_2()
                                    .flex_shrink_0()
                                    .flex()
                                    .justify_end()
                                    .text_color(theme.line_number)
                                    .child(n.map(|n| n.to_string()).unwrap_or_default())
                            };
                            let (bg, sign, sign_color): (Option<Hsla>, &str, Hsla) = match line.kind {
                                DiffLineKind::Added => (Some(Hsla::from(theme.git_added).alpha(0.12)), "+", theme.git_added.into()),
                                DiffLineKind::Removed => (Some(Hsla::from(theme.git_deleted).alpha(0.12)), "−", theme.git_deleted.into()),
                                DiffLineKind::Context => (None, " ", theme.text_dim.into()),
                                DiffLineKind::Skipped => (Some(theme.panel.into()), "", theme.text_dim.into()),
                            };
                            let row = div().id(ix).w_full().flex().h(px(20.)).when_some(bg, |el, bg| el.bg(bg));
                            if line.kind == DiffLineKind::Skipped {
                                row.justify_center()
                                    .items_center()
                                    .text_color(theme.text_dim)
                                    .font_family(theme.ui_font.clone())
                                    .text_size(px(11.5))
                                    .child(format!("⋯ {}", line.text))
                            } else {
                                let text: SharedString = line.text.replace('\t', "    ").into();
                                row.child(number(line.old_no))
                                    .child(number(line.new_no))
                                    .child(div().w(px(16.)).flex_shrink_0().text_color(sign_color).child(sign.to_string()))
                                    .child(
                                        div()
                                            .whitespace_nowrap()
                                            .text_color(if line.kind == DiffLineKind::Context { theme.text_muted } else { theme.text })
                                            .child(text),
                                    )
                            }
                        })
                        .collect()
                })
            })
            .track_scroll(self.scroll.clone())
            .flex_1()
            .w_full()
            .py_1()
            .bg(theme.background)
            .font_family(theme.mono_font.clone())
            .text_size(px(theme.editor_font_size - 1.))
            .into_any_element(),
        };

        modal_panel(cx)
            .key_context("DiffView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_escape))
            .w(px(960.))
            .max_w(window.viewport_size().width - px(48.))
            .h(px(680.))
            .max_h(window.viewport_size().height - px(110.))
            .child(header)
            .child(body)
    }
}
