//! The Keymap page: every Find Action entry with its shortcuts. Clicking a
//! row records a new shortcut, which goes into `keymap.json`.

use gpui::{
    AnyWindowHandle, App, ClickEvent, Context, Entity, EventEmitter, Focusable, FontWeight, Hsla, IntoElement, KeyBinding,
    KeyContext, Keystroke, Render, SharedString, Subscription, WeakEntity, Window, div, prelude::*, px,
};

use super::PreferencesEvent;
use crate::{
    assets::Icon,
    editor::{Editor, EditorEvent},
    keymap::{self, ShortcutChange},
    picker::ActionEntry,
    settings::{self, BaseKeymap},
    theme::Theme,
    ui::{HoverFill, icon_button, icon_el, input_box, kbd, shortcut_label, text_button, tooltip_view},
    workspace::SearchEverywhere,
};

struct Row {
    entry: ActionEntry,
    /// The built-in keys that apply in the editor, for telling changed rows
    /// apart (in the keymap `defaults_for`).
    default_keys: Vec<String>,
}

pub struct KeymapPage {
    filter: Entity<Editor>,
    rows: Vec<Row>,
    /// The base keymap `default_keys` came from.
    defaults_for: BaseKeymap,
    recording: Option<usize>,
    /// Shown under a row after recording, e.g. other actions using the keys.
    notice: Option<(usize, String)>,
    _subscriptions: Vec<Subscription>,
    scroll: gpui::ScrollHandle,
}

impl EventEmitter<PreferencesEvent> for KeymapPage {}

/// Where shortcuts are looked up: as if an editor had focus, like `binding_label`.
fn lookup_context() -> KeyContext {
    KeyContext::parse("Workspace Editor").unwrap_or_default()
}

fn applies_in_editor(binding: &KeyBinding) -> bool {
    binding.predicate().is_none_or(|p| matches!(p.to_string().as_str(), "Editor" | "Workspace"))
}

impl KeymapPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| Editor::single_line("Filter by action or shortcut", cx));
        let base = keymap::current(cx);
        let rows = keymap::action_entries().into_iter().map(|entry| Row { entry, default_keys: Vec::new() }).collect();
        let page: WeakEntity<Self> = cx.entity().downgrade();
        let handle: AnyWindowHandle = window.window_handle();
        let subscriptions = vec![
            cx.subscribe(&filter, |this, _, _: &EditorEvent, cx| {
                this.notice = None;
                cx.notify();
            }),
            // While recording, keystrokes become the shortcut instead of running anything.
            cx.intercept_keystrokes(move |event, window, cx| {
                if window.window_handle() != handle {
                    return;
                }
                let Some(page) = page.upgrade() else { return };
                if page.read(cx).recording.is_none() {
                    return;
                }
                let keystroke = event.keystroke.clone();
                if matches!(keystroke.key.as_str(), "shift" | "control" | "alt" | "platform" | "function") {
                    return;
                }
                cx.stop_propagation();
                page.update(cx, |page, cx| page.record(keystroke, window, cx));
            }),
        ];
        let mut page = Self { filter, rows, defaults_for: base, recording: None, notice: None, _subscriptions: subscriptions, scroll: gpui::ScrollHandle::new() };
        page.load_defaults(base);
        page
    }

    /// Each action's built-in keys in `base`.
    fn load_defaults(&mut self, base: BaseKeymap) {
        let defaults = keymap::bindings(base);
        for row in &mut self.rows {
            let mut keys: Vec<String> = defaults
                .iter()
                .filter(|b| b.action().partial_eq(row.entry.action.as_ref()) && applies_in_editor(b))
                .map(keymap::binding_keys)
                .collect();
            keys.sort();
            keys.dedup();
            row.default_keys = keys;
        }
        self.defaults_for = base;
    }

    pub fn focus_filter(&self, window: &mut Window, cx: &App) {
        window.focus(&self.filter.focus_handle(cx));
    }

    pub fn set_filter(&mut self, text: &str, cx: &mut Context<Self>) {
        self.filter.update(cx, |e, cx| {
            e.set_text(text, cx);
            e.select_range(text.len()..text.len(), cx);
        });
    }

    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    pub fn stop_recording(&mut self, cx: &mut Context<Self>) {
        self.recording = None;
        cx.notify();
    }

    /// How many actions' names match `query` (for Preferences' search).
    pub fn count_matching(&self, query: &str, _window: &Window) -> usize {
        let words = words(query);
        self.rows.iter().filter(|r| matches_name(&r.entry, &words)).count()
    }

    fn current_bindings(&self, row: &Row, window: &Window) -> Vec<KeyBinding> {
        window.bindings_for_action_in_context(row.entry.action.as_ref(), lookup_context())
    }

    fn start_recording(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.recording = if self.recording == Some(ix) { None } else { Some(ix) };
        self.notice = None;
        cx.notify();
    }

    fn record(&mut self, keystroke: Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.recording else { return };
        if keystroke.key == "escape" && keystroke.modifiers == Default::default() {
            self.stop_recording(cx);
            return;
        }
        let keys = keystroke.unparse();
        self.change(ix, ShortcutChange::Set(keys.clone()), window, cx);
        // Point out other actions the keys now collide with.
        let others: Vec<&'static str> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != ix)
            .filter(|(_, row)| self.current_bindings(row, window).iter().any(|b| keymap::binding_keys(b) == keys))
            .map(|(_, row)| row.entry.name)
            .collect();
        if !others.is_empty() {
            let label = shortcut_label(&keys);
            self.notice = Some((ix, format!("{label} is also bound to {}", others.join(", "))));
        }
    }

    fn change(&mut self, ix: usize, change: ShortcutChange, _window: &mut Window, cx: &mut Context<Self>) {
        self.recording = None;
        self.notice = None;
        let Some(row) = self.rows.get(ix) else { return };
        let mut problems = Vec::new();
        if let Err(err) = keymap::change_shortcut(row.entry.name, row.entry.action.as_ref(), change, cx) {
            problems.push(format!("keymap.json: {err}"));
        }
        problems.extend(settings::reload_keymap_now(cx));
        if !problems.is_empty() {
            cx.emit(PreferencesEvent::Problems(problems));
        }
        cx.notify();
    }
}

fn words(query: &str) -> Vec<String> {
    query.to_lowercase().split_whitespace().map(str::to_string).collect()
}

fn matches_name(entry: &ActionEntry, words: &[String]) -> bool {
    let name = entry.name.to_lowercase();
    words.iter().all(|w| name.contains(w.as_str()))
}

impl Render for KeymapPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Another base keymap was picked: "Changed" compares against its keys.
        let base = keymap::current(cx);
        if base != self.defaults_for {
            self.load_defaults(base);
        }
        let theme = Theme::get(cx).clone();
        let query = self.filter.read(cx).text(cx);
        let filter_words = words(&query);
        let mut rows = Vec::new();
        for (ix, row) in self.rows.iter().enumerate() {
            let bindings = self.current_bindings(row, window);
            let mut labels: Vec<String> = Vec::new();
            let mut current_keys: Vec<String> = Vec::new();
            for binding in &bindings {
                let label = binding.keystrokes().iter().map(|k| shortcut_label(&k.to_string())).collect::<Vec<_>>().join(" ");
                if !labels.contains(&label) {
                    labels.push(label);
                }
                current_keys.push(keymap::binding_keys(binding));
            }
            current_keys.sort();
            current_keys.dedup();
            // Double Shift isn't a binding; the workspace detects it.
            if row.entry.action.partial_eq(&SearchEverywhere) {
                labels.insert(0, if cfg!(target_os = "macos") { "⇧⇧".into() } else { "Shift Shift".into() });
            }
            let matches = filter_words.is_empty()
                || matches_name(&row.entry, &filter_words)
                || labels.iter().chain(&current_keys).any(|k| k.to_lowercase().replace(' ', "").contains(&query.to_lowercase().replace(' ', "")));
            if !matches {
                continue;
            }
            let customized = current_keys != row.default_keys;
            let recording = self.recording == Some(ix);
            let group = SharedString::from(format!("keymap-row-{ix}"));
            let shortcuts = if recording {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .px_2()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .border_1()
                            .border_color(Hsla::from(theme.accent).alpha(0.7))
                            .text_size(px(12.))
                            .text_color(theme.text)
                            .child("Press a shortcut…"),
                    )
                    .child(
                        text_button("keymap-remove", "No Shortcut", false, cx)
                            .h(px(24.))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.change(ix, ShortcutChange::Remove, window, cx);
                            })),
                    )
                    .into_any_element()
            } else if labels.is_empty() {
                div().text_color(theme.text_dim).child("—").into_any_element()
            } else {
                div().flex().items_center().gap_1().children(labels.into_iter().map(|l| kbd(l, cx))).into_any_element()
            };
            let notice = self.notice.as_ref().filter(|(n, _)| *n == ix).map(|(_, text)| text.clone());
            rows.push(
                div()
                    .flex()
                    .flex_col()
                    .when(!rows.is_empty(), |el| el.border_t_1().border_color(theme.border_subtle))
                    .child(
                        div()
                            .id(("keymap-row", ix))
                            .group(group.clone())
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(36.))
                            .px_2()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .when(recording, |el| el.bg(theme.accent_soft))
                            .row_hover(theme.hover)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.start_recording(ix, cx)))
                            .child(div().flex_1().min_w_0().truncate().text_color(theme.text).child(row.entry.name))
                            .when(customized && !recording, |el| {
                                el.child(
                                    div()
                                        .px_1p5()
                                        .rounded(px(4.))
                                        .bg(theme.hover)
                                        .text_size(px(10.5))
                                        .text_color(theme.text_muted)
                                        .child("Changed"),
                                )
                            })
                            .child(shortcuts)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .w(px(52.))
                                    .justify_end()
                                    .gap_0p5()
                                    .when(!recording, |el| {
                                        el.child(
                                            div()
                                                .invisible()
                                                .group_hover(group.clone(), |s| s.visible())
                                                .child(icon_el(Icon::Pencil, px(13.), theme.text_dim.into())),
                                        )
                                    })
                                    .when(customized, |el| {
                                        el.child(
                                            icon_button(("keymap-reset", ix), Icon::Undo, theme.text_dim.into(), cx)
                                                .size(px(22.))
                                                .tooltip(|_, cx| tooltip_view("Reset to default", cx))
                                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                                    cx.stop_propagation();
                                                    this.change(ix, ShortcutChange::Reset, window, cx);
                                                })),
                                        )
                                    }),
                            ),
                    )
                    .when_some(notice, |el, text| {
                        el.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .px_2()
                                .pb_2()
                                .text_size(px(12.))
                                .text_color(theme.warning)
                                .child(icon_el(Icon::TriangleAlert, px(12.), theme.warning.into()))
                                .child(text),
                        )
                    })
                    .into_any_element(),
            );
        }
        let empty = rows.is_empty();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_3()
            .child(
                div()
                    .px(px(28.))
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(input_box(&self.filter, Some(Icon::Search), window, cx))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(12.))
                            .text_color(theme.text_dim)
                            .child("Click an action, then press its new keys"),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                div()
                    .id("keymap-list")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .px(px(28.))
                    .pb(px(28.))
                    .child(if empty {
                        div()
                            .pt(px(40.))
                            .flex()
                            .justify_center()
                            .text_color(theme.text_dim)
                            .font_weight(FontWeight::NORMAL)
                            .child("No actions match")
                            .into_any_element()
                    } else {
                        div()
                            .flex()
                            .flex_col()
                            .rounded(px(10.))
                            .bg(theme.elevated)
                            .border_1()
                            .border_color(theme.border_subtle)
                            // Inset rows, so their rounded highlights clear the card's corners.
                            .p_1()
                            .children(rows)
                            .into_any_element()
                    }))
                    .child(crate::ui::scrollbar("keymap-scrollbar", &self.scroll, cx)),
            )
    }
}
