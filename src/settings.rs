//! User preferences in `settings.json`. The file may contain comments and
//! trailing commas; when Kin changes a key (Preferences, the theme picker) it
//! rewrites only that key's entry, so comments and formatting survive.
//!
//! Editor options resolve per file: built-in language defaults, then the
//! top-level settings, then `languages.<Name>` overrides, then `.editorconfig`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::SystemTime,
};

use gpui::{App, Global};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    editorconfig::EditorConfig,
    keymap,
    syntax::Language,
    theme::{self, config_dir},
};

#[derive(Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Settings {
    pub theme: Option<String>,
    /// The shortcuts Kin starts from; `keymap.json` goes on top.
    pub base_keymap: Option<BaseKeymap>,
    /// When edited files are written to disk without an explicit save.
    pub auto_save: Option<AutoSave>,
    /// Idle time in milliseconds before `after_delay` saves.
    pub auto_save_delay: Option<u64>,
    /// A font installed on the system; falls back to the built-in choice.
    pub editor_font_family: Option<String>,
    pub editor_font_size: Option<f32>,
    /// Line height as a multiple of the font size.
    pub editor_line_height: Option<f32>,
    #[serde(flatten)]
    pub editor: EditorSettings,
    /// Overrides by language name as shown in the status bar ("Go", "YAML").
    pub languages: HashMap<String, EditorSettings>,
    /// Per language: `false` to turn its language server off, or a command
    /// (`["pylsp"]`) to use instead of the built-in choice.
    pub language_servers: HashMap<String, serde_json::Value>,
    /// Per language: `"language_server"`, a command reading stdin and writing
    /// stdout (`["black", "-q", "-"]`), or `false` to never format.
    pub formatters: HashMap<String, serde_json::Value>,
    /// The terminal's shell and arguments; the login shell by default.
    pub terminal_shell: Option<Vec<String>>,
    /// Extra entries for Run…: a name, a shell command and an optional
    /// folder relative to the project.
    pub run_configurations: Vec<RunConfiguration>,
    /// Download new versions from GitHub in the background (on by default).
    pub auto_update: Option<bool>,
}

#[derive(Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct RunConfiguration {
    pub name: String,
    pub command: String,
    pub cwd: Option<String>,
}

impl Global for Settings {}

/// Built-in keymaps, named as in Zed's `base_keymap` setting.
#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BaseKeymap {
    #[default]
    JetBrains,
    #[serde(alias = "VS Code", alias = "vscode")]
    VSCode,
    #[serde(alias = "Sublime Text", alias = "sublime")]
    SublimeText,
    Zed,
}

impl BaseKeymap {
    pub const ALL: [BaseKeymap; 4] = [BaseKeymap::JetBrains, BaseKeymap::VSCode, BaseKeymap::SublimeText, BaseKeymap::Zed];

    /// The value in `settings.json`.
    pub fn key(self) -> &'static str {
        match self {
            BaseKeymap::JetBrains => "JetBrains",
            BaseKeymap::VSCode => "VSCode",
            BaseKeymap::SublimeText => "SublimeText",
            BaseKeymap::Zed => "Zed",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BaseKeymap::JetBrains => "JetBrains",
            BaseKeymap::VSCode => "VS Code",
            BaseKeymap::SublimeText => "Sublime Text",
            BaseKeymap::Zed => "Zed",
        }
    }
}

/// Auto-save modes, named like VS Code's `files.autoSave`. Every mode but
/// `off` also saves when the window loses focus, a tab is closed and on quit.
#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum AutoSave {
    Off,
    /// Shortly after typing stops (the default).
    #[default]
    AfterDelay,
    /// When switching tabs or leaving the window.
    OnFocusChange,
    /// Only when leaving the window.
    OnWindowChange,
}

impl AutoSave {
    pub fn key(self) -> &'static str {
        match self {
            AutoSave::Off => "off",
            AutoSave::AfterDelay => "after_delay",
            AutoSave::OnFocusChange => "on_focus_change",
            AutoSave::OnWindowChange => "on_window_change",
        }
    }
}

/// Options that can differ per language and per `.editorconfig` section.
#[derive(Deserialize, Default, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct EditorSettings {
    /// Columns per indent level and per tab character.
    pub tab_size: Option<usize>,
    /// Indent with tabs instead of spaces (when the file doesn't show a style).
    pub hard_tabs: Option<bool>,
    pub soft_wrap: Option<bool>,
    /// Remove spaces at line ends on save.
    pub trim_trailing_whitespace: Option<bool>,
    /// End the file with a newline on save.
    pub insert_final_newline: Option<bool>,
    /// Reformat when saving with Save (not auto-save).
    pub format_on_save: Option<bool>,
}

impl EditorSettings {
    /// `self`, with unset options taken from `base`.
    fn or(self, base: EditorSettings) -> EditorSettings {
        EditorSettings {
            tab_size: self.tab_size.or(base.tab_size),
            hard_tabs: self.hard_tabs.or(base.hard_tabs),
            soft_wrap: self.soft_wrap.or(base.soft_wrap),
            trim_trailing_whitespace: self.trim_trailing_whitespace.or(base.trim_trailing_whitespace),
            insert_final_newline: self.insert_final_newline.or(base.insert_final_newline),
            format_on_save: self.format_on_save.or(base.format_on_save),
        }
    }

    /// What Kin does for a language unless told otherwise.
    fn language_default(language: Language) -> EditorSettings {
        let mut s = EditorSettings::default();
        match language {
            // gofmt indents with tabs.
            Language::Go => s.hard_tabs = Some(true),
            // The usual style for markup and data files (and npm's package.json).
            Language::Html | Language::Json | Language::Yaml => s.tab_size = Some(2),
            // Prose reads better wrapped; code keeps its lines (JetBrains does the same).
            Language::Markdown | Language::PlainText => s.soft_wrap = Some(true),
            _ => {}
        }
        s
    }
}

const TOP_LEVEL_KEYS: &[&str] = &[
    "theme",
    "base_keymap",
    "auto_save",
    "auto_save_delay",
    "editor_font_family",
    "editor_font_size",
    "editor_line_height",
    "languages",
    "language_servers",
    "formatters",
    "terminal_shell",
    "run_configurations",
    "auto_update",
];

const EDITOR_KEYS: &[&str] = &[
    "tab_size",
    "hard_tabs",
    "soft_wrap",
    "trim_trailing_whitespace",
    "insert_final_newline",
    "format_on_save",
];

/// Editor options resolved for one file.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FileSettings {
    pub tab_size: usize,
    pub hard_tabs: bool,
    /// The indent style came from `.editorconfig`, so it beats detection.
    pub indent_is_explicit: bool,
    pub soft_wrap: bool,
    pub trim_trailing_whitespace: bool,
    pub insert_final_newline: bool,
    pub format_on_save: bool,
}

impl Settings {
    pub fn get(cx: &App) -> &Settings {
        cx.try_global::<Settings>().unwrap_or_else(|| {
            static DEFAULT: std::sync::LazyLock<Settings> = std::sync::LazyLock::new(Settings::default);
            &DEFAULT
        })
    }

    pub fn path() -> Option<PathBuf> {
        config_dir().map(|d| d.join("settings.json"))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&strip_jsonc(text)).map_err(|e| e.to_string())
    }

    /// Top-level and per-language keys that aren't settings (likely typos),
    /// each with a suggestion when one is close.
    pub fn unknown_keys(text: &str) -> Vec<String> {
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&strip_jsonc(text)) else {
            return Vec::new();
        };
        let describe = |key: &str, known: &[&'static str]| match theme::closest(key, known.iter().copied()) {
            Some(near) => format!("unknown setting \"{key}\" (did you mean \"{near}\"?)"),
            None => format!("unknown setting \"{key}\""),
        };
        let mut problems = Vec::new();
        for (key, value) in &map {
            if !TOP_LEVEL_KEYS.contains(&key.as_str()) && !EDITOR_KEYS.contains(&key.as_str()) {
                let all: Vec<&'static str> = TOP_LEVEL_KEYS.iter().chain(EDITOR_KEYS).copied().collect();
                problems.push(describe(key, &all));
            }
            if key == "languages"
                && let Some(languages) = value.as_object()
            {
                for overrides in languages.values().filter_map(|v| v.as_object()) {
                    for key in overrides.keys().filter(|k| !EDITOR_KEYS.contains(&k.as_str())) {
                        problems.push(describe(key, EDITOR_KEYS));
                    }
                }
            }
        }
        problems
    }

    /// Sets one top-level key in `settings.json`, keeping everything else.
    pub fn set(key: &str, value: Value) -> std::io::Result<()> {
        Self::update(key, |_| Some(value))
    }

    /// Removes a top-level key, so the option goes back to its default.
    pub fn remove(key: &str) -> std::io::Result<()> {
        Self::update(key, |_| None)
    }

    /// Rewrites one top-level key from its current value in the file (`None`
    /// when absent); returning `None` removes it. A file that doesn't parse is
    /// left alone, so a typo can't turn into lost settings.
    pub fn update(key: &str, f: impl FnOnce(Option<Value>) -> Option<Value>) -> std::io::Result<()> {
        let path = Self::path().ok_or_else(|| std::io::Error::other("no config directory"))?;
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let current = if text.trim().is_empty() {
            None
        } else {
            match serde_json::from_str::<Value>(&strip_jsonc(&text)) {
                Ok(Value::Object(mut map)) => map.remove(key),
                Ok(_) => return Err(std::io::Error::other("settings.json should contain an object")),
                Err(err) => return Err(std::io::Error::other(format!("settings.json has an error: {err}"))),
            }
        };
        let updated = match f(current) {
            Some(value) => {
                let text = if text.trim().is_empty() { "{\n}\n".to_string() } else { text.clone() };
                set_top_level_key(&text, key, &format_value(&value, 2))
            }
            None => remove_top_level_key(&text, key),
        };
        if updated == text {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, updated)
    }

    /// Whether `key` is set in the top level of `settings.json`.
    pub fn is_set(&self, key: &str) -> bool {
        match key {
            "theme" => self.theme.is_some(),
            "base_keymap" => self.base_keymap.is_some(),
            "auto_save" => self.auto_save.is_some(),
            "auto_save_delay" => self.auto_save_delay.is_some(),
            "editor_font_family" => self.editor_font_family.is_some(),
            "editor_font_size" => self.editor_font_size.is_some(),
            "editor_line_height" => self.editor_line_height.is_some(),
            "tab_size" => self.editor.tab_size.is_some(),
            "hard_tabs" => self.editor.hard_tabs.is_some(),
            "soft_wrap" => self.editor.soft_wrap.is_some(),
            "trim_trailing_whitespace" => self.editor.trim_trailing_whitespace.is_some(),
            "insert_final_newline" => self.editor.insert_final_newline.is_some(),
            "format_on_save" => self.editor.format_on_save.is_some(),
            "terminal_shell" => self.terminal_shell.is_some(),
            "run_configurations" => !self.run_configurations.is_empty(),
            "auto_update" => self.auto_update.is_some(),
            _ => false,
        }
    }

    /// Sets (`Some`) or clears one option in `languages.<language>`.
    pub fn set_language_option(language: Language, key: &str, value: Option<Value>) -> std::io::Result<()> {
        Self::update_language_entry("languages", language, |entry| {
            let mut options = entry.and_then(|v| v.as_object().cloned()).unwrap_or_default();
            match value {
                Some(value) => options.insert(key.to_string(), value),
                None => options.shift_remove(key),
            };
            (!options.is_empty()).then_some(Value::Object(options))
        })
    }

    /// Rewrites `<key>.<language name>` in a per-language map (`languages`,
    /// `language_servers`, `formatters`), dropping the map once it's empty.
    pub fn update_language_entry(
        key: &str,
        language: Language,
        f: impl FnOnce(Option<Value>) -> Option<Value>,
    ) -> std::io::Result<()> {
        Self::update(key, |map| {
            let mut map = map.and_then(|v| v.as_object().cloned()).unwrap_or_default();
            let name = language.name().to_string();
            // Replacing in place keeps the language where it was in the file.
            match f(map.get(&name).cloned()) {
                Some(value) => {
                    map.insert(name, value);
                }
                None => {
                    map.shift_remove(&name);
                }
            }
            (!map.is_empty()).then_some(Value::Object(map))
        })
    }

    /// Editor options for a file of `language` at `path`.
    pub fn for_file(&self, language: Language, path: Option<&Path>) -> FileSettings {
        let language_overrides = self.languages.get(language.name()).copied().unwrap_or_default();
        let config = path.map(EditorConfig::for_file).unwrap_or_default();
        let from_config = EditorSettings {
            tab_size: config.indent_size.or(config.tab_width),
            hard_tabs: config.hard_tabs,
            soft_wrap: None,
            trim_trailing_whitespace: config.trim_trailing_whitespace,
            insert_final_newline: config.insert_final_newline,
            format_on_save: None,
        };
        let s = from_config
            .or(language_overrides)
            .or(self.editor)
            .or(EditorSettings::language_default(language));
        FileSettings {
            tab_size: s.tab_size.unwrap_or(4).clamp(1, 16),
            hard_tabs: s.hard_tabs.unwrap_or(false),
            indent_is_explicit: config.hard_tabs.is_some(),
            soft_wrap: s.soft_wrap.unwrap_or(false),
            trim_trailing_whitespace: s.trim_trailing_whitespace.unwrap_or(false),
            insert_final_newline: s.insert_final_newline.unwrap_or(false),
            format_on_save: s.format_on_save.unwrap_or(false),
        }
    }
}

// ---- Loading and live reload ------------------------------------------------------

/// When `settings.json` and `keymap.json` were last applied.
#[derive(Default)]
struct ConfigStamps {
    settings: Option<SystemTime>,
    keymap: Option<SystemTime>,
}

impl Global for ConfigStamps {}

fn modified(path: Option<PathBuf>) -> Option<SystemTime> {
    std::fs::metadata(path?).ok()?.modified().ok()
}

fn settings_problems() -> Result<Settings, Vec<String>> {
    let text = Settings::path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    match Settings::parse(&text) {
        Ok(settings) => Ok(settings),
        Err(err) => Err(vec![format!("settings.json: {err}")]),
    }
}

/// Loads the settings and key bindings at startup. Returns problems with
/// the user's files, for the first window to show.
pub fn init(cx: &mut App) -> Vec<String> {
    let mut problems = Vec::new();
    let settings = settings_problems().unwrap_or_else(|p| {
        problems.extend(p);
        Settings::default()
    });
    problems.extend(unknown_key_problems());
    cx.set_global(settings);
    problems.extend(keymap::install(cx));
    cx.set_global(ConfigStamps { settings: modified(Settings::path()), keymap: modified(keymap::keymap_path()) });
    problems
}

fn unknown_key_problems() -> Vec<String> {
    let text = Settings::path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    Settings::unknown_keys(&text).into_iter().map(|p| format!("settings.json: {p}")).collect()
}

/// Re-applies `settings.json` and `keymap.json` if they changed on disk since
/// they were last applied (after saving them in Kin, or editing elsewhere).
/// A broken file keeps the previous state. Returns problems to report.
pub fn reload_if_changed(cx: &mut App) -> Vec<String> {
    let (settings_time, keymap_time) = (modified(Settings::path()), modified(keymap::keymap_path()));
    let (old_settings, old_keymap) =
        cx.try_global::<ConfigStamps>().map_or((None, None), |s| (s.settings, s.keymap));
    let mut problems = Vec::new();
    if settings_time != old_settings {
        match settings_problems() {
            Ok(settings) => {
                problems.extend(unknown_key_problems());
                let theme_changed = settings.theme != Settings::get(cx).theme;
                cx.set_global(settings.clone());
                // No theme set (any more) means the default one.
                if theme_changed {
                    let name = settings.theme.as_deref().unwrap_or(theme::DEFAULT_THEME);
                    if !theme::apply_theme(name, cx) {
                        problems.push(format!("settings.json: there's no theme named \"{name}\""));
                    }
                }
                theme::apply_fonts(cx);
            }
            Err(p) => problems.extend(p),
        }
    }
    // A different base keymap needs the bindings rebuilt too.
    let installed = cx.try_global::<keymap::Installed>().map(|i| i.0);
    if keymap_time != old_keymap || installed != Some(Settings::get(cx).base_keymap.unwrap_or_default()) {
        problems.extend(keymap::install(cx));
    }
    cx.set_global(ConfigStamps { settings: settings_time, keymap: keymap_time });
    problems
}

/// Applies `settings.json` now, after Kin changed it itself (a quick second
/// write can share the first one's modified time).
pub fn reload_now(cx: &mut App) -> Vec<String> {
    let keymap = cx.try_global::<ConfigStamps>().and_then(|s| s.keymap);
    cx.set_global(ConfigStamps { settings: None, keymap });
    reload_if_changed(cx)
}

/// Applies `keymap.json` now, after Kin changed it itself.
pub fn reload_keymap_now(cx: &mut App) -> Vec<String> {
    let settings = cx.try_global::<ConfigStamps>().and_then(|s| s.settings);
    cx.set_global(ConfigStamps { settings, keymap: None });
    reload_if_changed(cx)
}

/// Problems found at startup, shown by the first window and then cleared.
#[derive(Default)]
pub struct StartupProblems(pub Vec<String>);

impl Global for StartupProblems {}

/// Written by Open Settings when there's no file yet: every option, commented
/// out at its default.
pub const SETTINGS_TEMPLATE: &str = r#"// Kin settings. Changes apply when you save this file.
// Comments and trailing commas are allowed.
{
  // "theme": "Monokai Pro",

  // The shortcuts to start from: "JetBrains", "VSCode", "SublimeText" or "Zed".
  // keymap.json goes on top.
  // "base_keymap": "JetBrains",

  // "off", "after_delay", "on_focus_change" or "on_window_change"
  // "auto_save": "after_delay",
  // "auto_save_delay": 1000,

  // "editor_font_family": "JetBrains Mono",
  // "editor_font_size": 14,
  // Line height as a multiple of the font size.
  // "editor_line_height": 1.6,

  // Columns per indent level and per tab. A file's own indentation and
  // .editorconfig take precedence.
  // "tab_size": 4,
  // "hard_tabs": false,
  // "soft_wrap": false,
  // "trim_trailing_whitespace": false,
  // "insert_final_newline": false,
  // Reformat on Save (auto-save doesn't reformat while you type).
  // "format_on_save": false,

  // Per-language overrides, by the name shown in the status bar.
  // "languages": {
  //   "Markdown": { "soft_wrap": true },
  //   "Python": { "tab_size": 4 }
  // },

  // Language servers are found on PATH or in the project's node_modules.
  // false turns one off; a command replaces the built-in choice.
  // "language_servers": {
  //   "Python": ["pylsp"],
  //   "Markdown": false
  // },

  // Reformat Code uses the project's own formatter (e.g. its Prettier), then
  // the language server, then a formatter on PATH. To choose: a command that
  // reads stdin and writes stdout ({file} is the file's path),
  // "language_server", or false.
  // "formatters": {
  //   "Python": ["black", "--quiet", "-"],
  //   "TypeScript": "language_server"
  // },

  // The terminal's shell; your login shell by default.
  // "terminal_shell": ["/opt/homebrew/bin/fish", "--login"],

  // More entries for Run… (detected ones include cargo, npm scripts, go,
  // make and pytest).
  // "run_configurations": [
  //   { "name": "Serve docs", "command": "mkdocs serve", "cwd": "docs" }
  // ],

  // Download new versions in the background; Restart to Update installs them.
  // "auto_update": true
}
"#;

// ---- JSON with comments ---------------------------------------------------------

/// Turns JSON with comments and trailing commas into plain JSON. Removed
/// characters become spaces, so error positions still match the original.
pub fn strip_jsonc(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = text.as_bytes().to_vec();
    let mut i = 0;
    // Index of the last comma not yet followed by a value.
    let mut pending_comma: Option<usize> = None;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                pending_comma = None;
                i = skip_string(bytes, i);
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let end = text[i + 2..].find("*/").map_or(bytes.len(), |e| i + 2 + e + 2);
                for b in &mut out[i..end] {
                    if *b != b'\n' {
                        *b = b' ';
                    }
                }
                i = end;
                continue;
            }
            b',' => pending_comma = Some(i),
            b'}' | b']' => {
                if let Some(c) = pending_comma.take() {
                    out[c] = b' ';
                }
            }
            b if b.is_ascii_whitespace() => {}
            _ => pending_comma = None,
        }
        i += 1;
    }
    // Only ASCII bytes were replaced, so this stays valid UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// The index just past the string literal starting at `start`.
fn skip_string(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// Skips whitespace and comments.
fn skip_trivia(text: &str, mut i: usize) -> usize {
    let bytes = text.as_bytes();
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if text[i..].starts_with("//") {
            i = text[i..].find('\n').map_or(bytes.len(), |e| i + e);
        } else if text[i..].starts_with("/*") {
            i = text[i + 2..].find("*/").map_or(bytes.len(), |e| i + 2 + e + 2);
        } else {
            return i;
        }
    }
}

/// The end of the JSON value starting at `start`.
fn skip_value(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i = skip_string(bytes, i);
                if depth == 0 {
                    return i;
                }
                continue;
            }
            b'/' if matches!(bytes.get(i + 1), Some(b'/' | b'*')) => {
                i = skip_trivia(text, i);
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            b',' if depth == 0 => return i,
            b if depth == 0 && b.is_ascii_whitespace() => return i,
            _ => {}
        }
        i += 1;
    }
    i
}

/// Formats a value for `settings.json`: short lists and objects of plain
/// values on one line, anything nested spread over lines and indented to
/// sit under a key at `indent` columns.
pub fn format_value(value: &Value, indent: usize) -> String {
    let plain = |v: &Value| !matches!(v, Value::Array(_) | Value::Object(_));
    let inline = match value {
        Value::Array(items) if items.is_empty() => return "[]".into(),
        Value::Object(map) if map.is_empty() => return "{}".into(),
        Value::Array(items) if items.iter().all(plain) => {
            Some(format!("[{}]", items.iter().map(Value::to_string).collect::<Vec<_>>().join(", ")))
        }
        Value::Object(map) if map.values().all(plain) => {
            let entries: Vec<String> = map.iter().map(|(k, v)| format!("{}: {v}", Value::from(k.as_str()))).collect();
            Some(format!("{{ {} }}", entries.join(", ")))
        }
        Value::Array(_) | Value::Object(_) => None,
        plain => return plain.to_string(),
    };
    if let Some(line) = inline.filter(|l| indent + l.len() <= 90) {
        return line;
    }
    let pad = " ".repeat(indent + 2);
    let close = " ".repeat(indent);
    match value {
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(|v| format!("{pad}{}", format_value(v, indent + 2))).collect();
            format!("[\n{}\n{close}]", items.join(",\n"))
        }
        Value::Object(map) => {
            let entries: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{pad}{}: {}", Value::from(k.as_str()), format_value(v, indent + 2)))
                .collect();
            format!("{{\n{}\n{close}}}", entries.join(",\n"))
        }
        _ => unreachable!("plain values returned above"),
    }
}

/// Removes `"key": value` from the top-level object of a JSONC document,
/// with its comma and, when it had the line to itself, its line.
pub fn remove_top_level_key(text: &str, key: &str) -> String {
    let bytes = text.as_bytes();
    let open = skip_trivia(text, 0);
    if bytes.get(open) != Some(&b'{') {
        return text.to_string();
    }
    let mut i = open + 1;
    let mut previous_end = None;
    loop {
        i = skip_trivia(text, i);
        if bytes.get(i) != Some(&b'"') {
            return text.to_string();
        }
        let key_start = i;
        let key_end = skip_string(bytes, i);
        let found: Option<String> = serde_json::from_str(&text[i..key_end]).ok();
        let colon = skip_trivia(text, key_end);
        if bytes.get(colon) != Some(&b':') {
            return text.to_string();
        }
        let value_start = skip_trivia(text, colon + 1);
        let value_end = skip_value(text, value_start);
        let after = skip_trivia(text, value_end);
        if found.as_deref() == Some(key) {
            let mut start = key_start;
            let mut end = value_end;
            // The comma that separated this entry goes too: its own, or for
            // the last entry the previous one's.
            let mut stray_comma = None;
            if bytes.get(after) == Some(&b',') {
                end = after + 1;
                while matches!(bytes.get(end), Some(b' ' | b'\t')) {
                    end += 1;
                }
            } else if let Some(previous) = previous_end {
                let comma = skip_trivia(text, previous);
                if bytes.get(comma) == Some(&b',') {
                    stray_comma = Some(comma);
                }
            }
            let line_start = text[..start].rfind('\n').map_or(0, |n| n + 1);
            let line_end = text[end..].find('\n').map_or(text.len(), |n| end + n);
            if text[line_start..start].trim().is_empty() && text[end..line_end].trim().is_empty() {
                start = line_start;
                end = (line_end + 1).min(text.len());
            }
            return match stray_comma {
                Some(c) => format!("{}{}{}", &text[..c], &text[c + 1..start], &text[end..]),
                None => format!("{}{}", &text[..start], &text[end..]),
            };
        }
        previous_end = Some(value_end);
        i = after;
        if bytes.get(i) == Some(&b',') {
            i += 1;
        }
    }
}

/// Replaces (or adds) `"key": value` in the top-level object of a JSONC
/// document, leaving the rest of the text untouched.
pub fn set_top_level_key(text: &str, key: &str, value: &str) -> String {
    let bytes = text.as_bytes();
    let Some(open) = (|| {
        let i = skip_trivia(text, 0);
        (bytes.get(i) == Some(&b'{')).then_some(i)
    })() else {
        return format!("{{\n  {}: {value}\n}}\n", serde_json::Value::from(key));
    };
    let mut i = open + 1;
    let mut last_value_end = None;
    loop {
        i = skip_trivia(text, i);
        match bytes.get(i) {
            Some(b'"') => {
                let key_end = skip_string(bytes, i);
                let found: Option<String> = serde_json::from_str(&text[i..key_end]).ok();
                let colon = skip_trivia(text, key_end);
                if bytes.get(colon) != Some(&b':') {
                    break;
                }
                let value_start = skip_trivia(text, colon + 1);
                let value_end = skip_value(text, value_start);
                if found.as_deref() == Some(key) {
                    return format!("{}{value}{}", &text[..value_start], &text[value_end..]);
                }
                last_value_end = Some(value_end);
                i = skip_trivia(text, value_end);
                if bytes.get(i) == Some(&b',') {
                    i += 1;
                }
            }
            Some(b'}') => break,
            _ => break,
        }
    }
    let entry = format!("{}: {value}", serde_json::Value::from(key));
    match last_value_end {
        // After the last entry; add a comma unless one is already there.
        Some(end) => {
            let after = skip_trivia(text, end);
            if bytes.get(after) == Some(&b',') {
                format!("{}\n  {entry},{}", &text[..after + 1], &text[after + 1..])
            } else {
                // Keep a comment that ends the last entry's line on that line.
                let line_end = text[end..].find('\n').map_or(text.len(), |n| end + n);
                let rest = text[end..line_end].trim();
                if rest.is_empty() || rest.starts_with("//") {
                    format!("{},{}\n  {entry}{}", &text[..end], &text[end..line_end], &text[line_end..])
                } else {
                    format!("{},\n  {entry}{}", &text[..end], &text[end..])
                }
            }
        }
        // An object with no entries (maybe only comments): add before `}`.
        None => {
            let close = text[open..].rfind('}').map_or(text.len(), |c| open + c);
            let before = text[..close].trim_end_matches([' ', '\t']);
            let newline = if before.ends_with('\n') { "" } else { "\n" };
            format!("{before}{newline}  {entry}\n{}", &text[close..])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonc() {
        let text = r#"// Settings
{
  "theme": "Nord", // inline
  /* block */ "tab_size": 2,
  "languages": { "Go": { "tab_size": 8, }, },
}"#;
        let s = Settings::parse(text).unwrap();
        assert_eq!(s.theme.as_deref(), Some("Nord"));
        assert_eq!(s.editor.tab_size, Some(2));
        assert_eq!(s.languages["Go"].tab_size, Some(8));
        // Comment markers inside strings are text.
        assert_eq!(Settings::parse(r#"{"theme": "a // b /* c"}"#).unwrap().theme.as_deref(), Some("a // b /* c"));
        assert!(Settings::parse("{ \"tab_size\": \"x\" }").is_err());
        assert!(Settings::parse(SETTINGS_TEMPLATE).unwrap() == Settings::default());
    }

    #[test]
    fn reports_unknown_keys() {
        let problems = Settings::unknown_keys(r#"{"tabsize": 2, "theme": "x", "languages": {"Go": {"hard_tab": true}}}"#);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.contains(&"unknown setting \"tabsize\" (did you mean \"tab_size\"?)".to_string()));
        assert!(problems.contains(&"unknown setting \"hard_tab\" (did you mean \"hard_tabs\"?)".to_string()));
        assert!(Settings::unknown_keys(SETTINGS_TEMPLATE).is_empty());
    }

    #[test]
    fn sets_keys_keeping_comments() {
        let text = "{\n  // my theme\n  \"theme\": \"Nord\",\n  \"tab_size\": 2 // two\n}\n";
        let replaced = set_top_level_key(text, "theme", "\"Dracula\"");
        assert_eq!(replaced, "{\n  // my theme\n  \"theme\": \"Dracula\",\n  \"tab_size\": 2 // two\n}\n");
        let added = set_top_level_key(text, "auto_save", "\"off\"");
        assert_eq!(added, "{\n  // my theme\n  \"theme\": \"Nord\",\n  \"tab_size\": 2, // two\n  \"auto_save\": \"off\"\n}\n");
        assert_eq!(Settings::parse(&added).unwrap().auto_save, Some(AutoSave::Off));
        // Nested keys with the same name are left alone.
        let nested = "{\"languages\": {\"Go\": {\"theme\": 1}}, \"theme\": \"A\"}";
        assert_eq!(set_top_level_key(nested, "theme", "\"B\""), "{\"languages\": {\"Go\": {\"theme\": 1}}, \"theme\": \"B\"}");
        // The commented-out template gains a real entry.
        let from_template = set_top_level_key(SETTINGS_TEMPLATE, "theme", "\"Nord\"");
        assert_eq!(Settings::parse(&from_template).unwrap().theme.as_deref(), Some("Nord"));
        assert!(from_template.contains("// \"auto_save_delay\": 1000"));
        // A trailing comma is reused.
        let trailing = set_top_level_key("{\n  \"a\": 1,\n}\n", "b", "2");
        assert_eq!(Settings::parse(&trailing).unwrap(), Settings::default());
        assert!(trailing.contains("\"b\": 2"), "{trailing}");
        assert_eq!(set_top_level_key("", "theme", "\"A\""), "{\n  \"theme\": \"A\"\n}\n");
    }

    #[test]
    fn removes_keys_keeping_the_rest() {
        let text = "{\n  // my theme\n  \"theme\": \"Nord\",\n  \"tab_size\": 2, // two\n  \"soft_wrap\": true\n}\n";
        assert_eq!(remove_top_level_key(text, "theme"), "{\n  // my theme\n  \"tab_size\": 2, // two\n  \"soft_wrap\": true\n}\n");
        // The last entry takes the previous entry's comma with it.
        let last = remove_top_level_key(text, "soft_wrap");
        assert_eq!(last, "{\n  // my theme\n  \"theme\": \"Nord\",\n  \"tab_size\": 2 // two\n}\n");
        assert!(Settings::parse(&last).is_ok());
        // A comment after the comma stays.
        let middle = remove_top_level_key(text, "tab_size");
        assert!(middle.contains("// two") && Settings::parse(&middle).unwrap().editor.tab_size.is_none(), "{middle}");
        assert_eq!(remove_top_level_key("{\"a\": 1}", "a"), "{}");
        assert_eq!(remove_top_level_key(text, "missing"), text);
        let nested = "{\"languages\": {\"Go\": {\"theme\": 1}}}";
        assert_eq!(remove_top_level_key(nested, "theme"), nested);
        // Only one of several entries on a line.
        assert_eq!(remove_top_level_key("{ \"a\": 1, \"b\": 2 }", "a"), "{ \"b\": 2 }");
    }

    #[test]
    fn formats_values_readably() {
        use serde_json::json;
        assert_eq!(format_value(&json!(["fish", "--login"]), 2), r#"["fish", "--login"]"#);
        assert_eq!(format_value(&json!({"tab_size": 2}), 4), r#"{ "tab_size": 2 }"#);
        let languages = json!({"Go": {"tab_size": 8}, "Rust": {"soft_wrap": true}});
        assert_eq!(
            format_value(&languages, 2),
            "{\n    \"Go\": { \"tab_size\": 8 },\n    \"Rust\": { \"soft_wrap\": true }\n  }"
        );
        let text = set_top_level_key("{\n}\n", "languages", &format_value(&languages, 2));
        assert_eq!(Settings::parse(&text).unwrap().languages["Go"].tab_size, Some(8));
    }

    #[test]
    fn resolves_file_settings_in_order() {
        let mut s = Settings::parse(r#"{"tab_size": 3, "languages": {"YAML": {"tab_size": 5}}}"#).unwrap();
        assert_eq!(s.for_file(Language::Rust, None).tab_size, 3);
        // A language override beats the top level; built-in defaults lose to both.
        assert_eq!(s.for_file(Language::Yaml, None).tab_size, 5);
        assert_eq!(s.for_file(Language::Json, None).tab_size, 3);
        assert!(s.for_file(Language::Go, None).hard_tabs);
        assert!(s.for_file(Language::Markdown, None).soft_wrap);
        s.editor.soft_wrap = Some(false);
        assert!(!s.for_file(Language::Markdown, None).soft_wrap);
        assert_eq!(Settings::default().for_file(Language::Json, None).tab_size, 2);
    }
}
