//! Popups for jumping around: Search Everywhere (double Shift), Go to File,
//! Recent Files, Recent Projects, Find Action, File Structure and Go to Line.

use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::{
    Action, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle,
    Hsla, IntoElement, Render, ScrollStrategy, SharedString, StyledText, Subscription,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{
    assets::Icon,
    clone_dialog::display_path,
    lsp::protocol::{Encoding, Location},
    run::RunTarget,
    syntax::Symbol,
    editor::{Cancel, Editor, EditorEvent, MoveDown, MoveUp, Newline, PageDown, PageUp, Submit},
    theme::Theme,
    ui::{binding_label, icon_el, input_box, kbd, modal_panel},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    Everywhere,
    Files,
    Recent,
    /// Recently opened folders; `recent` holds them instead of files.
    Projects,
    Actions,
    /// The active file's symbols; see [`Picker::set_symbols`].
    Structure,
    /// Places in the project (usages, declarations); see [`Picker::set_locations`].
    Locations,
    /// Run configurations; see [`Picker::set_run_targets`].
    Run,
}

/// One place in a [`PickerMode::Locations`] list.
pub struct LocationRow {
    pub location: Location,
    /// The file, relative to the project.
    pub file: String,
    /// 1-based.
    pub line: usize,
    /// The line's text, trimmed.
    pub preview: String,
}

pub struct ActionEntry {
    pub name: &'static str,
    pub action: Box<dyn Action>,
}

pub enum PickerEvent {
    OpenFile(PathBuf),
    OpenProject { path: PathBuf, new_window: bool },
    RunAction(Box<dyn Action>),
    /// Jump to an offset in the active editor.
    GoTo(usize),
    OpenLocation { location: Location, encoding: Encoding },
    Run(RunTarget),
    Dismissed,
}

#[derive(Clone)]
enum Item {
    Header(SharedString),
    File {
        path: PathBuf,
        name: SharedString,
        dir: SharedString,
        name_hl: Vec<Range<usize>>,
        dir_hl: Vec<Range<usize>>,
    },
    Project {
        path: PathBuf,
        name: SharedString,
        dir: SharedString,
        name_hl: Vec<Range<usize>>,
        dir_hl: Vec<Range<usize>>,
    },
    Action {
        ix: usize,
        hl: Vec<Range<usize>>,
    },
    Symbol {
        ix: usize,
        hl: Vec<Range<usize>>,
        /// Indent by nesting; flat while filtering.
        nested: bool,
    },
    Location {
        ix: usize,
    },
    Run {
        ix: usize,
        hl: Vec<Range<usize>>,
    },
}

pub struct Picker {
    mode: PickerMode,
    query: Entity<Editor>,
    root: Option<PathBuf>,
    files: Arc<Vec<PathBuf>>,
    recent: Vec<PathBuf>,
    actions: Arc<Vec<ActionEntry>>,
    shortcuts: Arc<Vec<Option<String>>>,
    /// Symbols with their 1-based line numbers.
    symbols: Arc<Vec<(Symbol, usize)>>,
    /// The symbol around the caret, preselected like JetBrains.
    current_symbol: Option<usize>,
    locations: Arc<Vec<LocationRow>>,
    location_encoding: Encoding,
    run_targets: Arc<Vec<RunTarget>>,
    /// Overrides the mode's title (Locations: "Usages of foo").
    title: Option<SharedString>,
    items: Arc<Vec<Item>>,
    selected: usize,
    scroll: UniformListScrollHandle,
    matcher: Matcher,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PickerEvent> for Picker {}

const MAX_RESULTS: usize = 80;

/// Actions listed under Recent Projects, for folders not in the list yet.
const PROJECT_ACTIONS: [&str; 3] = ["Open Folder…", "Open Folder in This Window…", "New Window"];

impl Picker {
    pub fn new(
        mode: PickerMode,
        root: Option<PathBuf>,
        files: Arc<Vec<PathBuf>>,
        recent: Vec<PathBuf>,
        actions: Arc<Vec<ActionEntry>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = match mode {
            PickerMode::Everywhere => "Search files and actions",
            PickerMode::Files => "Go to file",
            PickerMode::Recent => "Search recent files",
            PickerMode::Projects => "Search recent projects",
            PickerMode::Actions => "Search actions",
            PickerMode::Structure => "Search symbols",
            PickerMode::Locations => "Filter",
            PickerMode::Run => "Run configuration",
        };
        let query = cx.new(|cx| Editor::single_line(placeholder, cx));
        window.focus(&query.focus_handle(cx));
        let shortcuts = Arc::new(actions.iter().map(|a| binding_label(a.action.as_ref(), window)).collect());
        let subscription = cx.subscribe(&query, |this, _, _: &EditorEvent, cx| this.update_items(cx));
        let mut this = Self {
            mode,
            query,
            root,
            files,
            recent,
            actions,
            shortcuts,
            symbols: Arc::new(Vec::new()),
            current_symbol: None,
            locations: Arc::new(Vec::new()),
            location_encoding: Encoding::Utf16,
            run_targets: Arc::new(Vec::new()),
            title: None,
            items: Arc::new(Vec::new()),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            matcher: Matcher::new({
                let mut config = Config::DEFAULT;
                config.set_match_paths();
                config
            }),
            _subscriptions: vec![subscription],
        };
        this.update_items(cx);
        this
    }

    pub fn mode(&self) -> PickerMode {
        self.mode
    }

    /// Called when the background file scan finishes.
    pub fn set_files(&mut self, files: Arc<Vec<PathBuf>>, cx: &mut Context<Self>) {
        self.files = files;
        self.update_items(cx);
    }

    /// Fills File Structure; `cursor` picks the symbol to preselect.
    pub fn set_symbols(&mut self, symbols: Vec<(Symbol, usize)>, cursor: usize, cx: &mut Context<Self>) {
        self.current_symbol = symbols.iter().rposition(|(s, _)| s.offset <= cursor);
        self.symbols = Arc::new(symbols);
        self.update_items(cx);
    }

    /// Fills a Run picker.
    pub fn set_run_targets(&mut self, targets: Vec<RunTarget>, cx: &mut Context<Self>) {
        self.run_targets = Arc::new(targets);
        self.update_items(cx);
    }

    /// Fills a Locations picker.
    pub fn set_locations(&mut self, title: SharedString, rows: Vec<LocationRow>, encoding: Encoding, cx: &mut Context<Self>) {
        self.title = Some(title);
        self.locations = Arc::new(rows);
        self.location_encoding = encoding;
        self.update_items(cx);
    }

    fn relative(&self, path: &Path) -> String {
        self.root
            .as_deref()
            .and_then(|r| path.strip_prefix(r).ok())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    fn file_item(&mut self, path: &Path, pattern: Option<&Pattern>) -> Option<(u32, Item)> {
        let rel = self.relative(path);
        let (score, name, dir, name_hl, dir_hl) = self.match_path(&rel, pattern)?;
        // Prefer matches in the file name itself.
        let bonus = if !name_hl.is_empty() { 20 } else { 0 };
        Some((score + bonus, Item::File { path: path.to_path_buf(), name, dir, name_hl, dir_hl }))
    }

    /// A recent project, matched on its folder name and `~`-relative location.
    fn project_item(&mut self, path: &Path, pattern: Option<&Pattern>) -> Option<(u32, Item)> {
        let (score, name, dir, name_hl, dir_hl) = self.match_path(&display_path(path), pattern)?;
        let bonus = if !name_hl.is_empty() { 20 } else { 0 };
        Some((score + bonus, Item::Project { path: path.to_path_buf(), name, dir, name_hl, dir_hl }))
    }

    /// Fuzzy-matches a path, split into its last component and the rest
    /// with the matched byte ranges of each.
    #[allow(clippy::type_complexity)]
    fn match_path(
        &mut self,
        rel: &str,
        pattern: Option<&Pattern>,
    ) -> Option<(u32, SharedString, SharedString, Vec<Range<usize>>, Vec<Range<usize>>)> {
        let (score, indices) = match pattern {
            Some(p) => {
                let mut buf = Vec::new();
                let mut indices = Vec::new();
                let score = p.indices(Utf32Str::new(rel, &mut buf), &mut self.matcher, &mut indices)?;
                indices.sort_unstable();
                indices.dedup();
                (score, indices)
            }
            None => (0, Vec::new()),
        };
        let split = rel.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
        let name = rel[split..].to_string();
        let dir = rel[..split].trim_end_matches(['/', '\\']).to_string();
        let split_chars = rel[..split].chars().count() as u32;
        let to_ranges = |s: &str, ix: &mut dyn Iterator<Item = u32>| -> Vec<Range<usize>> {
            let offsets: Vec<usize> = s.char_indices().map(|(b, _)| b).collect();
            ix.filter_map(|c| {
                let b = *offsets.get(c as usize)?;
                Some(b..b + s[b..].chars().next()?.len_utf8())
            })
            .collect()
        };
        let name_hl = to_ranges(&name, &mut indices.iter().filter(|&&i| i >= split_chars).map(|i| i - split_chars));
        let dir_hl = to_ranges(&dir, &mut indices.iter().copied().filter(|&i| i < split_chars));
        Some((score, name.into(), dir.into(), name_hl, dir_hl))
    }

    fn action_item(&mut self, ix: usize, pattern: Option<&Pattern>) -> Option<(u32, Item)> {
        let name = self.actions[ix].name;
        let Some(p) = pattern else {
            return Some((0, Item::Action { ix, hl: Vec::new() }));
        };
        let mut buf = Vec::new();
        let mut indices = Vec::new();
        let score = p.indices(Utf32Str::new(name, &mut buf), &mut self.matcher, &mut indices)?;
        let hl = indices
            .into_iter()
            .filter_map(|c| name.char_indices().nth(c as usize).map(|(b, ch)| b..b + ch.len_utf8()))
            .collect();
        Some((score, Item::Action { ix, hl }))
    }

    fn symbol_item(&mut self, ix: usize, pattern: Option<&Pattern>) -> Option<(u32, Item)> {
        let symbols = self.symbols.clone();
        let name = symbols[ix].0.name.as_str();
        let Some(p) = pattern else {
            return Some((0, Item::Symbol { ix, hl: Vec::new(), nested: true }));
        };
        let mut buf = Vec::new();
        let mut indices = Vec::new();
        let score = p.indices(Utf32Str::new(name, &mut buf), &mut self.matcher, &mut indices)?;
        let hl = indices
            .into_iter()
            .filter_map(|c| name.char_indices().nth(c as usize).map(|(b, ch)| b..b + ch.len_utf8()))
            .collect();
        Some((score, Item::Symbol { ix, hl, nested: false }))
    }

    fn update_items(&mut self, cx: &mut Context<Self>) {
        let text = self.query.read(cx).text();
        let pattern = (!text.trim().is_empty()).then(|| Pattern::parse(&text, CaseMatching::Smart, Normalization::Smart));
        let mut items = Vec::new();

        let wants_files = matches!(self.mode, PickerMode::Everywhere | PickerMode::Files | PickerMode::Recent);
        if wants_files {
            let mut scored: Vec<(u32, Item)> = Vec::new();
            let recent = self.recent.clone();
            if self.mode == PickerMode::Recent || pattern.is_none() {
                // Recent files first, in most-recently-used order.
                for p in &recent {
                    if let Some(item) = self.file_item(p, pattern.as_ref()) {
                        scored.push(item);
                    }
                }
            }
            if self.mode != PickerMode::Recent {
                let files = self.files.clone();
                let mut matched: Vec<(u32, Item)> = files
                    .iter()
                    .filter(|p| pattern.is_some() || !recent.contains(p))
                    .filter_map(|p| self.file_item(p, pattern.as_ref()))
                    .collect();
                if pattern.is_some() {
                    matched.sort_by(|a, b| b.0.cmp(&a.0));
                    // Recently used files win ties.
                    scored.clear();
                }
                scored.extend(matched);
            }
            let limit = if self.mode == PickerMode::Everywhere { 40 } else { MAX_RESULTS };
            if self.mode == PickerMode::Everywhere && !scored.is_empty() {
                items.push(Item::Header("Files".into()));
            }
            items.extend(scored.into_iter().take(limit).map(|(_, i)| i));
        }

        if self.mode == PickerMode::Projects {
            let mut scored: Vec<(u32, Item)> = Vec::new();
            for p in self.recent.clone() {
                // The window's own project is already open.
                if self.root.as_deref() == Some(p.as_path()) {
                    continue;
                }
                if let Some(item) = self.project_item(&p, pattern.as_ref()) {
                    scored.push(item);
                }
            }
            if pattern.is_some() {
                // Stable, so recency breaks ties.
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            items.extend(scored.into_iter().map(|(_, i)| i));
            let actions = self.actions.clone();
            let open: Vec<Item> = PROJECT_ACTIONS
                .iter()
                .filter_map(|name| actions.iter().position(|a| a.name == *name))
                .filter_map(|ix| self.action_item(ix, pattern.as_ref()))
                .map(|(_, i)| i)
                .collect();
            if !open.is_empty() && !items.is_empty() {
                items.push(Item::Header("Open".into()));
            }
            items.extend(open);
        }

        if matches!(self.mode, PickerMode::Everywhere | PickerMode::Actions) {
            let mut scored: Vec<(u32, Item)> =
                (0..self.actions.len()).filter_map(|ix| self.action_item(ix, pattern.as_ref())).collect();
            if pattern.is_some() {
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            let limit = if self.mode == PickerMode::Everywhere {
                if pattern.is_some() { 12 } else { 6 }
            } else {
                MAX_RESULTS
            };
            if self.mode == PickerMode::Everywhere && !scored.is_empty() {
                items.push(Item::Header("Actions".into()));
            }
            items.extend(scored.into_iter().take(limit).map(|(_, i)| i));
        }

        if self.mode == PickerMode::Run {
            let targets = self.run_targets.clone();
            let mut scored: Vec<(u32, Item)> = Vec::new();
            for (ix, target) in targets.iter().enumerate() {
                match &pattern {
                    Some(p) => {
                        let mut buf = Vec::new();
                        let mut indices = Vec::new();
                        let Some(score) = p.indices(Utf32Str::new(&target.name, &mut buf), &mut self.matcher, &mut indices) else {
                            continue;
                        };
                        let hl = indices
                            .into_iter()
                            .filter_map(|c| target.name.char_indices().nth(c as usize).map(|(b, ch)| b..b + ch.len_utf8()))
                            .collect();
                        scored.push((score, Item::Run { ix, hl }));
                    }
                    None => scored.push((0, Item::Run { ix, hl: Vec::new() })),
                }
            }
            if pattern.is_some() {
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            items.extend(scored.into_iter().map(|(_, i)| i));
        }

        if self.mode == PickerMode::Locations {
            let locations = self.locations.clone();
            let mut scored: Vec<(u32, Item)> = Vec::new();
            for (ix, row) in locations.iter().enumerate() {
                let score = match &pattern {
                    Some(p) => {
                        let text = format!("{} {}", row.file, row.preview);
                        let mut buf = Vec::new();
                        match p.score(Utf32Str::new(&text, &mut buf), &mut self.matcher) {
                            Some(score) => score,
                            None => continue,
                        }
                    }
                    None => 0,
                };
                scored.push((score, Item::Location { ix }));
            }
            if pattern.is_some() {
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            items.extend(scored.into_iter().map(|(_, i)| i));
        }

        if self.mode == PickerMode::Structure {
            let mut scored: Vec<(u32, Item)> =
                (0..self.symbols.len()).filter_map(|ix| self.symbol_item(ix, pattern.as_ref())).collect();
            if pattern.is_some() {
                // Stable, so document order breaks ties.
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            items.extend(scored.into_iter().map(|(_, i)| i));
        }

        self.items = Arc::new(items);
        // Like JetBrains, Recent Files preselects the previous file and File
        // Structure the symbol at the caret.
        self.selected = match self.mode {
            PickerMode::Recent if pattern.is_none() && self.items.len() > 1 => 1,
            PickerMode::Structure if pattern.is_none() => self.current_symbol.unwrap_or(0),
            _ => 0,
        };
        self.move_selection(0, cx);
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let selectable: Vec<usize> = (0..self.items.len())
            .filter(|&i| !matches!(self.items[i], Item::Header(_)))
            .collect();
        if let Some(last) = selectable.len().checked_sub(1) {
            let pos = selectable.iter().position(|&i| i == self.selected).unwrap_or(0) as isize;
            self.selected = selectable[(pos + delta).clamp(0, last as isize) as usize];
            self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
        }
        cx.notify();
    }

    /// `secondary` (⌘/Ctrl held) opens a project in a new window.
    fn confirm(&mut self, ix: usize, secondary: bool, cx: &mut Context<Self>) {
        match self.items.get(ix) {
            Some(Item::File { path, .. }) => cx.emit(PickerEvent::OpenFile(path.clone())),
            Some(Item::Project { path, .. }) => {
                cx.emit(PickerEvent::OpenProject { path: path.clone(), new_window: secondary })
            }
            Some(Item::Action { ix, .. }) => cx.emit(PickerEvent::RunAction(self.actions[*ix].action.boxed_clone())),
            Some(Item::Symbol { ix, .. }) => cx.emit(PickerEvent::GoTo(self.symbols[*ix].0.offset)),
            Some(Item::Run { ix, .. }) => cx.emit(PickerEvent::Run(self.run_targets[*ix].clone())),
            Some(Item::Location { ix }) => cx.emit(PickerEvent::OpenLocation {
                location: self.locations[*ix].location.clone(),
                encoding: self.location_encoding,
            }),
            _ => {}
        }
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn on_page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-10, cx);
    }

    fn on_page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(10, cx);
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.confirm(self.selected, false, cx);
    }

    fn on_submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        self.confirm(self.selected, true, cx);
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PickerEvent::Dismissed);
    }
}

impl Focusable for Picker {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

fn highlighted(text: SharedString, ranges: &[Range<usize>], color: Hsla) -> StyledText {
    StyledText::new(text).with_highlights(ranges.iter().map(|r| {
        (
            r.clone(),
            HighlightStyle {
                color: Some(color),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        )
    }))
}

impl Render for Picker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let (title, icon, hint) = match self.mode {
            PickerMode::Everywhere => ("Search Everywhere", Icon::Search, if cfg!(target_os = "macos") { "Double ⇧" } else { "Double Shift" }),
            PickerMode::Files => ("Files", Icon::File, ""),
            PickerMode::Recent => ("Recent Files", Icon::History, ""),
            PickerMode::Projects => (
                "Recent Projects",
                Icon::FolderOpen,
                if cfg!(target_os = "macos") { "⌘↩ opens in a new window" } else { "Ctrl+Enter opens in a new window" },
            ),
            PickerMode::Actions => ("Actions", Icon::Command, ""),
            PickerMode::Structure => ("File Structure", Icon::Braces, ""),
            PickerMode::Locations => ("", Icon::Search, ""),
            PickerMode::Run => ("Run", Icon::Play, ""),
        };
        let title: SharedString = match &self.title {
            Some(t) => t.clone(),
            None => title.into(),
        };
        let hint = if self.mode == PickerMode::Locations { format!("{} found", self.locations.len()) } else { hint.to_string() };
        let items = self.items.clone();
        let actions = self.actions.clone();
        let symbols = self.symbols.clone();
        let locations = self.locations.clone();
        let run_targets = self.run_targets.clone();
        let shortcuts = self.shortcuts.clone();
        let selected = self.selected;
        let count = items.len();

        let list = uniform_list("picker-items", count, {
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                let theme = Theme::get(cx).clone();
                range
                    .map(|ix| {
                        let row = div().id(ix).w_full().flex().items_center().gap_2().h(px(30.)).px_2().rounded(px(6.));
                        let row = match &items[ix] {
                            Item::Header(label) => row
                                .pt_1()
                                .text_size(px(11.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.text_dim)
                                .child(label.to_uppercase()),
                            item => {
                                let is_selected = ix == selected;
                                let row = row
                                    .cursor_pointer()
                                    .when(is_selected, |el| el.bg(theme.accent_soft))
                                    .when(!is_selected, |el| el.hover(|s| s.bg(theme.hover)))
                                    .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, _, cx| {
                                        this.confirm(ix, ev.modifiers().secondary(), cx)
                                    }));
                                match item {
                                    Item::File { path, name, dir, name_hl, dir_hl } => {
                                        row.child(crate::ui::file_icon(path, px(16.), &theme))
                                            .child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_color(theme.text)
                                                    .child(highlighted(name.clone(), name_hl, theme.accent.into())),
                                            )
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_size(px(12.))
                                                    .text_color(theme.text_dim)
                                                    .child(highlighted(dir.clone(), dir_hl, theme.text_muted.into())),
                                            )
                                    }
                                    Item::Project { name, dir, name_hl, dir_hl, .. } => row
                                        .child(crate::ui::folder_icon(false, px(16.), &theme))
                                        .child(
                                            div()
                                                .flex_shrink_0()
                                                .text_color(theme.text)
                                                .child(highlighted(name.clone(), name_hl, theme.accent.into())),
                                        )
                                        .child(
                                            div()
                                                .min_w_0()
                                                .truncate()
                                                .text_size(px(12.))
                                                .text_color(theme.text_dim)
                                                .child(highlighted(dir.clone(), dir_hl, theme.text_muted.into())),
                                        ),
                                    Item::Action { ix, hl } => row
                                        .child(icon_el(Icon::Command, px(14.), theme.text_muted.into()))
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_color(theme.text)
                                                .child(highlighted(actions[*ix].name.into(), hl, theme.accent.into())),
                                        )
                                        .when_some(shortcuts[*ix].clone(), |el, s| el.child(kbd(s, cx))),
                                    Item::Symbol { ix, hl, nested } => {
                                        let (symbol, line) = &symbols[*ix];
                                        let indent = if *nested { symbol.depth.min(8) as f32 * 16. } else { 0. };
                                        row.child(div().w(px(indent)).flex_shrink_0())
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_color(theme.text)
                                                    .child(highlighted(symbol.name.clone().into(), hl, theme.accent.into())),
                                            )
                                            .child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.text_dim).child(symbol.kind))
                                            .child(div().flex_1())
                                            .child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_size(px(12.))
                                                    .text_color(theme.text_dim)
                                                    .child(format!(":{line}")),
                                            )
                                    }
                                    Item::Run { ix, hl } => {
                                        let target = &run_targets[*ix];
                                        row.child(icon_el(Icon::Play, px(13.), theme.success.into()))
                                            .child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_color(theme.text)
                                                    .child(highlighted(target.name.clone().into(), hl, theme.accent.into())),
                                            )
                                            .when(target.command != target.name, |row| {
                                                row.child(
                                                    div()
                                                        .min_w_0()
                                                        .truncate()
                                                        .text_size(px(12.))
                                                        .text_color(theme.text_dim)
                                                        .child(target.command.clone()),
                                                )
                                            })
                                    }
                                    Item::Location { ix } => {
                                        let row_data = &locations[*ix];
                                        row.child(icon_el(Icon::FileCode, px(14.), theme.text_muted.into()))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_color(theme.text)
                                                    .child(row_data.preview.clone()),
                                            )
                                            .child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_size(px(12.))
                                                    .text_color(theme.text_dim)
                                                    .child(format!("{}:{}", row_data.file, row_data.line)),
                                            )
                                    }
                                    Item::Header(_) => unreachable!(),
                                }
                            }
                        };
                        div().w_full().px_1p5().child(row)
                    })
                    .collect()
            })
        })
        .track_scroll(self.scroll.clone())
        .h(px((count.clamp(1, 12) * 30 + 8) as f32))
        .w_full()
        .py_1();

        modal_panel(cx)
            .key_context("Picker")
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_page_up))
            .on_action(cx.listener(Self::on_page_down))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_submit))
            .on_action(cx.listener(Self::on_escape))
            .w(px(620.))
            .max_w(window.viewport_size().width - px(48.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .pt_2p5()
                    .pb_1p5()
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text_muted)
                            .child(icon_el(icon, px(13.), theme.text_muted.into()))
                            .child(title),
                    )
                    .child(div().text_color(theme.text_dim).child(hint)),
            )
            .child(div().px_3().pb_2().child(input_box(&self.query, None, window, cx)))
            .child(div().border_t_1().border_color(theme.border).child(if count == 0 {
                div()
                    .h(px(38.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme.text_dim)
                    .child("Nothing found")
                    .into_any_element()
            } else {
                list.into_any_element()
            }))
    }
}

// ---- Go to Line ----------------------------------------------------------------

pub enum GoToLineEvent {
    Confirm { line: usize, column: usize },
    Dismissed,
}

pub struct GoToLine {
    input: Entity<Editor>,
    line_count: usize,
    current: (usize, usize),
}

impl EventEmitter<GoToLineEvent> for GoToLine {}

impl GoToLine {
    pub fn new(line_count: usize, current: (usize, usize), window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| Editor::single_line(format!("{}:{}", current.0, current.1), cx));
        window.focus(&input.focus_handle(cx));
        cx.subscribe(&input, |_, _, _: &EditorEvent, cx| cx.notify()).detach();
        Self { input, line_count, current }
    }

    fn parse(&self, cx: &gpui::App) -> Option<(usize, usize)> {
        let text = self.input.read(cx).text();
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let mut parts = text.splitn(2, [':', ',']);
        let line = parts.next()?.trim().parse::<usize>().ok()?;
        let column = match parts.next() {
            Some(c) if !c.trim().is_empty() => c.trim().parse::<usize>().ok()?,
            _ => 1,
        };
        Some((line.clamp(1, self.line_count.max(1)), column.max(1)))
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((line, column)) = self.parse(cx) {
            cx.emit(GoToLineEvent::Confirm { line, column });
        }
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(GoToLineEvent::Dismissed);
    }
}

impl Focusable for GoToLine {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for GoToLine {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let valid = self.input.read(cx).doc.len() == 0 || self.parse(cx).is_some();
        modal_panel(cx)
            .key_context("GoToLine")
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_escape))
            .w(px(380.))
            .p_3()
            .gap_2()
            .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text_muted).child("Go to Line:Column"))
            .child(input_box(&self.input, Some(Icon::Hash), window, cx))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(if valid { theme.text_dim } else { theme.error })
                    .child(if valid {
                        format!(
                            "Current position {}:{} · {} lines",
                            self.current.0, self.current.1, self.line_count
                        )
                    } else {
                        "Enter a line number, optionally followed by :column".into()
                    }),
            )
    }
}
