//! Preferences (JetBrains' Settings dialog): every option in `settings.json`
//! and the key bindings in `keymap.json`, on pages with a search across all of
//! them. It looks the same on every platform. Changes apply and save right
//! away; the files stay the source of truth and keep their comments.

mod keymap_page;

use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, Bounds, ClickEvent, Corner, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    IntoElement, Pixels, Render, ScrollHandle, SharedString, Stateful, Subscription, Task, Window,
    anchored, canvas, div, point, prelude::*, px, uniform_list,
};
use serde_json::{Value, json};

use crate::{
    assets::Icon,
    editor::{Cancel, Editor, EditorEvent, MoveDown, MoveUp},
    format,
    lsp::servers,
    settings::{self, AutoSave, RunConfiguration, Settings},
    syntax::Language,
    theme::{self, Appearance, Theme, ThemeRegistry},
    workspace::Find,
    ui::{icon_button, icon_el, input_box, modal_panel, switch, text_button, tooltip_view},
};

use keymap_page::KeymapPage;

pub enum PreferencesEvent {
    /// Show `settings.json` / `keymap.json` in a tab (Preferences closes).
    OpenSettingsFile,
    OpenKeymapFile,
    /// Problems found while applying a change, for the workspace to report.
    Problems(Vec<String>),
    Dismissed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Appearance,
    Editor,
    Saving,
    Languages,
    Keymap,
    Terminal,
}

impl Page {
    const ALL: [Page; 6] = [Page::Appearance, Page::Editor, Page::Saving, Page::Languages, Page::Keymap, Page::Terminal];

    fn title(self) -> &'static str {
        match self {
            Page::Appearance => "Appearance",
            Page::Editor => "Editor",
            Page::Saving => "Saving",
            Page::Languages => "Languages",
            Page::Keymap => "Keymap",
            Page::Terminal => "Terminal & Run",
        }
    }

    fn summary(self) -> &'static str {
        match self {
            Page::Appearance => "Color themes for the whole window",
            Page::Editor => "Font, indentation and line wrapping",
            Page::Saving => "When files are saved and what saving does",
            Page::Languages => "Per-language options, language servers and formatters",
            Page::Keymap => "Shortcuts for every action, on top of the JetBrains keymap",
            Page::Terminal => "The terminal's shell and your own Run configurations",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Page::Appearance => Icon::Palette,
            Page::Editor => Icon::Type,
            Page::Saving => Icon::Save,
            Page::Languages => Icon::Braces,
            Page::Keymap => Icon::Keyboard,
            Page::Terminal => Icon::Terminal,
        }
    }
}

/// Options with a row of their own. The Languages and Keymap pages list
/// languages and actions instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Setting {
    Theme,
    FontFamily,
    FontSize,
    LineHeight,
    TabSize,
    HardTabs,
    SoftWrap,
    AutoSave,
    AutoSaveDelay,
    FormatOnSave,
    TrimWhitespace,
    FinalNewline,
    Shell,
    RunConfigurations,
}

impl Setting {
    const ALL: [Setting; 14] = [
        Setting::Theme,
        Setting::FontFamily,
        Setting::FontSize,
        Setting::LineHeight,
        Setting::TabSize,
        Setting::HardTabs,
        Setting::SoftWrap,
        Setting::AutoSave,
        Setting::AutoSaveDelay,
        Setting::FormatOnSave,
        Setting::TrimWhitespace,
        Setting::FinalNewline,
        Setting::Shell,
        Setting::RunConfigurations,
    ];

    fn page(self) -> Page {
        match self {
            Setting::Theme => Page::Appearance,
            Setting::FontFamily | Setting::FontSize | Setting::LineHeight => Page::Editor,
            Setting::TabSize | Setting::HardTabs | Setting::SoftWrap => Page::Editor,
            Setting::AutoSave
            | Setting::AutoSaveDelay
            | Setting::FormatOnSave
            | Setting::TrimWhitespace
            | Setting::FinalNewline => Page::Saving,
            Setting::Shell | Setting::RunConfigurations => Page::Terminal,
        }
    }

    fn section(self) -> &'static str {
        match self {
            Setting::Theme => "Theme",
            Setting::FontFamily | Setting::FontSize | Setting::LineHeight => "Font",
            Setting::TabSize | Setting::HardTabs => "Indentation",
            Setting::SoftWrap => "Wrapping",
            Setting::AutoSave | Setting::AutoSaveDelay => "Auto-save",
            Setting::FormatOnSave | Setting::TrimWhitespace | Setting::FinalNewline => "On save",
            Setting::Shell => "Terminal",
            Setting::RunConfigurations => "Run configurations",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Setting::Theme => "Theme",
            Setting::FontFamily => "Family",
            Setting::FontSize => "Size",
            Setting::LineHeight => "Line height",
            Setting::TabSize => "Tab size",
            Setting::HardTabs => "Indent with tabs",
            Setting::SoftWrap => "Soft wrap",
            Setting::AutoSave => "Auto-save",
            Setting::AutoSaveDelay => "Delay",
            Setting::FormatOnSave => "Reformat on save",
            Setting::TrimWhitespace => "Trim trailing whitespace",
            Setting::FinalNewline => "Insert final newline",
            Setting::Shell => "Shell",
            Setting::RunConfigurations => "Run configurations",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Setting::Theme => "Add your own themes as .json files in the themes folder.",
            Setting::FontFamily => "Monospaced fonts installed on this computer.",
            Setting::FontSize => "Zoom (Increase / Decrease Font Size) adds to this until you reset it.",
            Setting::LineHeight => "A multiple of the font size.",
            Setting::TabSize => "Columns per indent level and per tab. A file's own indentation and .editorconfig come first.",
            Setting::HardTabs => "Use tab characters when a file doesn't show a style of its own.",
            Setting::SoftWrap => "Wrap long lines at the edge of the editor. Markdown and plain text wrap either way.",
            Setting::AutoSave => "Every mode but Off also saves when Kin quits or a tab closes.",
            Setting::AutoSaveDelay => "How long after you stop typing.",
            Setting::FormatOnSave => "Reformat Code when you Save; auto-save never reformats while you type.",
            Setting::TrimWhitespace => "Remove spaces at the ends of lines, except on lines with a caret.",
            Setting::FinalNewline => "End every file with a line break.",
            Setting::Shell => "Program and arguments for new terminals. Empty uses your login shell.",
            Setting::RunConfigurations => "Commands for Run… next to the detected ones (cargo, npm scripts, go, make, pytest).",
        }
    }

    /// More words people may search for.
    fn keywords(self) -> &'static str {
        match self {
            Setting::Theme => "color scheme dark light appearance",
            Setting::FontFamily => "typeface family monospace",
            Setting::FontSize => "font zoom text",
            Setting::LineHeight => "spacing leading font",
            Setting::TabSize => "indent width spaces",
            Setting::HardTabs => "indentation spaces hard tabs",
            Setting::SoftWrap => "word wrap lines",
            Setting::AutoSave => "autosave save automatically",
            Setting::AutoSaveDelay => "autosave milliseconds idle",
            Setting::FormatOnSave => "format prettier formatter",
            Setting::TrimWhitespace => "spaces trailing whitespace",
            Setting::FinalNewline => "newline end of file eof",
            Setting::Shell => "terminal bash zsh fish powershell",
            Setting::RunConfigurations => "run tasks commands scripts",
        }
    }

    fn matches(self, words: &[String]) -> bool {
        let haystack = format!(
            "{} {} {} {} {}",
            self.label(),
            self.description(),
            self.section(),
            self.page().title(),
            self.keywords()
        )
        .to_lowercase();
        words.iter().all(|w| haystack.contains(w.as_str()))
    }
}

/// Which dropdown is open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    AutoSave,
    FontFamily,
    LanguageServer,
    Formatter,
}

/// How a language picks its server or formatter (`language_servers` and
/// `formatters` in settings).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ToolMode {
    Automatic,
    LanguageServer,
    Off,
    Custom,
}

impl ToolMode {
    fn of(value: Option<&Value>) -> ToolMode {
        match value {
            Some(Value::Bool(false)) => ToolMode::Off,
            Some(Value::Array(_)) => ToolMode::Custom,
            Some(Value::String(s)) if s == "language_server" => ToolMode::LanguageServer,
            _ => ToolMode::Automatic,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ToolMode::Automatic => "Automatic",
            ToolMode::LanguageServer => "Language server",
            ToolMode::Off => "Off",
            ToolMode::Custom => "Custom command",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Server,
    Formatter,
}

impl Tool {
    fn key(self) -> &'static str {
        match self {
            Tool::Server => "language_servers",
            Tool::Formatter => "formatters",
        }
    }
}

/// An editor option that languages can override (`languages.<Name>`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum LanguageOption {
    TabSize,
    HardTabs,
    SoftWrap,
    TrimWhitespace,
    FinalNewline,
    FormatOnSave,
}

impl LanguageOption {
    const ALL: [LanguageOption; 6] = [
        LanguageOption::TabSize,
        LanguageOption::HardTabs,
        LanguageOption::SoftWrap,
        LanguageOption::TrimWhitespace,
        LanguageOption::FinalNewline,
        LanguageOption::FormatOnSave,
    ];

    fn key(self) -> &'static str {
        match self {
            LanguageOption::TabSize => "tab_size",
            LanguageOption::HardTabs => "hard_tabs",
            LanguageOption::SoftWrap => "soft_wrap",
            LanguageOption::TrimWhitespace => "trim_trailing_whitespace",
            LanguageOption::FinalNewline => "insert_final_newline",
            LanguageOption::FormatOnSave => "format_on_save",
        }
    }

    fn label(self) -> &'static str {
        match self {
            LanguageOption::TabSize => "Tab size",
            LanguageOption::HardTabs => "Indent with tabs",
            LanguageOption::SoftWrap => "Soft wrap",
            LanguageOption::TrimWhitespace => "Trim trailing whitespace",
            LanguageOption::FinalNewline => "Insert final newline",
            LanguageOption::FormatOnSave => "Reformat on save",
        }
    }

    fn is_set(self, s: &settings::EditorSettings) -> bool {
        match self {
            LanguageOption::TabSize => s.tab_size.is_some(),
            LanguageOption::HardTabs => s.hard_tabs.is_some(),
            LanguageOption::SoftWrap => s.soft_wrap.is_some(),
            LanguageOption::TrimWhitespace => s.trim_trailing_whitespace.is_some(),
            LanguageOption::FinalNewline => s.insert_final_newline.is_some(),
            LanguageOption::FormatOnSave => s.format_on_save.is_some(),
        }
    }

    fn clear(self, s: &mut settings::EditorSettings) {
        match self {
            LanguageOption::TabSize => s.tab_size = None,
            LanguageOption::HardTabs => s.hard_tabs = None,
            LanguageOption::SoftWrap => s.soft_wrap = None,
            LanguageOption::TrimWhitespace => s.trim_trailing_whitespace = None,
            LanguageOption::FinalNewline => s.insert_final_newline = None,
            LanguageOption::FormatOnSave => s.format_on_save = None,
        }
    }

    fn flag(self, f: &settings::FileSettings) -> bool {
        match self {
            LanguageOption::TabSize => false,
            LanguageOption::HardTabs => f.hard_tabs,
            LanguageOption::SoftWrap => f.soft_wrap,
            LanguageOption::TrimWhitespace => f.trim_trailing_whitespace,
            LanguageOption::FinalNewline => f.insert_final_newline,
            LanguageOption::FormatOnSave => f.format_on_save,
        }
    }
}

struct RunRow {
    name: Entity<Editor>,
    command: Entity<Editor>,
    cwd: Entity<Editor>,
}

const DEFAULT_TAB_SIZE: usize = 4;
const DEFAULT_AUTO_SAVE_DELAY: u64 = 1000;
const ROW_HEIGHT: f32 = 52.;

pub struct Preferences {
    focus_handle: FocusHandle,
    page: Page,
    search: Entity<Editor>,
    scroll: ScrollHandle,
    menu: Option<Menu>,
    font_filter: Entity<Editor>,
    /// Monospaced fonts, found when the font menu first opens.
    fonts: Option<Arc<Vec<SharedString>>>,
    fonts_task: Option<Task<()>>,
    language: Language,
    shell: Entity<Editor>,
    server_command: Entity<Editor>,
    formatter_command: Entity<Editor>,
    /// "Custom command" was picked for this tool but there's no command yet.
    pending_custom: Option<Tool>,
    runs: Vec<RunRow>,
    keymap: Entity<KeymapPage>,
    /// Why `settings.json` can't be changed right now.
    file_error: Option<String>,
    /// Writes typed text a moment after typing stops.
    commit_task: Option<Task<()>>,
    /// Where each dropdown's button was last drawn (by `Menu`), and the open
    /// one's list for this frame.
    menu_anchors: [Rc<Cell<Bounds<Pixels>>>; 4],
    overlay: Option<AnyElement>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PreferencesEvent> for Preferences {}

impl Preferences {
    pub fn new(page: Page, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| Editor::single_line("Search settings", cx));
        window.focus(&search.focus_handle(cx));
        let font_filter = cx.new(|cx| Editor::single_line("Filter fonts", cx));
        let login_shell = std::env::var("SHELL").unwrap_or_else(|_| "login shell".into());
        let shell = cx.new(|cx| Editor::single_line(format!("{login_shell} (login shell)"), cx));
        let server_command = cx.new(|cx| Editor::single_line("Command and arguments", cx));
        let formatter_command = cx.new(|cx| Editor::single_line("Command and arguments ({file} is the file's path)", cx));
        let keymap = cx.new(|cx| KeymapPage::new(window, cx));
        let mut subscriptions = vec![
            cx.subscribe(&search, |this, _, _: &EditorEvent, cx| {
                this.menu = None;
                this.scroll.set_offset(point(px(0.), px(0.)));
                cx.notify();
            }),
            cx.subscribe(&font_filter, |_, _, _: &EditorEvent, cx| cx.notify()),
            cx.subscribe(&keymap, |_, _, event: &PreferencesEvent, cx| cx.emit(clone_event(event))),
        ];
        for field in [&shell, &server_command, &formatter_command] {
            subscriptions.push(cx.subscribe(field, |this, _, _: &EditorEvent, cx| this.schedule_commit(cx)));
        }
        let settings = Settings::get(cx).clone();
        set_field(&shell, &settings.terminal_shell.as_deref().map(join_command).unwrap_or_default(), cx);
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            page,
            search,
            scroll: ScrollHandle::new(),
            menu: None,
            font_filter,
            fonts: None,
            fonts_task: None,
            language: Language::JavaScript,
            shell,
            server_command,
            formatter_command,
            pending_custom: None,
            runs: Vec::new(),
            keymap,
            file_error: settings_file_error(),
            commit_task: None,
            menu_anchors: Default::default(),
            overlay: None,
            _subscriptions: subscriptions,
        };
        for run in &settings.run_configurations {
            this.add_run_row(run, cx);
        }
        this.load_language_fields(cx);
        this
    }

    pub fn page(&self) -> Page {
        self.page
    }

    fn query_words(&self, cx: &App) -> Vec<String> {
        self.search.read(cx).text().to_lowercase().split_whitespace().map(str::to_string).collect()
    }

    fn show_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        self.page = page;
        self.menu = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
        if !self.search.read(cx).text().is_empty() {
            self.search.update(cx, |e, cx| e.set_text("", cx));
        }
        if page == Page::Keymap {
            self.keymap.update(cx, |k, cx| k.focus_filter(window, cx));
        }
        cx.notify();
    }

    /// ↑/↓ in the search field walk the pages (when not searching).
    fn on_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        self.step_page(-1, window, cx);
    }

    fn on_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        self.step_page(1, window, cx);
    }

    fn step_page(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search.focus_handle(cx).is_focused(window) || !self.search.read(cx).text().is_empty() {
            cx.propagate();
            return;
        }
        let ix = Page::ALL.iter().position(|p| *p == self.page).unwrap_or(0) as isize + step;
        let page = Page::ALL[ix.clamp(0, Page::ALL.len() as isize - 1) as usize];
        self.show_page(page, window, cx);
        // Keep typing in the search field.
        window.focus(&self.search.focus_handle(cx));
    }

    fn on_cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        } else if self.keymap.read(cx).is_recording() {
            self.keymap.update(cx, |k, cx| k.stop_recording(cx));
        } else {
            cx.emit(PreferencesEvent::Dismissed);
        }
    }

    // ---- Writing settings ---------------------------------------------------------

    /// Applies what was just written (or reports why it couldn't be).
    fn applied(&mut self, results: Vec<std::io::Result<()>>, cx: &mut Context<Self>) {
        let mut problems: Vec<String> = results
            .into_iter()
            .filter_map(Result::err)
            .map(|e| {
                let text = e.to_string();
                if text.starts_with("settings.json") { text } else { format!("settings.json: {text}") }
            })
            .collect();
        problems.extend(settings::reload_now(cx));
        self.file_error = settings_file_error();
        if !problems.is_empty() {
            cx.emit(PreferencesEvent::Problems(problems));
        }
        cx.notify();
    }

    fn write(&mut self, result: std::io::Result<()>, cx: &mut Context<Self>) {
        self.applied(vec![result], cx);
    }

    /// Sets a top-level key, or removes it when `value` is the default, so
    /// the file only lists what the user changed.
    fn set_or_default(&mut self, key: &str, value: Value, default: Value, cx: &mut Context<Self>) {
        let result = if value == default { Settings::remove(key) } else { Settings::set(key, value) };
        self.write(result, cx);
    }

    fn reset(&mut self, key: &str, cx: &mut Context<Self>) {
        self.write(Settings::remove(key), cx);
        if key == "editor_font_size" {
            theme::zoom(None, cx);
        }
    }

    fn choose_theme(&mut self, name: SharedString, cx: &mut Context<Self>) {
        theme::apply_theme(&name, cx);
        let result = if name.as_ref() == theme::DEFAULT_DARK && Settings::get(cx).theme.is_none() {
            Ok(())
        } else {
            Settings::set("theme", name.as_ref().into())
        };
        self.write(result, cx);
    }

    fn schedule_commit(&mut self, cx: &mut Context<Self>) {
        self.commit_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(450)).await;
            this.update(cx, |this, cx| this.commit_text_fields(cx)).ok();
        }));
    }

    /// Writes anything typed but not yet saved; called before closing.
    pub fn flush(&mut self, cx: &mut Context<Self>) {
        if self.commit_task.take().is_some() {
            self.commit_text_fields(cx);
        }
    }

    /// Saves the text fields (shell, commands, Run configurations) that
    /// differ from the settings.
    fn commit_text_fields(&mut self, cx: &mut Context<Self>) {
        self.commit_task = None;
        let settings = Settings::get(cx).clone();
        let mut results = Vec::new();

        let shell = split_command(&self.shell.read(cx).text());
        let shell = (!shell.is_empty()).then_some(shell);
        if shell != settings.terminal_shell {
            results.push(match shell {
                Some(shell) => Settings::set("terminal_shell", json!(shell)),
                None => Settings::remove("terminal_shell"),
            });
        }

        let language = self.language;
        for (tool, field) in [(Tool::Server, &self.server_command), (Tool::Formatter, &self.formatter_command)] {
            let configured = tool_value(&settings, tool, language);
            let custom = ToolMode::of(configured.as_ref()) == ToolMode::Custom || self.pending_custom == Some(tool);
            let command = split_command(&field.read(cx).text());
            if custom && !command.is_empty() && configured != Some(json!(command)) {
                results.push(Settings::update_language_entry(tool.key(), language, |_| Some(json!(command))));
                if self.pending_custom == Some(tool) {
                    self.pending_custom = None;
                }
            }
        }

        let runs: Vec<RunConfiguration> = self
            .runs
            .iter()
            .filter_map(|row| {
                let command = row.command.read(cx).text().trim().to_string();
                let name = row.name.read(cx).text().trim().to_string();
                let cwd = row.cwd.read(cx).text().trim().to_string();
                (!command.is_empty()).then(|| RunConfiguration {
                    name: if name.is_empty() { command.clone() } else { name },
                    command,
                    cwd: (!cwd.is_empty()).then_some(cwd),
                })
            })
            .collect();
        if runs != settings.run_configurations {
            results.push(if runs.is_empty() {
                Settings::remove("run_configurations")
            } else {
                let list: Vec<Value> = runs
                    .iter()
                    .map(|r| {
                        let mut entry = json!({ "name": r.name, "command": r.command });
                        if let Some(cwd) = &r.cwd {
                            entry["cwd"] = cwd.as_str().into();
                        }
                        entry
                    })
                    .collect();
                Settings::set("run_configurations", Value::Array(list))
            });
        }
        if !results.is_empty() {
            self.applied(results, cx);
        }
    }

    // ---- Fonts --------------------------------------------------------------------

    fn toggle_menu(&mut self, menu: Menu, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu == Some(menu) {
            self.menu = None;
        } else {
            self.menu = Some(menu);
            if menu == Menu::FontFamily {
                self.font_filter.update(cx, |e, cx| e.set_text("", cx));
                window.focus(&self.font_filter.focus_handle(cx));
                self.load_fonts(cx);
            }
        }
        cx.notify();
    }

    fn load_fonts(&mut self, cx: &mut Context<Self>) {
        if self.fonts.is_some() || self.fonts_task.is_some() {
            return;
        }
        let text_system = cx.text_system().clone();
        self.fonts_task = Some(cx.spawn(async move |this, cx| {
            let fonts = cx.background_executor().spawn(async move { monospaced_fonts(&text_system) }).await;
            this.update(cx, |this, cx| {
                this.fonts = Some(Arc::new(fonts));
                cx.notify();
            })
            .ok();
        }));
    }

    fn choose_font(&mut self, family: Option<SharedString>, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let result = match family {
            Some(family) => Settings::set("editor_font_family", family.as_ref().into()),
            None => Settings::remove("editor_font_family"),
        };
        self.write(result, cx);
        window.focus(&self.search.focus_handle(cx));
    }

    // ---- Languages ----------------------------------------------------------------

    fn select_language(&mut self, language: Language, cx: &mut Context<Self>) {
        self.flush(cx);
        self.language = language;
        self.menu = None;
        self.pending_custom = None;
        self.load_language_fields(cx);
        cx.notify();
    }

    fn load_language_fields(&mut self, cx: &mut Context<Self>) {
        let settings = Settings::get(cx).clone();
        for (tool, field) in [(Tool::Server, self.server_command.clone()), (Tool::Formatter, self.formatter_command.clone())] {
            let command = match tool_value(&settings, tool, self.language) {
                Some(Value::Array(items)) => join_command(&strings(&items)),
                _ => String::new(),
            };
            set_field(&field, &command, cx);
        }
        // Setting the text counts as an edit; nothing to save.
        self.commit_task = None;
    }

    fn set_tool_mode(&mut self, tool: Tool, mode: ToolMode, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let language = self.language;
        let field = match tool {
            Tool::Server => self.server_command.clone(),
            Tool::Formatter => self.formatter_command.clone(),
        };
        let value = match mode {
            ToolMode::Automatic => None,
            ToolMode::Off => Some(Value::Bool(false)),
            ToolMode::LanguageServer => Some("language_server".into()),
            ToolMode::Custom => {
                // Start from the command Kin would use, or an empty field to type into.
                let typed = split_command(&field.read(cx).text());
                let command = if typed.is_empty() { default_command(tool, language) } else { typed };
                if command.is_empty() {
                    self.pending_custom = Some(tool);
                    window.focus(&field.focus_handle(cx));
                    cx.notify();
                    return;
                }
                set_field(&field, &join_command(&command), cx);
                self.commit_task = None;
                Some(json!(command))
            }
        };
        self.pending_custom = None;
        self.write(Settings::update_language_entry(tool.key(), language, |_| value), cx);
    }

    fn set_language_option(&mut self, option: LanguageOption, value: Value, cx: &mut Context<Self>) {
        let language = self.language;
        // Matching what the language gets anyway is no override at all.
        let mut without = Settings::get(cx).clone();
        if let Some(overrides) = without.languages.get_mut(language.name()) {
            option.clear(overrides);
        }
        let inherited = without.for_file(language, None);
        let same = match option {
            LanguageOption::TabSize => value.as_u64() == Some(inherited.tab_size as u64),
            _ => value.as_bool() == Some(option.flag(&inherited)),
        };
        let value = (!same).then_some(value);
        self.write(Settings::set_language_option(language, option.key(), value), cx);
    }

    // ---- Run configurations ---------------------------------------------------------

    fn add_run_row(&mut self, run: &RunConfiguration, cx: &mut Context<Self>) {
        let name = cx.new(|cx| Editor::single_line("Name", cx));
        let command = cx.new(|cx| Editor::single_line("Command", cx));
        let cwd = cx.new(|cx| Editor::single_line("Folder", cx));
        set_field(&name, &run.name, cx);
        set_field(&command, &run.command, cx);
        set_field(&cwd, run.cwd.as_deref().unwrap_or(""), cx);
        for field in [&name, &command, &cwd] {
            self._subscriptions.push(cx.subscribe(field, |this, _, _: &EditorEvent, cx| this.schedule_commit(cx)));
        }
        self.runs.push(RunRow { name, command, cwd });
        self.commit_task = None;
    }

    fn new_run_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_run_row(&RunConfiguration::default(), cx);
        if let Some(row) = self.runs.last() {
            window.focus(&row.name.focus_handle(cx));
        }
        cx.notify();
    }

    fn remove_run_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.runs.len() {
            self.runs.remove(ix);
            self.commit_text_fields(cx);
            cx.notify();
        }
    }
}

impl Focusable for Preferences {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ---- Rendering -----------------------------------------------------------------------

impl Render for Preferences {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let viewport = window.viewport_size();
        // Matches the modal layer's top offset, so the dialog fits below it.
        let top = (viewport.height * 0.12).max(px(50.));
        let height = (viewport.height - top - px(24.)).min(px(640.)).max(px(320.));
        let words = self.query_words(cx);
        self.overlay = None;

        let content = if words.is_empty() {
            self.render_page(window, cx)
        } else {
            self.render_results(&words, window, cx)
        };

        modal_panel(cx)
            .id("preferences")
            .key_context("Preferences")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(|this, _: &Find, window, cx| {
                window.focus(&this.search.focus_handle(cx));
                this.search.update(cx, |e, cx| e.select_all_text(cx));
            }))
            .flex_row()
            .bg(theme.background)
            .w(px(920.))
            .max_w(viewport.width - px(48.))
            .h(height)
            .text_size(px(13.))
            .font_family(theme.ui_font.clone())
            .child(self.render_nav(&words, window, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.render_header(&words, cx))
                    .when_some(self.file_error.clone(), |el, error| el.child(self.render_file_error(error, cx)))
                    .child(content),
            )
            .children(self.overlay.take())
    }
}

impl Preferences {
    fn render_nav(&self, words: &[String], window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let searching = !words.is_empty();
        let mut items: Vec<AnyElement> = Vec::new();
        for page in Page::ALL {
            let selected = !searching && page == self.page;
            items.push(div()
                .id(("pref-page", page as usize))
                .flex()
                .items_center()
                .gap_2()
                .h(px(30.))
                .px_2()
                .rounded(px(6.))
                .cursor_pointer()
                .text_color(if selected { theme.text } else { theme.text_muted })
                .when(selected, |el| el.bg(theme.active).font_weight(FontWeight::MEDIUM))
                .when(!selected, |el| el.hover(|s| s.bg(theme.hover).text_color(theme.text)))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.show_page(page, window, cx)))
                .child(icon_el(page.icon(), px(14.), if selected { theme.text.into() } else { theme.text_dim.into() }))
                .child(page.title())
                .into_any_element());
            // The open page's sections, to jump to.
            let sections = page_sections(page);
            if selected && sections.len() > 1 {
                for (ix, name) in sections.into_iter().enumerate() {
                    items.push(
                        div()
                            .id(("pref-section", ix))
                            .flex()
                            .items_center()
                            .h(px(24.))
                            .pl(px(32.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(theme.text_dim)
                            .hover(|s| s.bg(theme.hover).text_color(theme.text))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.scroll.scroll_to_top_of_item(ix);
                                cx.notify();
                            }))
                            .child(name)
                            .into_any_element(),
                    );
                }
            }
        }
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(212.))
            .h_full()
            // GPUI clips children to a rectangle, so backgrounds next to the
            // dialog's rounded corners need rounding of their own (12 - border).
            .rounded_l(px(11.))
            .bg(theme.panel)
            .border_r_1()
            .border_color(theme.border)
            .child(div().p_2().pt_3().child(input_box(&self.search, Some(Icon::Search), window, cx)))
            .child(div().flex().flex_col().gap_0p5().px_2().pt_1().flex_1().children(items))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .p_2()
                    .border_t_1()
                    .border_color(theme.border_subtle)
                    .child(
                        link_row("pref-open-settings", Icon::FileCode, "Open settings.json", &theme)
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(PreferencesEvent::OpenSettingsFile))),
                    ),
            )
    }

    fn render_header(&self, words: &[String], cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let (title, summary): (SharedString, SharedString) = if words.is_empty() {
            (self.page.title().into(), self.page.summary().into())
        } else {
            ("Search".into(), format!("Settings matching “{}”", words.join(" ")).into())
        };
        div()
            .flex()
            .items_start()
            .gap_3()
            .flex_shrink_0()
            .px(px(28.))
            .pt(px(22.))
            .pb(px(14.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_size(px(18.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(title))
                    .child(div().text_color(theme.text_dim).child(summary)),
            )
            .when(words.is_empty() && self.page == Page::Keymap, |el| {
                el.child(
                    text_button("pref-open-keymap", "Open keymap.json", false, cx)
                        .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(PreferencesEvent::OpenKeymapFile))),
                )
            })
            .child(
                icon_button("pref-close", Icon::X, theme.text_muted.into(), cx)
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(PreferencesEvent::Dismissed))),
            )
    }

    fn render_file_error(&self, error: String, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        div()
            .mx(px(28.))
            .mb_3()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .rounded(px(8.))
            .bg(Hsla::from(theme.warning).alpha(0.1))
            .border_1()
            .border_color(Hsla::from(theme.warning).alpha(0.35))
            .child(icon_el(Icon::TriangleAlert, px(14.), theme.warning.into()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.))
                    .text_color(theme.text)
                    .child(format!("Changes can't be saved until settings.json is fixed: {error}")),
            )
            .child(
                text_button("pref-fix-settings", "Open", false, cx)
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(PreferencesEvent::OpenSettingsFile))),
            )
    }

    /// The scrolling area under the header.
    fn body(&self) -> Stateful<Div> {
        div()
            .id("pref-body")
            .flex()
            .flex_col()
            .gap_6()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(28.))
            .pb(px(28.))
    }

    fn render_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.page {
            Page::Languages => return self.render_languages(window, cx).into_any_element(),
            Page::Keymap => return self.keymap.clone().into_any_element(),
            _ => {}
        }
        let mut sections: Vec<(&'static str, Vec<Setting>)> = Vec::new();
        for setting in Setting::ALL.into_iter().filter(|s| s.page() == self.page) {
            match sections.last_mut() {
                Some((name, list)) if *name == setting.section() => list.push(setting),
                _ => sections.push((setting.section(), vec![setting])),
            }
        }
        let theme = Theme::get(cx).clone();
        let sections: Vec<AnyElement> = sections
            .into_iter()
            .map(|(name, list)| {
                let rows: Vec<AnyElement> = list.into_iter().map(|s| self.render_setting(s, window, cx)).collect();
                section(name, rows, &theme).into_any_element()
            })
            .collect();
        self.body().children(sections).into_any_element()
    }

    /// Search results across pages, plus links into Languages and Keymap.
    fn render_results(&mut self, words: &[String], window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let mut groups: Vec<AnyElement> = Vec::new();
        for page in Page::ALL {
            let rows: Vec<AnyElement> = Setting::ALL
                .into_iter()
                .filter(|s| s.page() == page && s.matches(words))
                .map(|s| self.render_setting(s, window, cx))
                .collect();
            if !rows.is_empty() {
                groups.push(section(page.title(), rows, &theme).into_any_element());
            }
        }
        let languages: Vec<Language> = Language::ALL
            .into_iter()
            .filter(|l| {
                let name = l.name().to_lowercase();
                words.iter().all(|w| name.contains(w.as_str()) || "language server formatter".contains(w.as_str()))
            })
            .collect();
        if !languages.is_empty() {
            let rows = languages
                .into_iter()
                .map(|language| {
                    jump_row(("pref-find-lang", language as usize), Icon::Braces, language.name(), "Language settings", &theme)
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.show_page(Page::Languages, window, cx);
                            this.select_language(language, cx);
                        }))
                        .into_any_element()
                })
                .collect();
            groups.push(section("Languages", rows, &theme).into_any_element());
        }
        let query = words.join(" ");
        let actions = self.keymap.read(cx).count_matching(&query, window);
        if actions > 0 {
            let detail = format!("{actions} action{} with a shortcut to change", if actions == 1 { "" } else { "s" });
            let row = jump_row("pref-find-keymap", Icon::Keyboard, format!("Keymap: “{query}”"), detail, &theme)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    let query = query.clone();
                    this.show_page(Page::Keymap, window, cx);
                    this.keymap.update(cx, |k, cx| k.set_filter(&query, cx));
                }))
                .into_any_element();
            groups.push(section("Keymap", vec![row], &theme).into_any_element());
        }
        if groups.is_empty() {
            return div()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .pt(px(60.))
                .flex_1()
                .text_color(theme.text_dim)
                .child(icon_el(Icon::Search, px(22.), theme.text_dim.into()))
                .child("No settings match")
                .into_any_element();
        }
        self.body().children(groups).into_any_element()
    }

    fn render_setting(&mut self, setting: Setting, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let settings = Settings::get(cx).clone();
        let key = setting_key(setting);
        let reset = (!key.is_empty() && settings.is_set(key)).then(|| {
            self.reset_button(("pref-reset", setting as usize), cx, move |this, cx| this.reset(key, cx)).into_any_element()
        });
        let control: AnyElement = match setting {
            Setting::Theme => return self.render_themes(cx).into_any_element(),
            Setting::RunConfigurations => return self.render_runs(window, cx).into_any_element(),
            Setting::FontFamily => {
                let (trigger, list) = self.render_font_menu(window, cx);
                let row = setting_row(setting.label(), Some(setting.description().into()), trigger, reset, &theme);
                return div().flex().flex_col().child(row).children(list).into_any_element();
            }
            Setting::FontSize => {
                let size = settings.editor_font_size.unwrap_or(theme::DEFAULT_EDITOR_FONT_SIZE);
                self.stepper("pref-font-size", format!("{size}"), size > 6., size < 72., cx, |this, step, cx| {
                    let size = Settings::get(cx).editor_font_size.unwrap_or(theme::DEFAULT_EDITOR_FONT_SIZE);
                    let size = (size.round() + step as f32).clamp(6., 72.);
                    this.set_or_default("editor_font_size", json!(size), json!(theme::DEFAULT_EDITOR_FONT_SIZE), cx);
                    // What's picked here is what shows, without an old zoom on top.
                    theme::zoom(None, cx);
                })
            }
            Setting::LineHeight => {
                let current = Theme::get(cx);
                let height = settings
                    .editor_line_height
                    .unwrap_or(current.editor_line_height / current.editor_font_size);
                let label = format!("{:.1}", (height * 10.).round() / 10.);
                self.stepper("pref-line-height", label, height > 1.05, height < 2.95, cx, |this, step, cx| {
                    let current = Theme::get(cx);
                    let height = Settings::get(cx)
                        .editor_line_height
                        .unwrap_or(current.editor_line_height / current.editor_font_size);
                    let next = (((height * 10.).round() + step as f32) / 10.).clamp(1., 3.);
                    this.write(Settings::set("editor_line_height", json!((next * 10.).round() / 10.)), cx);
                })
            }
            Setting::TabSize => {
                let size = settings.editor.tab_size.unwrap_or(DEFAULT_TAB_SIZE);
                self.stepper("pref-tab-size", size.to_string(), size > 1, size < 16, cx, |this, step, cx| {
                    let size = Settings::get(cx).editor.tab_size.unwrap_or(DEFAULT_TAB_SIZE);
                    let size = (size as i64 + step).clamp(1, 16);
                    this.set_or_default("tab_size", json!(size), json!(DEFAULT_TAB_SIZE), cx);
                })
            }
            Setting::HardTabs => self.bool_switch("pref-hard-tabs", "hard_tabs", settings.editor.hard_tabs, cx),
            Setting::SoftWrap => self.bool_switch("pref-soft-wrap", "soft_wrap", settings.editor.soft_wrap, cx),
            Setting::FormatOnSave => self.bool_switch("pref-format", "format_on_save", settings.editor.format_on_save, cx),
            Setting::TrimWhitespace => {
                self.bool_switch("pref-trim", "trim_trailing_whitespace", settings.editor.trim_trailing_whitespace, cx)
            }
            Setting::FinalNewline => {
                self.bool_switch("pref-newline", "insert_final_newline", settings.editor.insert_final_newline, cx)
            }
            Setting::AutoSave => {
                let current = settings.auto_save.unwrap_or_default();
                let modes = [AutoSave::Off, AutoSave::AfterDelay, AutoSave::OnFocusChange, AutoSave::OnWindowChange];
                let options = modes.iter().map(|m| (auto_save_label(*m).into(), *m == current)).collect();
                self.dropdown(Menu::AutoSave, auto_save_label(current).into(), px(200.), options, cx, move |this, ix, _, cx| {
                    this.menu = None;
                    let mode = modes[ix];
                    this.set_or_default("auto_save", mode.key().into(), AutoSave::default().key().into(), cx);
                })
            }
            Setting::AutoSaveDelay => {
                let delay = settings.auto_save_delay.unwrap_or(DEFAULT_AUTO_SAVE_DELAY);
                let enabled = settings.auto_save.unwrap_or_default() == AutoSave::AfterDelay;
                let label = if delay >= 1000 { format!("{} s", delay as f64 / 1000.) } else { format!("{delay} ms") };
                div()
                    .when(!enabled, |el| el.opacity(0.45))
                    .child(self.stepper("pref-delay", label, enabled && delay > 250, enabled && delay < 10_000, cx, |this, step, cx| {
                        let delay = Settings::get(cx).auto_save_delay.unwrap_or(DEFAULT_AUTO_SAVE_DELAY) as i64;
                        // Snap to quarter seconds.
                        let next = ((delay + 125) / 250 * 250 + step * 250).clamp(250, 10_000);
                        this.set_or_default("auto_save_delay", json!(next), json!(DEFAULT_AUTO_SAVE_DELAY), cx);
                    }))
                    .into_any_element()
            }
            Setting::Shell => div().w(px(300.)).flex().child(input_box(&self.shell, None, window, cx)).into_any_element(),
        };
        setting_row(setting.label(), Some(setting.description().into()), control, reset, &theme).into_any_element()
    }

    fn bool_switch(&self, id: &'static str, key: &'static str, value: Option<bool>, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let on = value.unwrap_or(false);
        div()
            .id(id)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.set_or_default(key, json!(!on), json!(false), cx);
            }))
            .child(switch(on, &theme))
            .into_any_element()
    }

    fn reset_button(
        &self,
        id: impl Into<gpui::ElementId>,
        cx: &mut Context<Self>,
        on_reset: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        icon_button(id, Icon::Undo, theme.text_dim.into(), cx)
            .size(px(20.))
            .tooltip(|_, cx| tooltip_view("Reset to default", cx))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| on_reset(this, cx)))
    }

    /// `− value +`; `on_step` gets -1 or 1.
    fn stepper(
        &self,
        id: &'static str,
        value: String,
        can_decrease: bool,
        can_increase: bool,
        cx: &mut Context<Self>,
        on_step: impl Fn(&mut Self, i64, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let on_step = Rc::new(on_step);
        let button = |icon: Icon, step: i64, enabled: bool, cx: &mut Context<Self>| {
            let on_step = on_step.clone();
            div()
                .id((id, if step < 0 { 0usize } else { 1 }))
                .size(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .when(enabled, |el| {
                    el.cursor_pointer()
                        .hover(|s| s.bg(theme.hover))
                        .active(|s| s.bg(theme.active))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| on_step(this, step, cx)))
                })
                .child(icon_el(icon, px(13.), if enabled { theme.text_muted.into() } else { Hsla::from(theme.text_dim).alpha(0.5) }))
        };
        div()
            .flex()
            .items_center()
            .h(px(30.))
            .p(px(2.))
            .rounded(px(7.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(button(Icon::Minus, -1, can_decrease, cx))
            .child(
                div()
                    .min_w(px(52.))
                    .flex()
                    .justify_center()
                    .font_family(theme.mono_font.clone())
                    .text_size(px(12.5))
                    .text_color(theme.text)
                    .child(value),
            )
            .child(button(Icon::Plus, 1, can_increase, cx))
            .into_any_element()
    }

    /// A button showing the current choice, with a list under it while open.
    /// A button showing the current choice. While open, its list is kept in
    /// `self.overlay`, which `render` draws last so it floats over the rows
    /// below. (The modal layer is already deferred, and GPUI can't nest
    /// deferred drawing.)
    fn dropdown(
        &mut self,
        menu: Menu,
        label: SharedString,
        width: Pixels,
        options: Vec<(SharedString, bool)>,
        cx: &mut Context<Self>,
        on_pick: impl Fn(&mut Self, usize, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let open = self.menu == Some(menu);
        let on_pick = Rc::new(on_pick);
        let list = open.then(|| {
            let items = options.into_iter().enumerate().map(|(ix, (text, selected))| {
                let on_pick = on_pick.clone();
                div()
                    .id(("pref-option", ix))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(26.))
                    .px_2()
                    .rounded(px(5.))
                    .cursor_pointer()
                    .text_color(theme.text)
                    .hover(|s| s.bg(theme.hover))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| on_pick(this, ix, window, cx)))
                    .child(div().flex_1().child(text))
                    .when(selected, |el| el.child(icon_el(Icon::Check, px(13.), theme.text_muted.into())))
            });
            let anchor = self.menu_anchors[menu as usize].clone();
            let trigger = anchor.get();
            anchored()
                .position(point(trigger.right(), trigger.bottom() + px(4.)))
                .anchor(Corner::TopRight)
                .snap_to_window_with_margin(px(8.))
                .child(
                    div()
                        .w(width)
                        .p_1()
                        .flex()
                        .flex_col()
                        .rounded(px(8.))
                        .bg(theme.elevated)
                        .border_1()
                        .border_color(theme.border)
                        .shadow_lg()
                        .occlude()
                        .on_mouse_down_out(cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                            // The trigger's own click toggles it closed.
                            if !anchor.get().contains(&event.position) {
                                this.menu = None;
                                cx.notify();
                            }
                        }))
                        .children(items),
                )
                .into_any_element()
        });
        let anchor = self.menu_anchors[menu as usize].clone();
        if list.is_some() {
            self.overlay = list;
        }
        div()
            .relative()
            .child(dropdown_trigger(("pref-menu", menu as usize), label, width, open, &theme).on_click(cx.listener(
                move |this, _: &ClickEvent, window, cx| this.toggle_menu(menu, window, cx),
            )))
            .child(canvas(move |bounds, _, _| anchor.set(bounds), |_, _, _, _| {}).absolute().size_full())
            .into_any_element()
    }

    /// The font button, and while it's open, the list that unfolds under its
    /// row. (Not a floating menu: GPUI can't defer the filter field's
    /// editor inside another deferred element.)
    fn render_font_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> (AnyElement, Option<AnyElement>) {
        let theme = Theme::get(cx).clone();
        let configured = Settings::get(cx).editor_font_family.clone();
        let default_font = theme::pick_fonts(cx).1;
        let label: SharedString = match &configured {
            Some(family) if *family != theme.mono_font.as_ref() => format!("{family} (not installed)").into(),
            Some(family) => family.clone().into(),
            None => format!("Default ({default_font})").into(),
        };
        let open = self.menu == Some(Menu::FontFamily);
        let width = px(240.);
        let list = open.then(|| {
            let filter = self.font_filter.read(cx).text().to_lowercase();
            let mut rows: Vec<Option<SharedString>> = vec![None];
            match &self.fonts {
                Some(fonts) => rows.extend(fonts.iter().filter(|f| f.to_lowercase().contains(&filter)).cloned().map(Some)),
                None => {}
            }
            if !filter.is_empty() {
                rows.retain(Option::is_some);
            }
            let rows = Arc::new(rows);
            let count = rows.len();
            let loading = self.fonts.is_none();
            let list = uniform_list("pref-fonts", count, {
                let rows = rows.clone();
                let configured = configured.clone();
                let default_font = default_font.clone();
                cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                    let theme = Theme::get(cx).clone();
                    range
                        .map(|ix| {
                            let family = rows[ix].clone();
                            let selected = family.as_ref().map(|f| f.to_string()) == configured;
                            let text: SharedString = match &family {
                                Some(f) => f.clone(),
                                None => format!("Default ({default_font})").into(),
                            };
                            div().w_full().child(
                                div()
                                    .id(("pref-font", ix))
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .h(px(26.))
                                    .px_2()
                                    .rounded(px(5.))
                                    .cursor_pointer()
                                    .text_color(theme.text)
                                    .hover(|s| s.bg(theme.hover))
                                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                        this.choose_font(family.clone(), window, cx)
                                    }))
                                    .child(div().flex_1().min_w_0().truncate().child(text))
                                    // A sample in the font itself; the name stays readable
                                    // even for symbol fonts.
                                    .when_some(rows[ix].clone(), |el, f| {
                                        el.child(div().font_family(f).text_color(theme.text_muted).child("Ag 0O il1 {}"))
                                    })
                                    .when(selected, |el| el.child(icon_el(Icon::Check, px(13.), theme.text_muted.into()))),
                            )
                        })
                        .collect()
                })
            })
            .h(px((count.clamp(1, 8) * 26) as f32))
            .w_full();
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .mx_4()
                .mb_3()
                .p_1p5()
                .rounded(px(8.))
                .bg(theme.background)
                .border_1()
                .border_color(theme.border)
                .child(input_box(&self.font_filter, Some(Icon::Search), window, cx))
                .child(if loading {
                    div().h(px(26.)).px_2().flex().items_center().text_color(theme.text_dim).child("Finding fonts…").into_any_element()
                } else if count == 0 {
                    div().h(px(26.)).px_2().flex().items_center().text_color(theme.text_dim).child("No fonts match").into_any_element()
                } else {
                    list.into_any_element()
                })
                .into_any_element()
        });
        let trigger = dropdown_trigger("pref-font-menu", label, width, open, &theme)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.toggle_menu(Menu::FontFamily, window, cx)))
            .into_any_element();
        (trigger, list)
    }

    fn render_themes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let mut entries: Vec<(Arc<Theme>, bool)> =
            cx.global::<ThemeRegistry>().entries.iter().map(|e| (e.theme.clone(), e.path.is_some())).collect();
        entries.sort_by_key(|(t, _)| t.appearance == Appearance::Light);
        let errors = cx.global::<ThemeRegistry>().errors.clone();
        let cards: Vec<(Appearance, AnyElement)> = entries.into_iter().enumerate().map(|(ix, (t, custom))| {
            let appearance = t.appearance;
            let selected = t.name == theme.name;
            let name = t.name.clone();
            let bar = |w: f32, color: gpui::Rgba| div().h(px(4.)).w(px(w)).rounded_full().bg(color);
            let line = |indent: f32| div().flex().items_center().gap(px(4.)).pl(px(indent));
            let card = div()
                .id(("pref-theme", ix))
                .flex()
                .flex_col()
                .rounded(px(8.))
                .overflow_hidden()
                .border_1()
                // Only the current theme is outlined; the others keep a clear border so
                // every card has the same size.
                .border_color(if selected { Hsla::from(theme.accent) } else { gpui::transparent_black() })
                .when(selected, |el| el.shadow_sm())
                .cursor_pointer()
                .hover(|s| s.border_color(Hsla::from(theme.accent).alpha(0.6)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.choose_theme(name.clone(), cx)))
                // A few lines of "code" in the theme's own colors.
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(5.))
                        .h(px(52.))
                        .px_2p5()
                        .py(px(8.))
                        .rounded_t(px(7.))
                        .bg(t.background)
                        .child(line(0.).child(bar(16., t.syntax.keyword)).child(bar(30., t.syntax.function)).child(bar(8., t.syntax.punctuation)))
                        .child(line(10.).child(bar(12., t.syntax.keyword)).child(bar(22., t.syntax.variable_builtin)).child(bar(34., t.syntax.string)))
                        .child(line(10.).child(bar(44., t.syntax.comment)))
                        .child(line(0.).child(bar(8., t.syntax.punctuation)).child(bar(18., t.syntax.type_)).child(bar(10., t.syntax.number))),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .h(px(28.))
                        .px_2p5()
                        .rounded_b(px(7.))
                        .bg(t.panel)
                        .border_t_1()
                        .border_color(t.border)
                        .text_size(px(12.))
                        .text_color(t.text)
                        .child(div().flex_1().min_w_0().truncate().child(t.name.clone()))
                        .when(custom, |el| el.child(div().text_size(px(10.5)).text_color(t.text_dim).child("Custom")))
                        .when(selected, |el| el.child(icon_el(Icon::CircleCheck, px(13.), t.accent.into()))),
                );
            (appearance, card.into_any_element())
        }).collect();
        // Dark and light themes apart, in rows of four equal cards (GPUI's
        // grid doesn't size these).
        const PER_ROW: usize = 4;
        let mut groups: Vec<AnyElement> = Vec::new();
        let (dark, light): (Vec<_>, Vec<_>) = cards.into_iter().partition(|(a, _)| *a == Appearance::Dark);
        for (title, cards) in [("Dark", dark), ("Light", light)] {
            if cards.is_empty() {
                continue;
            }
            let mut cards: Vec<AnyElement> = cards.into_iter().map(|(_, c)| c).collect();
            while cards.len() % PER_ROW != 0 {
                cards.push(div().into_any_element());
            }
            let mut rows = Vec::new();
            while !cards.is_empty() {
                let rest = cards.split_off(PER_ROW);
                let row = std::mem::replace(&mut cards, rest);
                rows.push(div().flex().gap_3().children(row.into_iter().map(|c| div().flex_1().flex_basis(px(0.)).min_w_0().child(c))));
            }
            groups.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_size(px(12.)).font_weight(FontWeight::MEDIUM).text_color(theme.text_muted).child(title))
                    .children(rows)
                    .into_any_element(),
            );
        }
        let theme_dir = theme::themes_dir().map(|d| theme::display_path(&d)).unwrap_or_default();
        div()
            .flex()
            .flex_col()
            .gap_4()
            .p_3()
            .children(groups)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("pref-theme-hint")
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(if errors.is_empty() { theme.text_dim } else { theme.warning })
                            .child(if errors.is_empty() {
                                format!("Your own themes go in {theme_dir}")
                            } else {
                                format!("{} theme problem(s): {}", errors.len(), errors[0])
                            })
                            .when(!errors.is_empty(), |el| {
                                let all = errors.join("\n");
                                el.tooltip(move |_, cx| tooltip_view(all.clone(), cx))
                            }),
                    )
                    .child(text_button("pref-reload-themes", "Reload", false, cx).on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                        theme::reload_themes(cx);
                        cx.notify();
                    })))
                    .child(text_button("pref-themes-folder", "Open Themes Folder", false, cx).on_click(cx.listener(
                        |_, _: &ClickEvent, _, cx| match theme::ensure_themes_dir() {
                            Ok(dir) => {
                                cx.open_url(&format!("file://{}", dir.display()));
                                theme::reload_themes(cx);
                                cx.notify();
                            }
                            Err(err) => cx.emit(PreferencesEvent::Problems(vec![format!("themes: {err}")])),
                        },
                    ))),
            )
    }

    fn render_runs(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let rows: Vec<AnyElement> = self
            .runs
            .iter()
            .enumerate()
            .map(|(ix, row)| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .when(ix > 0, |el| el.border_t_1().border_color(theme.border_subtle))
                    .child(icon_el(Icon::Play, px(13.), theme.text_dim.into()))
                    .child(div().w(px(150.)).flex().child(input_box(&row.name, None, window, cx)))
                    .child(div().flex_1().min_w_0().flex().child(input_box(&row.command, None, window, cx)))
                    .child(div().w(px(110.)).flex().child(input_box(&row.cwd, None, window, cx)))
                    .child(
                        icon_button(("pref-run-remove", ix), Icon::Trash, theme.text_dim.into(), cx)
                            .tooltip(|_, cx| tooltip_view("Remove", cx))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.remove_run_row(ix, cx))),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        div()
            .flex()
            .flex_col()
            .children(rows)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .px_4()
                    .py_2p5()
                    .when(!empty, |el| el.border_t_1().border_color(theme.border_subtle))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(theme.text_dim)
                            .child(Setting::RunConfigurations.description()),
                    )
                    .child(
                        text_button("pref-run-add", "Add Configuration", false, cx)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.new_run_row(window, cx))),
                    ),
            )
    }

    fn render_languages(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let settings = Settings::get(cx).clone();
        let customized = |l: Language| {
            settings.languages.contains_key(l.name())
                || settings.language_servers.contains_key(l.name())
                || settings.formatters.contains_key(l.name())
        };
        let chosen = self.language;
        let list: Vec<AnyElement> = Language::ALL.into_iter().map(|language| {
            let selected = language == chosen;
            div()
                .id(("pref-lang", language as usize))
                .flex()
                .items_center()
                .gap_2()
                .h(px(28.))
                .px_2()
                .rounded(px(6.))
                .cursor_pointer()
                .text_color(if selected { theme.text } else { theme.text_muted })
                .when(selected, |el| el.bg(theme.active))
                .when(!selected, |el| el.hover(|s| s.bg(theme.hover).text_color(theme.text)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.select_language(language, cx)))
                .child(div().flex_1().child(language.name()))
                .when(customized(language), |el| {
                    el.child(div().size(px(6.)).rounded_full().bg(theme.text_muted))
                        .tooltip(|_, cx| tooltip_view("Has its own settings", cx))
                })
                .into_any_element()
        }).collect();

        let language = self.language;
        let overrides = settings.languages.get(language.name()).copied().unwrap_or_default();
        let effective = settings.for_file(language, None);
        let mut editor_rows: Vec<AnyElement> = Vec::new();
        for option in LanguageOption::ALL {
            let set = option.is_set(&overrides);
            let reset = set.then(|| {
                self.reset_button(("pref-lang-reset", option as usize), cx, move |this, cx| {
                    let language = this.language;
                    this.write(Settings::set_language_option(language, option.key(), None), cx);
                })
                .into_any_element()
            });
            let control = match option {
                LanguageOption::TabSize => {
                    let size = effective.tab_size;
                    self.stepper("pref-lang-tab", size.to_string(), size > 1, size < 16, cx, move |this, step, cx| {
                        let size = Settings::get(cx).for_file(this.language, None).tab_size as i64;
                        this.set_language_option(option, json!((size + step).clamp(1, 16)), cx);
                    })
                }
                _ => {
                    let on = option.flag(&effective);
                    div()
                        .id(("pref-lang-switch", option as usize))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_language_option(option, json!(!on), cx)))
                        .child(switch(on, &theme))
                        .into_any_element()
                }
            };
            let note: SharedString =
                if set { format!("Set for {}", language.name()).into() } else { "Same as the general setting".into() };
            editor_rows.push(setting_row(option.label(), Some(note), control, reset, &theme).into_any_element());
        }

        let tool_rows = |this: &mut Self, tool: Tool, window: &mut Window, cx: &mut Context<Self>| -> Vec<AnyElement> {
            let theme = Theme::get(cx).clone();
            let settings = Settings::get(cx).clone();
            let configured = tool_value(&settings, tool, language);
            let mode = if this.pending_custom == Some(tool) { ToolMode::Custom } else { ToolMode::of(configured.as_ref()) };
            let modes: Vec<ToolMode> = match tool {
                Tool::Server => vec![ToolMode::Automatic, ToolMode::Off, ToolMode::Custom],
                Tool::Formatter => vec![ToolMode::Automatic, ToolMode::LanguageServer, ToolMode::Off, ToolMode::Custom],
            };
            let options = modes.iter().map(|m| (SharedString::from(m.label()), *m == mode)).collect();
            let menu = match tool {
                Tool::Server => Menu::LanguageServer,
                Tool::Formatter => Menu::Formatter,
            };
            let picker = this.dropdown(menu, mode.label().into(), px(180.), options, cx, move |this, ix, window, cx| {
                this.set_tool_mode(tool, modes[ix], window, cx)
            });
            let (label, description) = tool_description(tool, language, mode);
            let mut rows = vec![setting_row(label, Some(description.into()), picker, None, &theme).into_any_element()];
            if mode == ToolMode::Custom {
                let field = match tool {
                    Tool::Server => &this.server_command,
                    Tool::Formatter => &this.formatter_command,
                };
                rows.push(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_4()
                        .py_2p5()
                        .border_t_1()
                        .border_color(theme.border_subtle)
                        .child(div().w(px(70.)).text_color(theme.text_muted).child("Command"))
                        .child(div().flex_1().min_w_0().flex().child(input_box(field, Some(Icon::Terminal), window, cx)))
                        .into_any_element(),
                );
            }
            rows
        };

        let mut sections = vec![section("Editor", editor_rows, &theme).into_any_element()];
        if servers::spec(language).is_some() {
            let rows = tool_rows(self, Tool::Server, window, cx);
            sections.push(section("Language server", rows, &theme).into_any_element());
        }
        if language != Language::PlainText {
            let rows = tool_rows(self, Tool::Formatter, window, cx);
            sections.push(section("Formatter", rows, &theme).into_any_element());
        }

        div()
            .flex()
            .flex_1()
            .min_h_0()
            .gap_5()
            .pl(px(28.))
            .child(
                div()
                    .id("pref-languages")
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .flex_shrink_0()
                    .w(px(150.))
                    .pb(px(28.))
                    .overflow_y_scroll()
                    .children(list),
            )
            .child(self.body().pl_0().child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(language.name()).mb(px(-12.))).children(sections))
    }
}

// ---- Building blocks -----------------------------------------------------------------

fn clone_event(event: &PreferencesEvent) -> PreferencesEvent {
    match event {
        PreferencesEvent::OpenSettingsFile => PreferencesEvent::OpenSettingsFile,
        PreferencesEvent::OpenKeymapFile => PreferencesEvent::OpenKeymapFile,
        PreferencesEvent::Problems(p) => PreferencesEvent::Problems(p.clone()),
        PreferencesEvent::Dismissed => PreferencesEvent::Dismissed,
    }
}

/// Section names of a page with settings rows, in order.
fn page_sections(page: Page) -> Vec<&'static str> {
    let mut sections: Vec<&'static str> = Vec::new();
    for setting in Setting::ALL.into_iter().filter(|s| s.page() == page) {
        if sections.last() != Some(&setting.section()) {
            sections.push(setting.section());
        }
    }
    sections
}

/// The `settings.json` key a row resets; empty for rows without one.
fn setting_key(setting: Setting) -> &'static str {
    match setting {
        Setting::Theme | Setting::RunConfigurations => "",
        Setting::FontFamily => "editor_font_family",
        Setting::FontSize => "editor_font_size",
        Setting::LineHeight => "editor_line_height",
        Setting::TabSize => "tab_size",
        Setting::HardTabs => "hard_tabs",
        Setting::SoftWrap => "soft_wrap",
        Setting::AutoSave => "auto_save",
        Setting::AutoSaveDelay => "auto_save_delay",
        Setting::FormatOnSave => "format_on_save",
        Setting::TrimWhitespace => "trim_trailing_whitespace",
        Setting::FinalNewline => "insert_final_newline",
        Setting::Shell => "terminal_shell",
    }
}

fn auto_save_label(mode: AutoSave) -> &'static str {
    match mode {
        AutoSave::Off => "Off",
        AutoSave::AfterDelay => "After a delay",
        AutoSave::OnFocusChange => "When switching tabs",
        AutoSave::OnWindowChange => "When leaving the window",
    }
}

fn tool_value(settings: &Settings, tool: Tool, language: Language) -> Option<Value> {
    let map = match tool {
        Tool::Server => &settings.language_servers,
        Tool::Formatter => &settings.formatters,
    };
    map.get(language.name()).cloned()
}

/// What Kin runs for `tool` unless told otherwise, as a command line.
fn default_command(tool: Tool, language: Language) -> Vec<String> {
    let first = match tool {
        Tool::Server => servers::spec(language).and_then(|s| s.candidates.first()),
        Tool::Formatter => format::candidates(language).first(),
    };
    first.map(|(name, args)| std::iter::once(*name).chain(args.iter().copied()).map(str::to_string).collect()).unwrap_or_default()
}

fn tool_description(tool: Tool, language: Language, mode: ToolMode) -> (&'static str, String) {
    match tool {
        Tool::Server => {
            let names: Vec<&str> = servers::spec(language).map(|s| s.candidates.iter().map(|(n, _)| *n).collect()).unwrap_or_default();
            let detail = match mode {
                ToolMode::Off => "No completions, diagnostics or navigation from a server.".to_string(),
                ToolMode::Custom => "Runs this command in the project; it talks LSP over stdin and stdout.".to_string(),
                _ => format!("The first one installed: {}.", names.join(", ")),
            };
            ("Which server", format!("{detail} Open files pick this up after Restart Language Server."))
        }
        Tool::Formatter => {
            let names: Vec<&str> = format::candidates(language).iter().map(|(n, _)| *n).collect();
            let detail = match mode {
                ToolMode::Off => "Reformat Code does nothing for this language.".to_string(),
                ToolMode::LanguageServer => "Always asks the language server.".to_string(),
                ToolMode::Custom => "Reads the file on stdin and writes the result to stdout; {file} is its path.".to_string(),
                ToolMode::Automatic if names.is_empty() => "The language server, if it can format.".to_string(),
                ToolMode::Automatic => format!("The project's own {}, then the language server, then one on PATH.", names.join(" or ")),
            };
            ("Which formatter", detail)
        }
    }
}

fn strings(items: &[Value]) -> Vec<String> {
    items.iter().filter_map(Value::as_str).map(str::to_string).collect()
}

fn set_field(field: &Entity<Editor>, text: &str, cx: &mut App) {
    field.update(cx, |e, cx| {
        e.set_text(text, cx);
        e.select_range(text.len()..text.len(), cx);
    });
}

/// Why `settings.json` can't be edited (it doesn't parse), if it can't.
fn settings_file_error() -> Option<String> {
    let text = Settings::path().and_then(|p| std::fs::read_to_string(p).ok())?;
    if text.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(&settings::strip_jsonc(&text)) {
        Ok(Value::Object(_)) => None,
        Ok(_) => Some("it should contain an object".into()),
        Err(err) => Some(err.to_string()),
    }
}

/// Installed font families whose letters are all one width.
fn monospaced_fonts(text_system: &gpui::TextSystem) -> Vec<SharedString> {
    let mut names = text_system.all_font_names();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    names
        .into_iter()
        // macOS lists private system fonts with a leading dot.
        .filter(|name| !name.starts_with('.'))
        .filter(|name| {
            let id = text_system.resolve_font(&gpui::font(name.clone()));
            let size = px(16.);
            match (text_system.advance(id, size, 'i'), text_system.advance(id, size, 'W')) {
                (Ok(narrow), Ok(wide)) => narrow.width > px(0.) && (narrow.width - wide.width).abs() < px(0.01),
                _ => false,
            }
        })
        .map(SharedString::from)
        .collect()
}

/// Splits a command line into words, honoring quotes ("my app" --flag).
pub fn split_command(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    word.push(next);
                    in_word = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// The reverse of [`split_command`]: quotes words that need it.
pub fn join_command(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            if !w.is_empty() && !w.contains(|c: char| c.is_whitespace() || "\"'\\".contains(c)) {
                w.clone()
            } else {
                format!("\"{}\"", w.replace('\\', "\\\\").replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A group of rows under a small heading.
fn section(title: &'static str, rows: Vec<AnyElement>, theme: &Theme) -> Div {
    let rows = rows.into_iter().enumerate().map(|(ix, row)| {
        div().when(ix > 0, |el| el.border_t_1().border_color(theme.border_subtle)).child(row)
    });
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .px_1()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_dim)
                .child(title.to_uppercase()),
        )
        .child(card(theme).children(rows))
}

fn card(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(10.))
        .bg(theme.elevated)
        .border_1()
        .border_color(theme.border_subtle)
}

/// Label and description on the left, the control on the right.
fn setting_row(
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    control: AnyElement,
    reset: Option<AnyElement>,
    theme: &Theme,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap_4()
        .min_h(px(ROW_HEIGHT))
        .px_4()
        .py_2p5()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .flex_1()
                .min_w_0()
                // A changed option has a reset button right after its name.
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .h(px(20.))
                        .child(div().text_color(theme.text).child(label.into()))
                        .children(reset),
                )
                .when_some(description, |el, d| el.child(div().text_size(px(12.)).text_color(theme.text_dim).child(d))),
        )
        .child(div().flex_shrink_0().child(control))
}

fn dropdown_trigger(id: impl Into<gpui::ElementId>, label: SharedString, width: Pixels, open: bool, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .w(width)
        .h(px(30.))
        .pl_2p5()
        .pr_2()
        .rounded(px(7.))
        .border_1()
        .border_color(if open { Hsla::from(theme.accent).alpha(0.7) } else { theme.border.into() })
        .bg(theme.background)
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover))
        .child(div().flex_1().min_w_0().truncate().text_color(theme.text).child(label))
        .child(icon_el(Icon::ChevronDown, px(13.), theme.text_dim.into()))
}

/// A row that goes somewhere (search results pointing into a page).
fn jump_row(
    id: impl Into<gpui::ElementId>,
    icon: Icon,
    label: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_3()
        .h(px(44.))
        .px_4()
        .rounded(px(9.))
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover))
        .child(icon_el(icon, px(14.), theme.text_dim.into()))
        .child(div().text_color(theme.text).child(label.into()))
        .child(div().flex_1().text_size(px(12.)).text_color(theme.text_dim).child(detail.into()))
        .child(icon_el(Icon::ChevronRight, px(13.), theme.text_dim.into()))
}

fn link_row(id: &'static str, icon: Icon, label: &'static str, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .h(px(28.))
        .px_2()
        .rounded(px(6.))
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(theme.text_muted)
        .hover(|s| s.bg(theme.hover).text_color(theme.text))
        .child(icon_el(icon, px(13.), theme.text_dim.into()))
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_and_joins_commands() {
        assert_eq!(split_command("  pylsp  "), vec!["pylsp"]);
        assert_eq!(split_command(r#"black --quiet "-""#), vec!["black", "--quiet", "-"]);
        assert_eq!(split_command(r#""/Applications/My App/bin" --x 'a b'"#), vec!["/Applications/My App/bin", "--x", "a b"]);
        assert_eq!(split_command(r#"a\ b "c\"d" """#), vec!["a b", "c\"d", ""]);
        assert!(split_command("   ").is_empty());
        for words in [vec!["fish", "--login"], vec!["/My App/x", "say \"hi\"", ""], vec!["{file}"]] {
            let words: Vec<String> = words.into_iter().map(str::to_string).collect();
            assert_eq!(split_command(&join_command(&words)), words);
        }
    }

    #[test]
    fn search_finds_settings_by_any_word() {
        let find = |q: &str| {
            let words: Vec<String> = q.split_whitespace().map(str::to_lowercase).collect();
            Setting::ALL.into_iter().filter(|s| s.matches(&words)).collect::<Vec<_>>()
        };
        assert_eq!(find("tab size"), vec![Setting::TabSize]);
        assert!(find("font").contains(&Setting::FontFamily) && find("font").contains(&Setting::LineHeight));
        assert_eq!(find("zsh"), vec![Setting::Shell]);
        assert!(find("nothing like this").is_empty());
    }
}
