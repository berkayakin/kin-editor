//! Small shared UI building blocks.

use gpui::{
    AnyView, App, Context, Div, Entity, Focusable, Hsla, IntoElement, ParentElement, Pixels,
    Img, Render, SharedString, Stateful, Styled, Window, div, img, prelude::*, px, svg,
};

use crate::{
    assets::Icon,
    editor::Editor,
    file_icons,
    theme::{Appearance, Theme},
};

// ---- Islands ------------------------------------------------------------------------
//
// Kin's look: content sits on rounded "islands" (the editor's background) that
// float on the chrome (the theme's panel color, faintly lit with the accent from
// above). The main window, Preferences and the pickers all follow it; menus,
// popovers and tooltips are `popover`s floating above everything.

/// Space between islands, and around them.
pub const GAP: f32 = 6.0;
pub const ISLAND_RADIUS: f32 = 12.0;
/// Content masks are square, so content that paints its own background
/// (editor, terminal) is kept this far inside an island's rounded corners.
pub const ISLAND_INSET: f32 = 4.0;

/// The chrome's background: the panel color, with an accent glow at the top
/// that has faded out by `fade` (0–1) of the height; just the panel color
/// with Reduce visual effects.
pub fn chrome_bg(theme: &Theme, fade: f32) -> gpui::Background {
    if theme.reduce_effects {
        return theme.panel.into();
    }
    let top = Hsla::from(theme.panel).blend(Hsla::from(theme.accent).alpha(0.07));
    gpui::linear_gradient(180., gpui::linear_color_stop(top, 0.), gpui::linear_color_stop(theme.panel, fade))
}

/// A soft drop shadow; `depth` 1 for islands, 2 for popovers, 3 for modals.
/// None with Reduce visual effects (borders separate the surfaces then).
pub fn soft_shadow(theme: &Theme, depth: u8) -> Vec<gpui::BoxShadow> {
    if theme.reduce_effects {
        return Vec::new();
    }
    let alpha: f32 = match theme.appearance {
        Appearance::Dark => 0.32,
        Appearance::Light => 0.07,
    };
    let (alpha, y, blur) = match depth {
        0 | 1 => (alpha, 2., 12.),
        2 => (alpha * 1.4, 8., 24.),
        _ => (alpha * 1.8, 16., 48.),
    };
    vec![gpui::BoxShadow {
        color: gpui::hsla(0., 0., 0., alpha.min(0.6)),
        offset: gpui::point(px(0.), px(y)),
        blur_radius: px(blur),
        spread_radius: px(0.),
    }]
}

/// A rounded card on the chrome: the sidebar, each pane, each bottom panel,
/// Preferences' pages, a picker's list.
pub fn island(theme: &Theme) -> Div {
    div()
        .relative()
        .flex()
        .flex_col()
        .p(px(ISLAND_INSET))
        .rounded(px(ISLAND_RADIUS))
        .bg(theme.background)
        .border_1()
        .border_color(Hsla::from(theme.border).alpha(0.75))
        .shadow(soft_shadow(theme, 1))
}

/// A card floating above the window: menus, completion, hover, toasts.
pub fn popover(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(10.))
        .bg(theme.elevated)
        .border_1()
        .border_color(Hsla::from(theme.border).alpha(0.85))
        .shadow(soft_shadow(theme, 2))
}

/// The lit edge that marks the selected row in a list (the row must be
/// `relative`), like the open file in the file tree: a short pill set just
/// inside the row, wide enough that its rounded ends show.
pub fn selected_edge(theme: &Theme) -> Div {
    div().absolute().left(px(3.)).top(px(7.)).bottom(px(7.)).w(px(3.)).rounded_full().bg(theme.accent)
}

/// A container for related toggles, drawn like a segmented control; its
/// buttons use `segment_on` for the one that's active.
pub fn dock(theme: &Theme) -> Div {
    div().flex().flex_shrink_0().items_center().gap(px(2.)).p(px(2.)).rounded(px(9.)).bg(theme.hover)
}

pub fn segment_on<E: Styled>(el: E, theme: &Theme) -> E {
    el.bg(theme.background).shadow(soft_shadow(theme, 1))
}

/// A soft accent halo for focused fields and active controls; none with
/// Reduce visual effects (their accent border still marks them).
pub fn focus_glow(theme: &Theme) -> Vec<gpui::BoxShadow> {
    glow(theme, Hsla::from(theme.accent).alpha(0.22), 6., 1.)
}

/// A halo of `color` around an element, e.g. a status light; none with
/// Reduce visual effects.
pub fn glow(theme: &Theme, color: Hsla, blur: f32, spread: f32) -> Vec<gpui::BoxShadow> {
    if theme.reduce_effects {
        return Vec::new();
    }
    vec![gpui::BoxShadow {
        color,
        offset: gpui::point(px(0.), px(0.)),
        blur_radius: px(blur),
        spread_radius: px(spread),
    }]
}

/// The file-type icon for `path`, in the pack's own colors (see `file_icons`).
pub fn file_icon(path: &std::path::Path, size: Pixels, theme: &Theme) -> Img {
    img(file_icons::file_source(path, theme.appearance)).size(size).flex_shrink_0()
}

pub fn folder_icon(open: bool, size: Pixels, theme: &Theme) -> Img {
    img(file_icons::folder_source(open, theme.appearance)).size(size).flex_shrink_0()
}

pub fn icon_el(icon: Icon, size: Pixels, color: Hsla) -> impl IntoElement {
    svg().path(icon.path()).size(size).flex_shrink_0().text_color(color)
}

/// A rounded text field wrapping a single-line editor.
pub fn input_box(editor: &Entity<Editor>, icon: Option<Icon>, window: &Window, cx: &App) -> Div {
    let theme = Theme::get(cx);
    let focused = editor.read(cx).focus_handle(cx).is_focused(window);
    div()
        .flex()
        .items_center()
        .gap_2()
        .flex_1()
        .min_w_0()
        .h(px(32.))
        .px_2p5()
        .rounded(px(10.))
        .bg(theme.background)
        .border_1()
        .border_color(if focused { Hsla::from(theme.accent).alpha(0.7) } else { Hsla::from(theme.border).alpha(0.75) })
        .when(focused, |el| el.shadow(focus_glow(theme)))
        .when_some(icon, |el, icon| el.child(icon_el(icon, px(14.), theme.text_dim.into())))
        .child(div().flex_1().min_w_0().overflow_hidden().child(editor.clone()))
}

/// A small toggle like JetBrains' "Cc", "W" and ".*" search options.
pub fn toggle(id: &'static str, label: &'static str, active: bool, tooltip: &'static str, cx: &App) -> Stateful<Div> {
    let theme = Theme::get(cx);
    div()
        .id(id)
        .h(px(24.))
        .min_w(px(26.))
        .px_1p5()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.))
        .font_family(theme.mono_font.clone())
        .text_size(px(12.))
        .cursor_pointer()
        .when(active, |el| el.bg(theme.accent_soft).text_color(theme.accent))
        .when(!active, |el| el.text_color(theme.text_muted).hover(|s| s.bg(theme.hover)))
        .tooltip(move |_, cx| tooltip_view(tooltip, cx))
        .child(label)
}

/// An on/off switch (draw only; the caller handles clicks).
pub fn switch(on: bool, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .w(px(28.))
        .h(px(16.))
        .p(px(2.))
        .rounded_full()
        .when(on, |el| el.bg(theme.accent).justify_end().shadow(focus_glow(theme)))
        .when(!on, |el| el.bg(theme.active))
        .child(div().size(px(12.)).rounded_full().bg(if on { theme.background } else { theme.text_dim }))
}

pub fn icon_button(id: impl Into<gpui::ElementId>, icon: Icon, color: Hsla, cx: &App) -> Stateful<Div> {
    let theme = Theme::get(cx);
    div()
        .id(id)
        .size(px(26.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded(px(8.))
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover))
        .active(|s| s.bg(theme.active))
        .child(icon_el(icon, px(15.), color))
}

pub fn text_button(id: impl Into<gpui::ElementId>, label: &'static str, primary: bool, cx: &App) -> Stateful<Div> {
    let theme = Theme::get(cx);
    div()
        .id(id)
        .h(px(28.))
        .px_3()
        .flex()
        .flex_shrink_0()
        .items_center()
        .rounded(px(14.))
        .cursor_pointer()
        .font_weight(gpui::FontWeight::MEDIUM)
        .when(primary, |el| {
            el.bg(theme.accent_soft)
                .text_color(theme.accent)
                .border_1()
                .border_color(Hsla::from(theme.accent).alpha(0.3))
                .hover(|s| s.bg(Hsla::from(theme.accent).alpha(0.24)))
        })
        .when(!primary, |el| {
            el.bg(theme.hover)
                .text_color(theme.text)
                .hover(|s| s.bg(theme.active))
        })
        .child(label)
}

/// A key hint chip, e.g. "⌘⇧F".
pub fn kbd(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    let theme = Theme::get(cx);
    div()
        .px_1p5()
        .rounded(px(5.))
        .bg(theme.hover)
        .border_1()
        .border_color(theme.border_subtle)
        .text_size(px(11.5))
        .text_color(theme.text_muted)
        .child(text.into())
}

/// The shortcut currently bound to `action`, formatted for this platform.
pub fn binding_label(action: &dyn gpui::Action, window: &Window) -> Option<String> {
    // Look the binding up as if an editor were focused, so editor-only
    // shortcuts show too. The first built-in binding is the primary one.
    let context = gpui::KeyContext::parse("Workspace Editor").ok()?;
    let bindings = window.bindings_for_action_in_context(action, context);
    // The user's own binding (keymap.json) is the one to show.
    let binding = bindings
        .iter()
        .rev()
        .find(|b| b.meta() == Some(crate::keymap::USER_BINDING))
        .or(bindings.first())?;
    let parts: Vec<String> = binding
        .keystrokes()
        .iter()
        .map(|k| shortcut_label(&k.to_string()))
        .collect();
    Some(parts.join(" "))
}

/// Renders a keybinding like `secondary-shift-o` the platform's way.
pub fn shortcut_label(keys: &str) -> String {
    let mac = cfg!(target_os = "macos");
    // GPUI already renders macOS keystrokes with symbols (e.g. "⇧enter");
    // only the key name needs tidying.
    if mac && !keys.contains('-') {
        let split = keys.char_indices().find(|(_, c)| !"⌃⌥⇧⌘".contains(*c)).map_or(keys.len(), |(i, _)| i);
        let (mods, key) = keys.split_at(split);
        let key = match key {
            "enter" => "↩".to_string(),
            "escape" => "Esc".to_string(),
            "tab" => "⇥".to_string(),
            "space" => "Space".to_string(),
            k if k.chars().count() == 1 => k.to_uppercase(),
            k => {
                let mut c = k.chars();
                c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
            }
        };
        return format!("{mods}{key}");
    }
    let tokens: Vec<&str> = if keys.ends_with("--") {
        // A binding on the '-' key itself.
        let mut t: Vec<&str> = keys[..keys.len() - 2].split('-').collect();
        t.push("-");
        t
    } else {
        keys.split('-').collect()
    };
    let parts: Vec<String> = tokens
        .iter()
        .map(|k| match (*k, mac) {
            ("secondary" | "cmd" | "platform", true) => "⌘".into(),
            ("secondary", false) | ("ctrl", false) => "Ctrl".into(),
            ("shift", true) => "⇧".into(),
            ("shift", false) => "Shift".into(),
            ("alt", true) => "⌥".into(),
            ("alt", false) => "Alt".into(),
            ("ctrl", true) => "⌃".into(),
            ("enter", true) => "↩".into(),
            ("backspace", true) => "⌫".into(),
            ("up", _) => "↑".into(),
            ("down", _) => "↓".into(),
            ("left", _) => "←".into(),
            ("right", _) => "→".into(),
            ("escape", _) => "Esc".into(),
            (k, _) if k.len() == 1 => k.to_uppercase(),
            (k, _) => {
                let mut c = k.chars();
                c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
            }
        })
        .collect();
    if mac {
        // macOS orders modifiers ⌃⌥⇧⌘ and joins without separators.
        let (mods, keys): (Vec<&String>, Vec<&String>) =
            parts.iter().partition(|p| ["⌃", "⌥", "⇧", "⌘"].contains(&p.as_str()));
        let order = ["⌃", "⌥", "⇧", "⌘"];
        let mut mods = mods;
        mods.sort_by_key(|m| order.iter().position(|o| o == m));
        mods.into_iter().chain(keys).map(String::as_str).collect()
    } else {
        parts.join("+")
    }
}

pub struct Tooltip(pub SharedString);

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx);
        popover(theme)
            // Block, not the popover's flex column: in a column GPUI sizes the
            // box for one unwrapped line, and wrapped text spills out of it.
            .block()
            .max_w(px(420.))
            .px_2()
            .py_1()
            .rounded(px(8.))
            .text_size(px(12.))
            .text_color(theme.text)
            .font_family(theme.ui_font.clone())
            .child(self.0.clone())
    }
}

pub fn tooltip_view(text: impl Into<SharedString>, cx: &mut App) -> AnyView {
    let text = text.into();
    cx.new(|_| Tooltip(text)).into()
}

/// The floating panel used by all modal popups: chrome, like the window
/// around the islands. Long content goes into an `island` inside it.
pub fn modal_panel(cx: &App) -> Div {
    let theme = Theme::get(cx);
    div()
        .flex()
        .flex_col()
        .bg(chrome_bg(theme, 0.5))
        .border_1()
        .border_color(Hsla::from(theme.border).alpha(0.85))
        .rounded(px(16.))
        .shadow(soft_shadow(theme, 3))
        .overflow_hidden()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduced_effects_drop_gradients_shadows_and_glows() {
        let mut theme = Theme::blank_for_tests();
        theme.panel = gpui::rgba(0x202020ff);
        theme.accent = gpui::rgba(0xff8000ff);
        assert_ne!(chrome_bg(&theme, 0.5), gpui::Background::from(theme.panel));
        assert!(!soft_shadow(&theme, 1).is_empty() && !focus_glow(&theme).is_empty());
        theme.reduce_effects = true;
        assert_eq!(chrome_bg(&theme, 0.5), gpui::Background::from(theme.panel));
        assert!(soft_shadow(&theme, 3).is_empty());
        assert!(focus_glow(&theme).is_empty());
        assert!(glow(&theme, theme.accent.into(), 4., 0.).is_empty());
    }
}
