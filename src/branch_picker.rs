//! Branch switcher: check out local or remote branches, create one, or
//! merge / rebase with another.

use std::{ops::Range, path::PathBuf, sync::Arc};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, Render,
    ScrollStrategy, SharedString, Subscription, UniformListScrollHandle, Window, div, prelude::*, px,
    uniform_list,
};

use crate::{
    assets::Icon,
    editor::{Cancel, Editor, EditorEvent, MoveDown, MoveUp, Newline},
    git::{self, Branch},
    git_panel::GitOp,
    theme::Theme,
    ui::{icon_el, input_box, modal_panel},
};

pub enum BranchPickerEvent {
    Run(GitOp),
    Dismissed,
}

#[derive(Clone)]
enum Item {
    Create(String),
    Branch(Branch),
}

pub struct BranchPicker {
    query: Entity<Editor>,
    branches: Option<Arc<Vec<Branch>>>,
    error: Option<String>,
    items: Arc<Vec<Item>>,
    selected: usize,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BranchPickerEvent> for BranchPicker {}

impl BranchPicker {
    pub fn new(repo: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| Editor::single_line("Search or create a branch", cx));
        window.focus(&query.focus_handle(cx));
        let subscription = cx.subscribe(&query, |this, _, _: &EditorEvent, cx| this.update_items(cx));
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { git::branches(&repo) }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(branches) => this.branches = Some(Arc::new(branches)),
                    Err(e) => this.error = Some(e),
                }
                this.update_items(cx);
            })
            .ok();
        })
        .detach();
        Self {
            query,
            branches: None,
            error: None,
            items: Arc::new(Vec::new()),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: vec![subscription],
        }
    }

    fn update_items(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text().trim().to_string();
        let needle = query.to_lowercase();
        let mut items = Vec::new();
        let branches = self.branches.clone().unwrap_or_default();
        let exists = branches.iter().any(|b| !b.remote && b.name == query);
        if !query.is_empty() && !exists {
            items.push(Item::Create(query.replace(char::is_whitespace, "-")));
        }
        // Current branch first, then local, then remote.
        let mut sorted: Vec<&Branch> = branches.iter().filter(|b| b.name.to_lowercase().contains(&needle)).collect();
        sorted.sort_by_key(|b| (!b.current, b.remote));
        items.extend(sorted.into_iter().cloned().map(Item::Branch));
        self.items = Arc::new(items);
        self.selected = 0;
        cx.notify();
    }

    fn confirm(&mut self, ix: usize, cx: &mut Context<Self>) {
        match self.items.get(ix) {
            Some(Item::Create(name)) => cx.emit(BranchPickerEvent::Run(GitOp::CreateBranch(name.clone()))),
            Some(Item::Branch(b)) if b.current => cx.emit(BranchPickerEvent::Dismissed),
            Some(Item::Branch(b)) => cx.emit(BranchPickerEvent::Run(GitOp::Checkout(b.clone()))),
            None => {}
        }
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if !self.items.is_empty() {
            self.selected = ix.min(self.items.len() - 1);
            self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
        }
        cx.notify();
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.saturating_sub(1), cx);
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected + 1, cx);
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.confirm(self.selected, cx);
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(BranchPickerEvent::Dismissed);
    }
}

impl Focusable for BranchPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for BranchPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let items = self.items.clone();
        let selected = self.selected;
        let count = items.len();
        let list = uniform_list("branches", count, {
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                let theme = Theme::get(cx).clone();
                range
                    .map(|ix| {
                        let is_selected = ix == selected;
                        let row = div()
                            .id(ix)
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(30.))
                            .px_2()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .when(is_selected, |el| el.bg(theme.accent_soft))
                            .when(!is_selected, |el| el.hover(|s| s.bg(theme.hover)))
                            .on_click(cx.listener(move |this, _, _, cx| this.confirm(ix, cx)));
                        let row = match &items[ix] {
                            Item::Create(name) => row
                                .child(icon_el(Icon::Plus, px(14.), theme.accent.into()))
                                .child(div().text_color(theme.text).child(format!("Create branch “{name}”"))),
                            Item::Branch(b) => row
                                .group(SharedString::from(format!("branch-{ix}")))
                                .child(icon_el(
                                    if b.current { Icon::Check } else { Icon::GitBranch },
                                    px(14.),
                                    if b.current { theme.accent.into() } else { theme.text_muted.into() },
                                ))
                                .child(div().flex_1().truncate().text_color(theme.text).child(b.name.clone()))
                                .when(b.remote, |el| {
                                    el.child(div().text_size(px(11.)).text_color(theme.text_dim).child("remote"))
                                })
                                .when(b.current, |el| {
                                    el.child(div().text_size(px(11.)).text_color(theme.text_dim).child("current"))
                                })
                                // Merge into / rebase onto, on hover (or for the selected row).
                                .when(!b.current, |el| {
                                    let group = SharedString::from(format!("branch-{ix}"));
                                    let action = |id: &'static str, label: &'static str, tip: &'static str, op: GitOp| {
                                        div()
                                            .id((id, ix))
                                            .px_1p5()
                                            .h(px(20.))
                                            .flex()
                                            .items_center()
                                            .rounded(px(4.))
                                            .text_size(px(11.5))
                                            .text_color(theme.text_muted)
                                            .bg(theme.elevated)
                                            .border_1()
                                            .border_color(theme.border)
                                            .hover(|s| s.text_color(theme.text).bg(theme.hover))
                                            .tooltip(move |_, cx| crate::ui::tooltip_view(tip, cx))
                                            .on_click(cx.listener(move |_, _, _, cx| {
                                                cx.stop_propagation();
                                                cx.emit(BranchPickerEvent::Run(op.clone()));
                                            }))
                                            .child(label)
                                    };
                                    el.child(
                                        div()
                                            .flex()
                                            .gap_1()
                                            .when(!is_selected, |el| el.invisible().group_hover(group, |s| s.visible()))
                                            .child(action("merge", "Merge", "Merge into Current Branch", GitOp::Merge(b.name.clone())))
                                            .child(action("rebase", "Rebase", "Rebase Current Branch onto This", GitOp::Rebase(b.name.clone()))),
                                    )
                                }),
                        };
                        div().w_full().px_1p5().child(row)
                    })
                    .collect()
            })
        })
        .track_scroll(self.scroll.clone())
        .h(px((count.clamp(1, 12) * 30) as f32))
        .w_full();
        // Padding goes on a wrapper: GPUI's scroll-to-item counts a list's own
        // padding twice, so a list that fits its rows would scroll by 8px.
        let list = div().py_1().child(list);

        modal_panel(cx)
            .key_context("BranchPicker")
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_escape))
            .w(px(520.))
            .max_w(window.viewport_size().width - px(48.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .px_3()
                    .pt_2p5()
                    .pb_1p5()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(icon_el(Icon::GitBranch, px(13.), theme.text_muted.into()))
                    .child("Branches"),
            )
            .child(div().px_3().pb_2().child(input_box(&self.query, None, window, cx)))
            .child(div().border_t_1().border_color(theme.border).child(match (&self.branches, &self.error) {
                (_, Some(e)) => div().p_3().text_color(theme.error).child(e.clone()).into_any_element(),
                (None, _) => div().p_3().text_color(theme.text_dim).child("Loading branches…").into_any_element(),
                _ if count == 0 => div().p_3().text_color(theme.text_dim).child("No branches").into_any_element(),
                _ => list.into_any_element(),
            }))
    }
}
