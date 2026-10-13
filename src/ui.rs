//! Small shared UI building blocks.

use gpui::{
    AnyView, App, Context, Div, Entity, Focusable, Hsla, IntoElement, ParentElement, Pixels,
    Render, SharedString, Stateful, Styled, Window, div, prelude::*, px, svg,
};

use crate::{assets::Icon, editor::Editor, syntax::Language, theme::Theme};

/// The icon and tint for a file, by language.
pub fn file_icon(path: &std::path::Path, theme: &Theme) -> (Icon, Hsla) {
    match Language::from_path(path) {
        Language::JavaScript => (Icon::FileCode, theme.file_js.into()),
        Language::Html => (Icon::CodeXml, theme.file_html.into()),
        Language::Rust | Language::Go | Language::Python | Language::Php => (Icon::FileCode, theme.text_muted.into()),
        Language::Markdown => (Icon::FileText, theme.text_muted.into()),
        Language::PlainText => (Icon::File, theme.text_dim.into()),
    }
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
        .h(px(30.))
        .px_2()
        .rounded(px(7.))
        .bg(theme.background)
        .border_1()
        .border_color(if focused { Hsla::from(theme.accent).alpha(0.7) } else { theme.border.into() })
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
        .rounded(px(6.))
        .font_family(theme.mono_font.clone())
        .text_size(px(12.))
        .cursor_pointer()
        .when(active, |el| el.bg(theme.accent_soft).text_color(theme.accent))
        .when(!active, |el| el.text_color(theme.text_muted).hover(|s| s.bg(theme.hover)))
        .tooltip(move |_, cx| tooltip_view(tooltip, cx))
        .child(label)
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
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover))
        .active(|s| s.bg(theme.active))
        .child(icon_el(icon, px(15.), color))
}

pub fn text_button(id: &'static str, label: &'static str, primary: bool, cx: &App) -> Stateful<Div> {
    let theme = Theme::get(cx);
    div()
        .id(id)
        .h(px(28.))
        .px_3()
        .flex()
        .flex_shrink_0()
        .items_center()
        .rounded(px(7.))
        .cursor_pointer()
        .font_weight(gpui::FontWeight::MEDIUM)
        .when(primary, |el| {
            el.bg(theme.accent_soft)
                .text_color(theme.accent)
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
        .rounded(px(4.))
        .bg(theme.hover)
        .text_size(px(11.5))
        .text_color(theme.text_muted)
        .child(text.into())
}

/// The shortcut currently bound to `action`, formatted for this platform.
pub fn binding_label(action: &dyn gpui::Action, window: &Window) -> Option<String> {
    // Look the binding up as if an editor were focused, so editor-only
    // shortcuts show too. The first binding is the primary one.
    let context = gpui::KeyContext::parse("Workspace Editor").ok()?;
    let binding = window.bindings_for_action_in_context(action, context).into_iter().next()?;
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
        div()
            .max_w(px(420.))
            .px_2()
            .py_1()
            .rounded(px(6.))
            .bg(theme.elevated)
            .border_1()
            .border_color(theme.border)
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

/// The floating panel used by all modal popups.
pub fn modal_panel(cx: &App) -> Div {
    let theme = Theme::get(cx);
    div()
        .flex()
        .flex_col()
        .bg(theme.elevated)
        .border_1()
        .border_color(theme.border)
        .rounded(px(12.))
        .shadow_lg()
        .overflow_hidden()
}
