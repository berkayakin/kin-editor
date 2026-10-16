//! Manage Remotes: the repository's remotes with their URLs; add one, edit
//! one's name and URL, or remove it. Changes run as `GitOp`s in the
//! workspace, which keeps the dialog open and reloads it afterwards.

use std::path::PathBuf;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, Render, ScrollHandle, SharedString,
    Subscription, Window, div, prelude::*, px,
};

use crate::{
    assets::Icon,
    editor::{Cancel, Editor, EditorEvent, Indent, Newline, Outdent},
    git::{self, Remote},
    git_panel::GitOp,
    theme::Theme,
    ui::{HoverFill, icon_el, input_box, modal_panel, row_pill, text_button, tooltip_view},
};

pub enum RemotesEvent {
    Run(GitOp),
    Dismissed,
}

pub struct RemotesDialog {
    repo: PathBuf,
    remotes: Option<Result<Vec<Remote>, String>>,
    /// The remote the form edits; None: the form adds one.
    editing: Option<String>,
    name: Entity<Editor>,
    url: Entity<Editor>,
    error: Option<String>,
    /// What the form last sent, cleared once the list shows it.
    sent: Option<Remote>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<RemotesEvent> for RemotesDialog {}

impl RemotesDialog {
    pub fn new(repo: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| Editor::single_line("Name", cx));
        let url = cx.new(|cx| Editor::single_line("https://github.com/owner/repo.git", cx));
        window.focus(&name.focus_handle(cx));
        let on_edit = |this: &mut Self, _: Entity<Editor>, _: &EditorEvent, cx: &mut Context<Self>| {
            if this.error.take().is_some() {
                cx.notify();
            }
        };
        let subscriptions = vec![cx.subscribe(&name, on_edit), cx.subscribe(&url, on_edit)];
        let mut this = Self {
            repo,
            remotes: None,
            editing: None,
            name,
            url,
            error: None,
            sent: None,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    /// Re-reads the remotes (after an operation ran); the form empties once
    /// what it sent is there.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        cx.spawn(async move |this, cx| {
            let remotes = cx.background_executor().spawn(async move { git::remotes(&repo) }).await;
            this.update(cx, |this, cx| {
                let first = this.remotes.is_none();
                if let (Some(sent), Ok(list)) = (&this.sent, &remotes)
                    && list.contains(sent)
                {
                    this.sent = None;
                    this.editing = None;
                    this.name.update(cx, |e, cx| e.set_text("", cx));
                    this.url.update(cx, |e, cx| e.set_text("", cx));
                }
                // A repository without remotes usually wants an "origin".
                if first && remotes.as_ref().is_ok_and(|r| r.is_empty()) {
                    this.name.update(cx, |e, cx| e.set_text("origin", cx));
                }
                this.remotes = Some(remotes);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn edit(&mut self, remote: &Remote, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = Some(remote.name.clone());
        self.error = None;
        let (name, url) = (remote.name.clone(), remote.url.clone());
        self.name.update(cx, |e, cx| e.set_text(&name, cx));
        self.url.update(cx, |e, cx| {
            e.set_text(&url, cx);
            e.select_all_text(cx);
        });
        window.focus(&self.url.focus_handle(cx));
        cx.notify();
    }

    fn stop_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        self.error = None;
        self.name.update(cx, |e, cx| e.set_text("", cx));
        self.url.update(cx, |e, cx| e.set_text("", cx));
        window.focus(&self.name.focus_handle(cx));
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text(cx).trim().to_string();
        let url = self.url.read(cx).text(cx).trim().to_string();
        let existing: Vec<String> = match &self.remotes {
            Some(Ok(list)) => list.iter().map(|r| r.name.clone()).collect(),
            _ => Vec::new(),
        };
        let taken = existing.contains(&name) && self.editing.as_ref() != Some(&name);
        self.error = if name.is_empty() {
            Some("Enter a name for the remote".into())
        } else if name.contains(char::is_whitespace) {
            Some("A remote's name can't contain spaces".into())
        } else if taken {
            Some(format!("There's already a remote named {name}"))
        } else if url.is_empty() {
            Some("Enter the repository's URL".into())
        } else {
            None
        };
        if self.error.is_some() {
            cx.notify();
            return;
        }
        let op = match self.editing.clone() {
            Some(old) => GitOp::EditRemote { name: old, new_name: name.clone(), url: url.clone() },
            None => GitOp::AddRemote { name: name.clone(), url: url.clone() },
        };
        self.sent = Some(Remote { name, url });
        cx.emit(RemotesEvent::Run(op));
        cx.notify();
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.submit(cx);
    }

    fn on_escape(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.stop_editing(window, cx);
        } else {
            cx.emit(RemotesEvent::Dismissed);
        }
    }

    fn on_tab(&mut self, _: &Indent, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_field(window, cx);
    }

    fn on_shift_tab(&mut self, _: &Outdent, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_field(window, cx);
    }

    fn switch_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = if self.name.focus_handle(cx).is_focused(window) { &self.url } else { &self.name };
        window.focus(&target.focus_handle(cx));
    }

    fn render_list(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let remotes = match &self.remotes {
            None => return div().p_3().text_color(theme.text_dim).child("Loading remotes…").into_any_element(),
            Some(Err(e)) => return div().p_3().text_color(theme.error).child(e.clone()).into_any_element(),
            Some(Ok(list)) if list.is_empty() => {
                return div()
                    .p_3()
                    .text_color(theme.text_dim)
                    .child("No remotes yet. Add the repository you push to; it's usually named origin.")
                    .into_any_element();
            }
            Some(Ok(list)) => list.clone(),
        };
        let rows = remotes.into_iter().enumerate().map(|(ix, remote)| {
            let editing = self.editing.as_ref() == Some(&remote.name);
            let group = SharedString::from(format!("remote-{ix}"));
            let (edit_remote, remove) = (remote.clone(), remote.name.clone());
            div().w_full().px_1().child(
                div()
                    .id(("remote", ix))
                    .group(group.clone())
                    .relative()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(34.))
                    .px_2p5()
                    .rounded(px(8.))
                    .when(editing, |el| el.bg(theme.accent_soft))
                    .row_hover(theme.hover)
                    .child(icon_el(Icon::Cloud, px(14.), if editing { theme.accent.into() } else { theme.text_muted.into() }))
                    .child(div().flex_shrink_0().font_weight(FontWeight::MEDIUM).text_color(theme.text).child(remote.name.clone()))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(theme.text_dim).child(remote.url.clone()))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .invisible()
                            .group_hover(group, |s| s.visible())
                            .child(
                                row_pill(("remote-edit", ix), "Edit", false, cx)
                                    .tooltip(|_, cx| tooltip_view("Change Name or URL", cx))
                                    .on_click(cx.listener(move |this, _, window, cx| this.edit(&edit_remote, window, cx))),
                            )
                            .child(
                                row_pill(("remote-remove", ix), "Remove", true, cx)
                                    .tooltip(|_, cx| tooltip_view("Remove Remote", cx))
                                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(RemotesEvent::Run(GitOp::RemoveRemote(remove.clone()))))),
                            ),
                    ),
            )
        });
        div()
            .relative()
            .child(
                div()
                    .id("remotes-list")
                    .max_h(px(34. * 6. + 8.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .py_1()
                    .children(rows),
            )
            .child(crate::ui::scrollbar("remotes-scrollbar", &self.scroll, cx))
            .into_any_element()
    }
}

impl Focusable for RemotesDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for RemotesDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let form_title: SharedString = match &self.editing {
            Some(name) => format!("Edit {name}").into(),
            None => "Add Remote".into(),
        };
        let editing = self.editing.is_some();
        modal_panel(cx)
            .key_context("RemotesDialog")
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_shift_tab))
            .w(px(600.))
            .max_w(window.viewport_size().width - px(48.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .px_3p5()
                    .pt_3()
                    .pb_2()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(icon_el(Icon::Cloud, px(13.), theme.text_muted.into()))
                    .child("Remotes"),
            )
            .child(crate::ui::island(&theme).p_0().mx(px(crate::ui::GAP)).child(self.render_list(&theme, cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .px_2p5()
                    .pt_3()
                    .pb_2p5()
                    .child(
                        div()
                            .px_1()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(form_title),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex().w(px(150.)).flex_shrink_0().child(input_box(&self.name, None, window, cx)))
                            .child(input_box(&self.url, Some(Icon::Link), window, cx))
                            .when(editing, |el| {
                                el.child(
                                    text_button("remote-cancel", "Cancel", false, cx)
                                        .h(px(30.))
                                        .on_click(cx.listener(|this, _, window, cx| this.stop_editing(window, cx))),
                                )
                            })
                            .child(
                                text_button("remote-submit", if editing { "Save" } else { "Add" }, true, cx)
                                    .h(px(30.))
                                    .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                            ),
                    )
                    .child(
                        div()
                            .min_h(px(16.))
                            .px_1()
                            .text_size(px(12.))
                            .when_some(self.error.clone(), |el, e| el.text_color(theme.error).child(e))
                            .when(self.error.is_none(), |el| {
                                el.text_color(theme.text_dim).child("HTTPS or SSH URL · Tab switches fields · Enter saves")
                            }),
                    ),
            )
    }
}
