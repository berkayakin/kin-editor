//! A right-click menu: icons, shortcut hints, separators and section
//! headers, navigable with the keyboard. Emits the chosen item's index.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, IntoElement, Render,
    SharedString, Window, div, prelude::*, px,
};

use crate::{
    assets::Icon,
    editor::{Cancel, MoveDown, MoveUp, Newline},
    theme::Theme,
    ui::{HoverFill, icon_el},
};

pub enum MenuEntry {
    Item {
        label: SharedString,
        icon: Option<Icon>,
        shortcut: Option<SharedString>,
        danger: bool,
    },
    Header(SharedString),
    Separator,
}

impl MenuEntry {
    pub fn item(label: impl Into<SharedString>, icon: Icon) -> Self {
        MenuEntry::Item { label: label.into(), icon: Some(icon), shortcut: None, danger: false }
    }

    /// An item without an icon (its column stays reserved for alignment).
    pub fn text(label: impl Into<SharedString>) -> Self {
        MenuEntry::Item { label: label.into(), icon: None, shortcut: None, danger: false }
    }

    pub fn shortcut(mut self, keys: Option<String>) -> Self {
        if let MenuEntry::Item { shortcut, .. } = &mut self {
            *shortcut = keys.map(Into::into);
        }
        self
    }

    pub fn danger(mut self) -> Self {
        if let MenuEntry::Item { danger, .. } = &mut self {
            *danger = true;
        }
        self
    }

    fn is_item(&self) -> bool {
        matches!(self, MenuEntry::Item { .. })
    }
}

pub enum ContextMenuEvent {
    /// Index into the entries passed to [`ContextMenu::new`].
    Chose(usize),
    Dismissed,
}

pub struct ContextMenu {
    entries: Vec<MenuEntry>,
    selected: Option<usize>,
    focus_handle: FocusHandle,
}

impl EventEmitter<ContextMenuEvent> for ContextMenu {}

impl ContextMenu {
    pub fn new(entries: Vec<MenuEntry>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        Self { entries, selected: None, focus_handle }
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let items: Vec<usize> = (0..self.entries.len()).filter(|&i| self.entries[i].is_item()).collect();
        if items.is_empty() {
            return;
        }
        let pos = self.selected.and_then(|s| items.iter().position(|&i| i == s));
        let next = match (pos, forward) {
            (None, true) => 0,
            (None, false) => items.len() - 1,
            (Some(p), true) => (p + 1) % items.len(),
            (Some(p), false) => (p + items.len() - 1) % items.len(),
        };
        self.selected = Some(items[next]);
        cx.notify();
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step(true, cx);
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step(false, cx);
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected {
            cx.emit(ContextMenuEvent::Chose(ix));
        }
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ContextMenuEvent::Dismissed);
    }
}

impl Focusable for ContextMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ContextMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        div()
            .key_context("ContextMenu")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_escape))
            .min_w(px(196.))
            .p_1()
            .flex()
            .flex_col()
            .bg(theme.elevated)
            .border_1()
            .border_color(gpui::Hsla::from(theme.border).alpha(0.85))
            .rounded(px(10.))
            .shadow(crate::ui::soft_shadow(&theme, 2))
            .text_size(px(12.5))
            .children(self.entries.iter().enumerate().map(|(ix, entry)| match entry {
                MenuEntry::Separator => div().my(px(3.)).mx_1().h(px(1.)).bg(theme.border_subtle).into_any_element(),
                MenuEntry::Header(label) => div()
                    .px_2()
                    .pt_1()
                    .pb(px(2.))
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_dim)
                    .child(label.to_uppercase())
                    .into_any_element(),
                MenuEntry::Item { label, icon, shortcut, danger } => {
                    let selected = self.selected == Some(ix);
                    let color: Hsla = if *danger { theme.error.into() } else { theme.text.into() };
                    // The whole row (not just its text) is the hover and click target.
                    // Rows rely on the column's stretch alignment: inside a popover
                    // the menu's width isn't known up front, so `w_full()` would
                    // collapse to the label's width.
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .id(ix)
                                .relative()
                                .flex()
                                .items_center()
                                .gap(px(7.))
                                .h(px(24.))
                                .px(px(7.))
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_color(color)
                                .when(selected, |el| el.bg(theme.accent_soft).child(crate::ui::selected_edge(&theme).top(px(5.)).bottom(px(5.))))
                                .row_hover(theme.accent_soft)
                                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                                    if this.selected != Some(ix) {
                                        this.selected = Some(ix);
                                        cx.notify();
                                    }
                                }))
                                .on_click(cx.listener(move |_, _, _, cx| cx.emit(ContextMenuEvent::Chose(ix))))
                                .child(div().w(px(14.)).flex_shrink_0().when_some(*icon, |el, icon| {
                                    el.child(icon_el(icon, px(13.), if *danger { color } else { theme.text_muted.into() }))
                                }))
                                .child(div().flex_1().child(label.clone()))
                                .when_some(shortcut.clone(), |el, keys| {
                                    el.child(div().pl_4().text_size(px(12.)).text_color(theme.text_dim).child(keys))
                                }),
                        )
                        .into_any_element()
                }
            }))
    }
}
