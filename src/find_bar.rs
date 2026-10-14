//! In-file Find / Replace bar shown above the active editor (Ctrl+F / Ctrl+R).

use std::ops::Range;

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla, IntoElement, Render, Subscription,
    Window, div, prelude::*, px,
};

use crate::{
    assets::Icon,
    editor::{
        Cancel, Editor, EditorEvent, Indent, MoveDown, MoveUp, Newline, NewlineBelow, Outdent,
    },
    search::{SearchOptions, SearchQuery},
    theme::Theme,
    ui::{icon_button, input_box, text_button, toggle, tooltip_view},
};

pub enum FindBarEvent {
    Dismissed,
}

pub struct FindBar {
    query: Entity<Editor>,
    replacement: Entity<Editor>,
    replace_mode: bool,
    options: SearchOptions,
    target: Option<Entity<Editor>>,
    target_subscription: Option<Subscription>,
    matches: Vec<Range<usize>>,
    active: Option<usize>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FindBarEvent> for FindBar {}

impl FindBar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| Editor::single_line("Search", cx));
        let replacement = cx.new(|cx| Editor::single_line("Replace", cx));
        let subscription = cx.subscribe(&query, |this, _, _: &EditorEvent, cx| this.update_matches(true, cx));
        Self {
            query,
            replacement,
            replace_mode: false,
            options: SearchOptions::default(),
            target: None,
            target_subscription: None,
            matches: Vec::new(),
            active: None,
            error: None,
            _subscriptions: vec![subscription],
        }
    }

    pub fn has_query(&self, cx: &gpui::App) -> bool {
        self.query.read(cx).doc.len() > 0
    }

    /// Opens the bar for `target`, prefilled with the editor's selection.
    pub fn show(&mut self, target: Entity<Editor>, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_mode = replace;
        self.set_target(Some(target.clone()), cx);
        let selected = {
            let editor = target.read(cx);
            let s = editor.primary_selection();
            let text = editor.doc.slice(s.range());
            (!s.is_empty() && !text.contains('\n')).then_some(text)
        };
        if let Some(text) = selected {
            self.query.update(cx, |q, cx| q.set_text(&text, cx));
        }
        self.query.update(cx, |q, cx| q.select_all_text(cx));
        window.focus(&self.query.focus_handle(cx));
        self.update_matches(false, cx);
    }

    /// Follows the active tab; highlights move with it.
    pub fn set_target(&mut self, target: Option<Entity<Editor>>, cx: &mut Context<Self>) {
        if self.target == target {
            return;
        }
        if let Some(old) = self.target.take() {
            old.update(cx, |e, cx| e.set_search_highlights(Vec::new(), None, cx));
        }
        self.target_subscription = target
            .as_ref()
            .map(|t| cx.subscribe(t, |this, _, _: &EditorEvent, cx| this.update_matches(false, cx)));
        self.target = target;
        self.update_matches(false, cx);
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        if let Some(target) = self.target.as_ref() {
            target.update(cx, |e, cx| e.set_search_highlights(Vec::new(), None, cx));
        }
        self.target = None;
        self.target_subscription = None;
        cx.emit(FindBarEvent::Dismissed);
    }

    fn query(&self, cx: &gpui::App) -> Result<Option<SearchQuery>, String> {
        SearchQuery::new(&self.query.read(cx).text(), self.options)
    }

    /// Recomputes matches. When the query changed, jumps to the nearest match.
    fn update_matches(&mut self, jump: bool, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else { return };
        self.error = None;
        self.matches = match self.query(cx) {
            Ok(Some(q)) => q.find_all(&target.read(cx).doc.rope().to_string()),
            Ok(None) => Vec::new(),
            Err(e) => {
                self.error = Some(e);
                Vec::new()
            }
        };
        let cursor = target.read(cx).primary_selection().range().start;
        self.active = if self.matches.is_empty() {
            None
        } else {
            Some(self.matches.iter().position(|m| m.end > cursor).unwrap_or(0))
        };
        let (matches, active) = (self.matches.clone(), self.active);
        target.update(cx, |e, cx| {
            e.set_search_highlights(matches.clone(), active, cx);
            if jump && let Some(a) = active {
                e.select_range(matches[a].clone(), cx);
            }
        });
        cx.notify();
    }

    pub fn select_next(&mut self, cx: &mut Context<Self>) {
        self.step(true, cx);
    }

    pub fn select_prev(&mut self, cx: &mut Context<Self>) {
        self.step(false, cx);
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else { return };
        if self.matches.is_empty() {
            return;
        }
        let sel = target.read(cx).primary_selection().range();
        let n = self.matches.len();
        let ix = if forward {
            self.matches.iter().position(|m| m.start > sel.start).unwrap_or(0)
        } else {
            self.matches.iter().rposition(|m| m.start < sel.start).unwrap_or(n - 1)
        };
        self.active = Some(ix);
        let (matches, range) = (self.matches.clone(), self.matches[ix].clone());
        target.update(cx, |e, cx| {
            e.set_search_highlights(matches, Some(ix), cx);
            e.select_range(range, cx);
        });
        cx.notify();
    }

    fn replace_one(&mut self, cx: &mut Context<Self>) {
        let (Some(target), Some(ix), Ok(Some(query))) = (self.target.clone(), self.active, self.query(cx)) else {
            return;
        };
        let range = self.matches[ix].clone();
        if target.read(cx).primary_selection().range() != range {
            self.select_next(cx);
            return;
        }
        let text = target.read(cx).doc.rope().to_string();
        let replacement = query.replacement_for(&text, range.clone(), &self.replacement.read(cx).text());
        let after = range.start + replacement.len();
        target.update(cx, |e, cx| {
            e.replace_ranges(vec![(range, replacement)], cx);
            e.select_range(after..after, cx);
        });
        // Matches were recomputed by the edit subscription; go to the next one.
        self.select_next(cx);
    }

    fn replace_all(&mut self, cx: &mut Context<Self>) {
        let (Some(target), Ok(Some(query))) = (self.target.clone(), self.query(cx)) else {
            return;
        };
        let text = target.read(cx).doc.rope().to_string();
        let replacement = self.replacement.read(cx).text();
        let edits: Vec<_> = self
            .matches
            .iter()
            .map(|m| (m.clone(), query.replacement_for(&text, m.clone(), &replacement)))
            .collect();
        target.update(cx, |e, cx| e.replace_ranges(edits, cx));
    }

    fn toggle_option(&mut self, f: impl FnOnce(&mut SearchOptions), cx: &mut Context<Self>) {
        f(&mut self.options);
        self.update_matches(true, cx);
    }

    // Keys typed in the inputs bubble up here as editor actions.

    fn on_enter(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.replacement.focus_handle(cx).is_focused(window) {
            self.replace_one(cx);
        } else {
            self.select_next(cx);
        }
    }

    fn on_shift_enter(&mut self, _: &NewlineBelow, _: &mut Window, cx: &mut Context<Self>) {
        self.select_prev(cx);
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.select_next(cx);
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select_prev(cx);
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.dismiss(cx);
    }

    fn on_tab(&mut self, _: &Indent, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_field(window, cx);
    }

    fn on_shift_tab(&mut self, _: &Outdent, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_field(window, cx);
    }

    fn switch_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replace_mode {
            return;
        }
        let to = if self.query.focus_handle(cx).is_focused(window) {
            &self.replacement
        } else {
            &self.query
        };
        to.update(cx, |e, cx| e.select_all_text(cx));
        window.focus(&to.focus_handle(cx));
    }
}

impl Focusable for FindBar {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for FindBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let has_query = self.has_query(cx);
        let (status, status_color): (String, Hsla) = match (&self.error, self.active) {
            (Some(e), _) => (e.clone(), theme.error.into()),
            (None, Some(a)) => (format!("{}/{}", a + 1, self.matches.len()), theme.text_muted.into()),
            (None, None) if has_query => ("0 results".into(), theme.error.into()),
            _ => (String::new(), theme.text_muted.into()),
        };
        let opts = self.options;

        let search_row = div()
            .flex()
            .items_center()
            .gap_1()
            .child(input_box(&self.query, Some(Icon::Search), window, cx))
            .child(
                div()
                    .min_w(px(78.))
                    .px_2()
                    .text_size(px(12.))
                    .text_color(status_color)
                    .truncate()
                    .child(status),
            )
            .child(
                icon_button("find-prev", Icon::ChevronUp, theme.text_muted.into(), cx)
                    .tooltip(|_, cx| tooltip_view("Previous Occurrence (⇧↩)", cx))
                    .on_click(cx.listener(|this, _, _, cx| this.select_prev(cx))),
            )
            .child(
                icon_button("find-next", Icon::ChevronDown, theme.text_muted.into(), cx)
                    .tooltip(|_, cx| tooltip_view("Next Occurrence (↩)", cx))
                    .on_click(cx.listener(|this, _, _, cx| this.select_next(cx))),
            )
            .child(div().w(px(1.)).h(px(16.)).mx_1().bg(theme.border))
            .child(
                toggle("opt-case", "Cc", opts.case_sensitive, "Match Case", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| o.case_sensitive = !o.case_sensitive, cx))),
            )
            .child(
                toggle("opt-word", "W", opts.whole_word, "Words", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| o.whole_word = !o.whole_word, cx))),
            )
            .child(
                toggle("opt-regex", ".*", opts.regex, "Regex", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| o.regex = !o.regex, cx))),
            )
            .child(
                icon_button("find-close", Icon::X, theme.text_muted.into(), cx)
                    .tooltip(|_, cx| tooltip_view("Close (Esc)", cx))
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx))),
            );

        div()
            .key_context("FindBar")
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_shift_enter))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_shift_tab))
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_1p5()
            .mx_1()
            .mb_1()
            .px_2()
            .py_1p5()
            .rounded(px(10.))
            .bg(theme.panel)
            .border_1()
            .border_color(gpui::Hsla::from(theme.border).alpha(0.75))
            .child(search_row)
            .when(self.replace_mode, |el| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(input_box(&self.replacement, Some(Icon::Replace), window, cx))
                        .child(
                            text_button("replace-one", "Replace", false, cx)
                                .on_click(cx.listener(|this, _, _, cx| this.replace_one(cx))),
                        )
                        .child(
                            text_button("replace-all", "Replace All", true, cx)
                                .on_click(cx.listener(|this, _, _, cx| this.replace_all(cx))),
                        ),
                )
            })
    }
}
