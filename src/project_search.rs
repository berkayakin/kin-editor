//! "Find in Files" / "Replace in Files" popup (Ctrl+Shift+F / Ctrl+Shift+R).

use std::{
    collections::HashMap,
    ops::Range,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle,
    IntoElement, Render, ScrollStrategy, SharedString, StyledText, Subscription, Task,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use crate::{
    assets::Icon,
    editor::{Cancel, Editor, EditorEvent, Indent, MoveDown, MoveUp, Newline, Outdent, PageDown, PageUp},
    search::{ProjectMatch, SearchOptions, SearchQuery, search_project},
    theme::Theme,
    ui::{icon_el, input_box, kbd, modal_panel, text_button, toggle},
};

pub enum ProjectSearchEvent {
    Open { path: PathBuf, range: Range<usize> },
    ReplaceAll { query: SearchQuery, replacement: String, files: Vec<PathBuf> },
    Dismissed,
}

pub struct ProjectSearch {
    root: PathBuf,
    query: Entity<Editor>,
    replacement: Entity<Editor>,
    replace_mode: bool,
    options: SearchOptions,
    overrides: HashMap<PathBuf, Arc<str>>,
    matches: Arc<Vec<ProjectMatch>>,
    files: usize,
    truncated: bool,
    searching: bool,
    error: Option<String>,
    selected: usize,
    scroll: UniformListScrollHandle,
    task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ProjectSearchEvent> for ProjectSearch {}

impl ProjectSearch {
    pub fn new(
        root: PathBuf,
        initial: Option<String>,
        replace_mode: bool,
        overrides: HashMap<PathBuf, Arc<str>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| Editor::single_line("Text to find", cx));
        let replacement = cx.new(|cx| Editor::single_line("Replace with", cx));
        if let Some(text) = initial {
            query.update(cx, |q, cx| q.set_text(&text, cx));
        }
        window.focus(&query.focus_handle(cx));
        let subscription = cx.subscribe(&query, |this, _, _: &EditorEvent, cx| this.search(cx));
        let mut this = Self {
            root,
            query,
            replacement,
            replace_mode,
            options: SearchOptions::default(),
            overrides,
            matches: Arc::new(Vec::new()),
            files: 0,
            truncated: false,
            searching: false,
            error: None,
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            task: None,
            _subscriptions: vec![subscription],
        };
        this.search(cx);
        this
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        let query = match SearchQuery::new(&self.query.read(cx).text(), self.options) {
            Ok(Some(q)) => q,
            Ok(None) => {
                self.matches = Arc::new(Vec::new());
                self.files = 0;
                self.error = None;
                self.searching = false;
                self.task = None;
                cx.notify();
                return;
            }
            Err(e) => {
                self.error = Some(e);
                self.matches = Arc::new(Vec::new());
                self.task = None;
                cx.notify();
                return;
            }
        };
        self.error = None;
        self.searching = true;
        let root = self.root.clone();
        let overrides = self.overrides.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            // Debounce typing.
            cx.background_executor().timer(Duration::from_millis(120)).await;
            let results = cx
                .background_executor()
                .spawn(async move { search_project(&root, &query, &overrides) })
                .await;
            this.update(cx, |this, cx| {
                this.matches = Arc::new(results.matches);
                this.files = results.files;
                this.truncated = results.truncated;
                this.searching = false;
                this.selected = 0;
                this.scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = ix.min(self.matches.len() - 1);
        self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
        cx.notify();
    }

    fn open_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(m) = self.matches.get(self.selected) {
            cx.emit(ProjectSearchEvent::Open {
                path: m.path.clone(),
                range: m.range.clone(),
            });
        }
    }

    fn replace_all(&mut self, cx: &mut Context<Self>) {
        let Ok(Some(query)) = SearchQuery::new(&self.query.read(cx).text(), self.options) else {
            return;
        };
        let mut files: Vec<PathBuf> = self.matches.iter().map(|m| m.path.clone()).collect();
        files.dedup();
        cx.emit(ProjectSearchEvent::ReplaceAll {
            query,
            replacement: self.replacement.read(cx).text(),
            files,
        });
    }

    /// Re-runs the search after a replace, with fresh buffer contents.
    pub fn refresh(&mut self, overrides: HashMap<PathBuf, Arc<str>>, cx: &mut Context<Self>) {
        self.overrides = overrides;
        self.search(cx);
    }

    fn toggle_option(&mut self, f: impl FnOnce(&mut SearchOptions), cx: &mut Context<Self>) {
        f(&mut self.options);
        self.search(cx);
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.saturating_sub(1), cx);
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected + 1, cx);
    }

    fn on_page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.saturating_sub(10), cx);
    }

    fn on_page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected + 10, cx);
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.open_selected(cx);
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ProjectSearchEvent::Dismissed);
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

    fn render_preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let Some(m) = self.matches.get(self.selected) else {
            return div().flex_1().into_any_element();
        };
        const CONTEXT: usize = 6;
        let lines: Vec<&str> = m.contents.split('\n').collect();
        let first = m.line.saturating_sub(CONTEXT);
        let last = (m.line + CONTEXT + 1).min(lines.len());
        let mono = theme.mono_font.clone();
        div()
            .id("preview")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .py_2()
            .overflow_hidden()
            .font_family(mono)
            .text_size(px(12.5))
            .children((first..last).map(|row| {
                let text: SharedString = lines[row].replace('\t', "    ").into();
                let is_match = row == m.line;
                let styled = if is_match && m.line_range.end <= lines[row].len() && !lines[row].contains('\t') {
                    StyledText::new(text).with_highlights([(
                        m.line_range.clone(),
                        HighlightStyle {
                            background_color: Some(theme.search_active.into()),
                            color: Some(theme.text.into()),
                            ..Default::default()
                        },
                    )])
                } else {
                    StyledText::new(text)
                };
                div()
                    .flex()
                    .h(px(20.))
                    .when(is_match, |el| el.bg(theme.current_line))
                    .child(
                        div()
                            .w(px(52.))
                            .pr_3()
                            .flex_shrink_0()
                            .flex()
                            .justify_end()
                            .text_color(if is_match { theme.line_number_active } else { theme.line_number })
                            .child((row + 1).to_string()),
                    )
                    .child(div().text_color(theme.text_muted).whitespace_nowrap().child(styled))
            }))
            .into_any_element()
    }
}

impl Focusable for ProjectSearch {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for ProjectSearch {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let opts = self.options;
        let summary = if let Some(e) = &self.error {
            e.clone()
        } else if self.searching {
            "Searching…".into()
        } else if self.matches.is_empty() {
            if self.query.read(cx).doc.len() > 0 { "No matches".into() } else { String::new() }
        } else {
            format!(
                "{}{} matches in {} file{}",
                self.matches.len(),
                if self.truncated { "+" } else { "" },
                self.files,
                if self.files == 1 { "" } else { "s" }
            )
        };
        let title = if self.replace_mode { "Replace in Files" } else { "Find in Files" };
        let scope = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .pt_3()
            .pb_2()
            .child(icon_el(Icon::TextSearch, px(15.), theme.text_muted.into()))
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1p5()
                    .rounded(px(5.))
                    .bg(theme.hover)
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .child(icon_el(Icon::Folder, px(12.), theme.text_muted.into()))
                    .child(scope),
            )
            .child(div().flex_1().text_size(px(12.)).text_color(if self.error.is_some() { theme.error } else { theme.text_dim }).child(summary))
            .child(toggle("ps-case", "Cc", opts.case_sensitive, "Match Case", cx).on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| o.case_sensitive = !o.case_sensitive, cx))))
            .child(toggle("ps-word", "W", opts.whole_word, "Words", cx).on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| o.whole_word = !o.whole_word, cx))))
            .child(toggle("ps-regex", ".*", opts.regex, "Regex", cx).on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| o.regex = !o.regex, cx))));

        let inputs = div()
            .flex()
            .flex_col()
            .gap_1p5()
            .px_3()
            .pb_3()
            .child(input_box(&self.query, Some(Icon::Search), window, cx))
            .when(self.replace_mode, |el| {
                el.child(
                    div()
                        .flex()
                        .gap_1p5()
                        .child(input_box(&self.replacement, Some(Icon::Replace), window, cx))
                        .child(text_button("ps-replace-all", "Replace All", true, cx).on_click(cx.listener(|this, _, _, cx| this.replace_all(cx)))),
                )
            });

        let matches = self.matches.clone();
        let selected = self.selected;
        let list = uniform_list("project-results", matches.len(), {
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                let theme = Theme::get(cx).clone();
                range
                    .map(|ix| {
                        let m = &matches[ix];
                        // Trim leading whitespace and keep the match visible.
                        let raw = m.line_text.replace('\t', " ");
                        let indent = raw.len() - raw.trim_start().len();
                        let mut start = indent.min(m.line_range.start);
                        if m.line_range.start > start + 60 {
                            start = raw.floor_char_boundary(m.line_range.start - 30);
                        }
                        let shown = &raw[start..];
                        let shown_text: SharedString = if shown.len() > 200 {
                            shown[..shown.floor_char_boundary(200)].to_string().into()
                        } else {
                            shown.to_string().into()
                        };
                        let hl = (m.line_range.start - start).min(shown_text.len())..(m.line_range.end - start).min(shown_text.len());
                        let file_name = m.relative.rsplit(['/', '\\']).next().unwrap_or(&m.relative).to_string();
                        let icon = crate::ui::file_icon(&m.path, px(13.), &theme);
                        let is_selected = ix == selected;
                        div()
                            .id(ix)
                            .relative()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(26.))
                            .px_3()
                            .rounded(px(7.))
                            .cursor_pointer()
                            .when(is_selected, |el| el.bg(theme.accent_soft).child(crate::ui::selected_edge(&theme).top(px(5.)).bottom(px(5.))))
                            .when(!is_selected, |el| el.hover(|s| s.bg(theme.hover)))
                            .on_click(cx.listener(move |this, e: &gpui::ClickEvent, _, cx| {
                                this.select(ix, cx);
                                if e.click_count() >= 2 {
                                    this.open_selected(cx);
                                }
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(theme.mono_font.clone())
                                    .text_size(px(12.5))
                                    .text_color(theme.text_muted)
                                    .child(StyledText::new(shown_text).with_highlights([(
                                        hl,
                                        HighlightStyle {
                                            color: Some(theme.text.into()),
                                            background_color: Some(theme.search_match.into()),
                                            font_weight: Some(FontWeight::SEMIBOLD),
                                            ..Default::default()
                                        },
                                    )])),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .flex_shrink_0()
                                    .text_size(px(12.))
                                    .text_color(theme.text_dim)
                                    .child(icon)
                                    .child(format!("{file_name} {}", m.line + 1)),
                            )
                    })
                    .collect()
            })
        })
        .track_scroll(self.scroll.clone())
        .h(px(260.))
        .w_full();

        let path_line = self
            .matches
            .get(self.selected)
            .map(|m| m.relative.clone())
            .unwrap_or_default();

        modal_panel(cx)
            .key_context("ProjectSearch")
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_page_up))
            .on_action(cx.listener(Self::on_page_down))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_shift_tab))
            .w(px(860.))
            .max_w(window.viewport_size().width - px(48.))
            .h(px(640.))
            .max_h(window.viewport_size().height - px(120.))
            .child(header)
            .child(inputs)
            .child(crate::ui::island(&theme).mx(px(crate::ui::GAP)).child(list))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(30.))
                    .px_3p5()
                    .text_size(px(12.))
                    .text_color(theme.text_dim)
                    .child(div().flex_1().truncate().child(path_line))
                    .child(kbd("↩", cx))
                    .child("open"),
            )
            .child(
                crate::ui::island(&theme)
                    .flex_1()
                    .min_h_0()
                    .mx(px(crate::ui::GAP))
                    .mb(px(crate::ui::GAP))
                    .child(self.render_preview(cx)),
            )
    }
}
