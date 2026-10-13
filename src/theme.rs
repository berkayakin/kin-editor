//! Themes are JSON files. Built-in themes live in `assets/themes/` and are
//! compiled in; user themes are loaded from the config directory
//! (`~/.config/kin/themes`, or `%APPDATA%\kin\themes` on Windows).
//!
//! A theme only has to specify what it changes: anything missing is taken
//! from the theme named in `"extends"`, or from the default theme of the
//! same appearance ("Clay Black" / "Clay Light").

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::{App, Global, Hsla, Rgba, SharedString, rgba};
use serde::Deserialize;

use crate::{settings::Settings, state::SessionState, syntax::Highlight};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Appearance {
    Dark,
    Light,
}

#[derive(Clone)]
pub struct SyntaxColors {
    pub plain: Rgba,
    pub keyword: Rgba,
    pub string: Rgba,
    pub number: Rgba,
    pub constant: Rgba,
    pub function: Rgba,
    pub method: Rgba,
    pub property: Rgba,
    pub type_: Rgba,
    pub variable_builtin: Rgba,
    pub comment: Rgba,
    pub operator: Rgba,
    pub punctuation: Rgba,
    pub tag: Rgba,
    pub attribute: Rgba,
    pub escape: Rgba,
}

#[derive(Clone)]
pub struct Theme {
    pub name: SharedString,
    pub appearance: Appearance,
    pub italic_comments: bool,

    pub ui_font: SharedString,
    pub mono_font: SharedString,
    pub ui_font_size: f32,
    pub editor_font_size: f32,
    pub editor_line_height: f32,

    pub background: Rgba,
    pub panel: Rgba,
    pub elevated: Rgba,
    pub border: Rgba,
    pub border_subtle: Rgba,
    pub hover: Rgba,
    pub active: Rgba,

    pub text: Rgba,
    pub text_muted: Rgba,
    pub text_dim: Rgba,
    pub accent: Rgba,
    pub accent_soft: Rgba,

    pub cursor: Rgba,
    pub selection: Rgba,
    pub selection_inactive: Rgba,
    pub current_line: Rgba,
    pub line_number: Rgba,
    pub line_number_active: Rgba,
    pub indent_guide: Rgba,
    pub search_match: Rgba,
    pub search_active: Rgba,
    pub bracket_match: Rgba,

    pub error: Rgba,
    pub warning: Rgba,
    pub info: Rgba,
    pub success: Rgba,

    pub git_added: Rgba,
    pub git_modified: Rgba,
    pub git_deleted: Rgba,
    pub git_untracked: Rgba,
    pub git_conflict: Rgba,

    /// The terminal's 16 ANSI colors.
    pub terminal_black: Rgba,
    pub terminal_red: Rgba,
    pub terminal_green: Rgba,
    pub terminal_yellow: Rgba,
    pub terminal_blue: Rgba,
    pub terminal_magenta: Rgba,
    pub terminal_cyan: Rgba,
    pub terminal_white: Rgba,
    pub terminal_bright_black: Rgba,
    pub terminal_bright_red: Rgba,
    pub terminal_bright_green: Rgba,
    pub terminal_bright_yellow: Rgba,
    pub terminal_bright_blue: Rgba,
    pub terminal_bright_magenta: Rgba,
    pub terminal_bright_cyan: Rgba,
    pub terminal_bright_white: Rgba,

    pub syntax: SyntaxColors,
}

impl Global for Theme {}

/// Maps JSON keys to theme fields, so the key list lives in one place.
macro_rules! color_keys {
    ($($key:literal => $($field:ident).+),* $(,)?) => {
        pub const COLOR_KEYS: &[&str] = &[$($key),*];

        fn color_mut<'a>(theme: &'a mut Theme, key: &str) -> Option<&'a mut Rgba> {
            match key {
                $($key => Some(&mut theme.$($field).+),)*
                _ => None,
            }
        }
    };
}

color_keys! {
    "background" => background,
    "panel" => panel,
    "elevated" => elevated,
    "border" => border,
    "border_subtle" => border_subtle,
    "hover" => hover,
    "active" => active,
    "text" => text,
    "text_muted" => text_muted,
    "text_dim" => text_dim,
    "accent" => accent,
    "accent_soft" => accent_soft,
    "cursor" => cursor,
    "selection" => selection,
    "selection_inactive" => selection_inactive,
    "current_line" => current_line,
    "line_number" => line_number,
    "line_number_active" => line_number_active,
    "indent_guide" => indent_guide,
    "search_match" => search_match,
    "search_active" => search_active,
    "bracket_match" => bracket_match,
    "error" => error,
    "warning" => warning,
    "info" => info,
    "success" => success,
    "git_added" => git_added,
    "git_modified" => git_modified,
    "git_deleted" => git_deleted,
    "git_untracked" => git_untracked,
    "git_conflict" => git_conflict,
    "terminal_black" => terminal_black,
    "terminal_red" => terminal_red,
    "terminal_green" => terminal_green,
    "terminal_yellow" => terminal_yellow,
    "terminal_blue" => terminal_blue,
    "terminal_magenta" => terminal_magenta,
    "terminal_cyan" => terminal_cyan,
    "terminal_white" => terminal_white,
    "terminal_bright_black" => terminal_bright_black,
    "terminal_bright_red" => terminal_bright_red,
    "terminal_bright_green" => terminal_bright_green,
    "terminal_bright_yellow" => terminal_bright_yellow,
    "terminal_bright_blue" => terminal_bright_blue,
    "terminal_bright_magenta" => terminal_bright_magenta,
    "terminal_bright_cyan" => terminal_bright_cyan,
    "terminal_bright_white" => terminal_bright_white,
    "syntax.plain" => syntax.plain,
    "syntax.keyword" => syntax.keyword,
    "syntax.string" => syntax.string,
    "syntax.number" => syntax.number,
    "syntax.constant" => syntax.constant,
    "syntax.function" => syntax.function,
    "syntax.method" => syntax.method,
    "syntax.property" => syntax.property,
    "syntax.type" => syntax.type_,
    "syntax.variable_builtin" => syntax.variable_builtin,
    "syntax.comment" => syntax.comment,
    "syntax.operator" => syntax.operator,
    "syntax.punctuation" => syntax.punctuation,
    "syntax.tag" => syntax.tag,
    "syntax.attribute" => syntax.attribute,
    "syntax.escape" => syntax.escape,
}

impl Theme {
    pub fn get(cx: &App) -> &Theme {
        cx.global::<Theme>()
    }

    #[cfg(test)]
    pub fn blank_for_tests() -> Self {
        Self::blank(Appearance::Dark)
    }

    /// Neutral starting point before a theme file is applied.
    fn blank(appearance: Appearance) -> Self {
        let (bg, fg) = match appearance {
            Appearance::Dark => (rgba(0x1e1e1eff), rgba(0xe0e0e0ff)),
            Appearance::Light => (rgba(0xffffffff), rgba(0x202020ff)),
        };
        let syntax = SyntaxColors {
            plain: fg,
            keyword: fg,
            string: fg,
            number: fg,
            constant: fg,
            function: fg,
            method: fg,
            property: fg,
            type_: fg,
            variable_builtin: fg,
            comment: fg,
            operator: fg,
            punctuation: fg,
            tag: fg,
            attribute: fg,
            escape: fg,
        };
        Self {
            name: "".into(),
            appearance,
            italic_comments: true,
            ui_font: ".SystemUIFont".into(),
            mono_font: "Menlo".into(),
            ui_font_size: 13.0,
            editor_font_size: 14.0,
            editor_line_height: 22.0,
            background: bg,
            panel: bg,
            elevated: bg,
            border: fg,
            border_subtle: fg,
            hover: rgba(0x80808020),
            active: rgba(0x80808040),
            text: fg,
            text_muted: fg,
            text_dim: fg,
            accent: fg,
            accent_soft: rgba(0x80808040),
            cursor: fg,
            selection: rgba(0x80808060),
            selection_inactive: rgba(0x80808040),
            current_line: rgba(0x80808010),
            line_number: fg,
            line_number_active: fg,
            indent_guide: rgba(0x80808020),
            search_match: rgba(0xffcc0040),
            search_active: rgba(0xffcc0080),
            bracket_match: rgba(0x80808040),
            error: rgba(0xff5555ff),
            warning: rgba(0xffaa00ff),
            info: rgba(0x55aaffff),
            success: rgba(0x55cc55ff),
            git_added: rgba(0x55cc55ff),
            git_modified: rgba(0x55aaffff),
            git_deleted: rgba(0xff5555ff),
            git_untracked: rgba(0xcc8855ff),
            git_conflict: rgba(0xffaa00ff),
            terminal_black: rgba(0x2b2b2bff),
            terminal_red: rgba(0xe06c75ff),
            terminal_green: rgba(0x98c379ff),
            terminal_yellow: rgba(0xe5c07bff),
            terminal_blue: rgba(0x61afefff),
            terminal_magenta: rgba(0xc678ddff),
            terminal_cyan: rgba(0x56b6c2ff),
            terminal_white: rgba(0xc8c8c8ff),
            terminal_bright_black: rgba(0x6b6b6bff),
            terminal_bright_red: rgba(0xef8a92ff),
            terminal_bright_green: rgba(0xb0d48fff),
            terminal_bright_yellow: rgba(0xefcf94ff),
            terminal_bright_blue: rgba(0x85c1f5ff),
            terminal_bright_magenta: rgba(0xd595e6ff),
            terminal_bright_cyan: rgba(0x7ccbd4ff),
            terminal_bright_white: rgba(0xffffffff),
            syntax,
        }
    }

    pub fn highlight_color(&self, h: Highlight) -> Hsla {
        let s = &self.syntax;
        match h {
            Highlight::None | Highlight::Variable => s.plain,
            Highlight::Keyword => s.keyword,
            Highlight::String => s.string,
            Highlight::Number => s.number,
            Highlight::Constant => s.constant,
            Highlight::Function => s.function,
            Highlight::Method => s.method,
            Highlight::Property => s.property,
            Highlight::Type => s.type_,
            Highlight::VariableBuiltin => s.variable_builtin,
            Highlight::Comment => s.comment,
            Highlight::Operator => s.operator,
            Highlight::Punctuation => s.punctuation,
            Highlight::Tag => s.tag,
            Highlight::Attribute => s.attribute,
            Highlight::Escape => s.escape,
        }
        .into()
    }

    fn with_fonts(mut self, from: &Theme) -> Self {
        self.ui_font = from.ui_font.clone();
        self.mono_font = from.mono_font.clone();
        self.ui_font_size = from.ui_font_size;
        self.editor_font_size = from.editor_font_size;
        self.editor_line_height = from.editor_line_height;
        self
    }
}

/// Parses `#rgb`, `#rrggbb` or `#rrggbbaa`.
pub fn parse_color(s: &str) -> Option<Rgba> {
    let hex = s.trim().strip_prefix('#')?;
    let expanded: String = match hex.len() {
        3 => hex.chars().flat_map(|c| [c, c]).chain("ff".chars()).collect(),
        6 => format!("{hex}ff"),
        8 => hex.to_string(),
        _ => return None,
    };
    u32::from_str_radix(&expanded, 16).ok().map(rgba)
}

// ---- Theme files -------------------------------------------------------------

#[derive(Deserialize, Clone, Debug)]
pub struct ThemeFile {
    pub name: String,
    #[serde(default)]
    pub appearance: Option<String>,
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub italic_comments: Option<bool>,
    #[serde(default)]
    pub colors: HashMap<String, String>,
    #[serde(default)]
    pub syntax: HashMap<String, String>,
}

impl ThemeFile {
    fn appearance(&self) -> Option<Appearance> {
        match self.appearance.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some("light") => Some(Appearance::Light),
            Some("dark") => Some(Appearance::Dark),
            _ => None,
        }
    }
}

static BUILTIN: &[&str] = &[
    include_str!("../assets/themes/clay-black.json"),
    include_str!("../assets/themes/clay-dark.json"),
    include_str!("../assets/themes/clay-light.json"),
    include_str!("../assets/themes/github-light.json"),
    include_str!("../assets/themes/one-dark.json"),
    include_str!("../assets/themes/dracula.json"),
    include_str!("../assets/themes/nord.json"),
    include_str!("../assets/themes/tokyo-night.json"),
    include_str!("../assets/themes/gruvbox-dark.json"),
    include_str!("../assets/themes/catppuccin-mocha.json"),
    include_str!("../assets/themes/rose-pine.json"),
];

pub const DEFAULT_DARK: &str = "Clay Black";
pub const DEFAULT_LIGHT: &str = "Clay Light";

pub struct ThemeEntry {
    pub theme: Arc<Theme>,
    /// `Some` for themes loaded from the user's themes folder.
    pub path: Option<PathBuf>,
}

/// All available themes, resolved and ready to apply.
pub struct ThemeRegistry {
    pub entries: Vec<ThemeEntry>,
    /// Problems found while loading user themes.
    pub errors: Vec<String>,
}

impl Global for ThemeRegistry {}

impl ThemeRegistry {
    pub fn load() -> Self {
        let mut files: Vec<(ThemeFile, Option<PathBuf>)> = Vec::new();
        let mut errors = Vec::new();
        for source in BUILTIN {
            let file: ThemeFile = serde_json::from_str(source).expect("invalid built-in theme");
            files.push((file, None));
        }
        if let Some(dir) = themes_dir()
            && let Ok(read) = std::fs::read_dir(&dir)
        {
            let mut paths: Vec<PathBuf> = read
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect();
            paths.sort();
            for path in paths {
                match std::fs::read_to_string(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|s| serde_json::from_str::<ThemeFile>(&s).map_err(|e| e.to_string()))
                {
                    Ok(file) => {
                        // A user theme with a built-in's name replaces it.
                        files.retain(|(f, _)| f.name != file.name);
                        files.push((file, Some(path)));
                    }
                    Err(e) => errors.push(format!("{}: {e}", file_label(&path))),
                }
            }
        }

        let by_name: HashMap<String, ThemeFile> =
            files.iter().map(|(f, _)| (f.name.clone(), f.clone())).collect();
        let mut entries = Vec::new();
        for (file, path) in &files {
            if let Some(theme) = resolve(file, &by_name, &mut Vec::new(), &mut errors) {
                entries.push(ThemeEntry {
                    theme: Arc::new(theme),
                    path: path.clone(),
                });
            }
        }
        errors.dedup();
        Self { entries, errors }
    }

    pub fn find(&self, name: &str) -> Option<&ThemeEntry> {
        self.entries.iter().find(|e| e.theme.name.as_ref() == name)
    }
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn resolve(
    file: &ThemeFile,
    all: &HashMap<String, ThemeFile>,
    stack: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> Option<Theme> {
    if stack.contains(&file.name) || stack.len() > 8 {
        errors.push(format!("{}: circular \"extends\"", file.name));
        return None;
    }
    stack.push(file.name.clone());

    let parent_name = match &file.extends {
        Some(parent) => Some(parent.clone()),
        None => {
            let default = match file.appearance().unwrap_or(Appearance::Dark) {
                Appearance::Dark => DEFAULT_DARK,
                Appearance::Light => DEFAULT_LIGHT,
            };
            (file.name != default).then(|| default.to_string())
        }
    };
    let mut theme = match parent_name.as_deref().and_then(|p| all.get(p)) {
        Some(parent) => resolve(parent, all, stack, errors)?,
        None => {
            if let Some(p) = &file.extends {
                errors.push(format!("{}: extends unknown theme \"{p}\"", file.name));
            }
            Theme::blank(file.appearance().unwrap_or(Appearance::Dark))
        }
    };
    stack.pop();

    theme.name = file.name.clone().into();
    if let Some(a) = file.appearance() {
        theme.appearance = a;
    }
    if let Some(i) = file.italic_comments {
        theme.italic_comments = i;
    }
    let entries = file
        .colors
        .iter()
        .map(|(k, v)| (k.clone(), v))
        .chain(file.syntax.iter().map(|(k, v)| (format!("syntax.{k}"), v)));
    for (key, value) in entries {
        match (color_mut(&mut theme, &key), parse_color(value)) {
            (Some(slot), Some(color)) => *slot = color,
            (None, _) => {
                let hint = suggest_key(&key).map(|k| format!(" (did you mean \"{k}\"?)")).unwrap_or_default();
                errors.push(format!("{}: unknown color \"{key}\"{hint}", file.name));
            }
            (_, None) => errors.push(format!("{}: invalid color \"{value}\" for \"{key}\"", file.name)),
        }
    }
    Some(theme)
}

/// The known key closest to a misspelled one, if any is close enough.
fn suggest_key(key: &str) -> Option<&'static str> {
    closest(key, COLOR_KEYS.iter().copied())
}

/// The candidate closest to a misspelled word (edit distance ≤ 3), for
/// "did you mean" hints.
pub fn closest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    fn distance(a: &str, b: &str) -> usize {
        let b: Vec<char> = b.chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.chars().enumerate() {
            let mut row = vec![i + 1];
            for (j, cb) in b.iter().enumerate() {
                row.push((prev[j] + (ca != *cb) as usize).min(prev[j + 1] + 1).min(row[j] + 1));
            }
            prev = row;
        }
        prev[b.len()]
    }
    candidates
        .into_iter()
        .map(|k| (distance(word, k), k))
        .filter(|(d, _)| *d <= 3)
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
}

/// A path for display, with the home directory shortened to `~`.
pub fn display_path(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if path.starts_with(&home) => {
            format!("~/{}", path.strip_prefix(&home).unwrap().display())
        }
        _ => path.display().to_string(),
    }
}

// ---- Settings ------------------------------------------------------------------

/// Where user configuration lives: `~/.config/kin` (or `$XDG_CONFIG_HOME/kin`)
/// on macOS and Linux, `%APPDATA%\kin` on Windows.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("kin"));
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("kin"))
}

pub fn themes_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("themes"))
}

/// Makes `name` the active theme, keeping the current fonts.
pub fn apply_theme(name: &str, cx: &mut App) -> bool {
    let Some(theme) = cx
        .try_global::<ThemeRegistry>()
        .and_then(|r| r.find(name))
        .map(|e| (*e.theme).clone())
    else {
        return false;
    };
    let theme = match cx.try_global::<Theme>() {
        Some(current) => theme.with_fonts(current),
        None => theme,
    };
    cx.set_global(theme);
    cx.refresh_windows();
    true
}

/// Rescans theme files and re-applies the active theme so edits show up.
pub fn reload_themes(cx: &mut App) {
    let current = Theme::get(cx).name.to_string();
    cx.set_global(ThemeRegistry::load());
    if !apply_theme(&current, cx) {
        apply_theme(DEFAULT_DARK, cx);
    }
}

/// Writes a starter theme into the user's themes folder (if it has none)
/// and returns the folder.
pub fn ensure_themes_dir() -> std::io::Result<PathBuf> {
    let dir = themes_dir().ok_or_else(|| std::io::Error::other("no config directory"))?;
    std::fs::create_dir_all(&dir)?;
    let has_theme = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().is_some_and(|x| x == "json"));
    if !has_theme {
        std::fs::write(dir.join("my-theme.json"), EXAMPLE_THEME)?;
    }
    Ok(dir)
}

const EXAMPLE_THEME: &str = r##"{
  "name": "My Theme",
  "appearance": "dark",
  "extends": "Clay Dark",
  "colors": {
    "accent": "#7fb4ca",
    "accent_soft": "#7fb4ca2b",
    "cursor": "#7fb4ca",
    "selection": "#7fb4ca40"
  },
  "syntax": {
    "keyword": "#c4a7e7",
    "function": "#9ccfd8"
  }
}
"##;

/// Picks the first installed font from a preference list. Keeps the editor
/// looking native on each platform without bundling fonts.
pub fn pick_fonts(cx: &App) -> (SharedString, SharedString) {
    let installed = cx.text_system().all_font_names();
    let has = |name: &str| installed.iter().any(|f| f == name);

    let mono_candidates: &[&str] = if cfg!(target_os = "macos") {
        &["Lilex", "JetBrains Mono", "SF Mono", "Menlo"]
    } else if cfg!(target_os = "windows") {
        &["Lilex", "JetBrains Mono", "Cascadia Code", "Cascadia Mono", "Consolas"]
    } else {
        &["Lilex", "JetBrains Mono", "DejaVu Sans Mono", "monospace"]
    };
    let mono = mono_candidates
        .iter()
        .copied()
        .find(|f| has(f))
        .unwrap_or(mono_candidates[mono_candidates.len() - 1]);

    let ui = if cfg!(target_os = "windows") {
        ["Segoe UI Variable Text", "Segoe UI"]
            .into_iter()
            .find(|f| has(f))
            .unwrap_or("Segoe UI")
    } else {
        ".SystemUIFont"
    };

    (ui.into(), mono.into())
}

/// Installs the theme registry and the saved (or default) theme.
pub fn init(cx: &mut App) {
    let registry = ThemeRegistry::load();
    let wanted = Settings::get(cx).theme.clone().unwrap_or_else(|| DEFAULT_DARK.to_string());
    let entry = registry
        .find(&wanted)
        .or_else(|| registry.find(DEFAULT_DARK))
        .expect("default theme is built in");
    let theme = (*entry.theme).clone();
    cx.set_global(registry);
    cx.set_global(theme);
    apply_fonts(cx);
}

/// Default editor size; zoom and `editor_font_size` move away from it.
pub const DEFAULT_EDITOR_FONT_SIZE: f32 = 14.0;
/// 22px lines at 14px, the spacing Kin has always used.
const DEFAULT_LINE_HEIGHT: f32 = 22.0 / 14.0;

/// Sets the fonts from the settings and the remembered zoom.
pub fn apply_fonts(cx: &mut App) {
    let (ui_font, mono_font) = pick_fonts(cx);
    let settings = Settings::get(cx).clone();
    let mono_font = match settings.editor_font_family.as_deref() {
        Some(family) if cx.text_system().all_font_names().iter().any(|f| f == family) => family.to_string().into(),
        _ => mono_font,
    };
    let zoom = SessionState::load().font_zoom.unwrap_or(0.);
    let size = (settings.editor_font_size.unwrap_or(DEFAULT_EDITOR_FONT_SIZE) + zoom).clamp(6., 72.);
    let line_height = settings.editor_line_height.unwrap_or(DEFAULT_LINE_HEIGHT).clamp(1., 3.);
    let mut theme = Theme::get(cx).clone();
    theme.ui_font = ui_font;
    theme.mono_font = mono_font;
    theme.editor_font_size = size;
    theme.editor_line_height = (size * line_height).round();
    cx.set_global(theme);
    cx.refresh_windows();
}

/// Changes the editor zoom by `delta` points (or resets it with `None`)
/// and remembers it for the next launch.
pub fn zoom(delta: Option<f32>, cx: &mut App) {
    let mut state = SessionState::load();
    let base = Settings::get(cx).editor_font_size.unwrap_or(DEFAULT_EDITOR_FONT_SIZE);
    state.font_zoom = delta
        .map(|d| (state.font_zoom.unwrap_or(0.) + d).clamp(6. - base, 72. - base))
        .filter(|z| *z != 0.);
    if let Err(err) = state.save() {
        eprintln!("couldn't save the zoom: {err}");
    }
    apply_fonts(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtins() -> HashMap<String, ThemeFile> {
        BUILTIN
            .iter()
            .map(|s| serde_json::from_str::<ThemeFile>(s).unwrap())
            .map(|f| (f.name.clone(), f))
            .collect()
    }

    #[test]
    fn builtins_are_complete() {
        let all = builtins();
        assert_eq!(all.len(), 11);
        for file in all.values() {
            let mut errors = Vec::new();
            resolve(file, &all, &mut Vec::new(), &mut errors).unwrap();
            assert!(errors.is_empty(), "{errors:?}");
            let defined = file.colors.len() + file.syntax.len();
            assert_eq!(defined, COLOR_KEYS.len(), "{} is missing keys", file.name);
        }
        let light = all.values().filter(|f| f.appearance() == Some(Appearance::Light)).count();
        assert_eq!(light, 2);
    }

    #[test]
    fn partial_themes_inherit() {
        let mut all = builtins();
        let custom: ThemeFile = serde_json::from_str(EXAMPLE_THEME).unwrap();
        all.insert(custom.name.clone(), custom.clone());
        let mut errors = Vec::new();
        let theme = resolve(&custom, &all, &mut Vec::new(), &mut errors).unwrap();
        let base = resolve(&all["Clay Dark"], &all, &mut Vec::new(), &mut errors).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(theme.accent, parse_color("#7fb4ca").unwrap());
        assert_eq!(theme.background, base.background);
    }

    #[test]
    fn reports_bad_keys_and_cycles() {
        let mut all = builtins();
        let a: ThemeFile = serde_json::from_str(r##"{"name":"A","extends":"B","colors":{"nope":"#fff","text":"blue"}}"##).unwrap();
        let b: ThemeFile = serde_json::from_str(r#"{"name":"B","extends":"A"}"#).unwrap();
        all.insert("A".into(), a.clone());
        all.insert("B".into(), b);
        let mut errors = Vec::new();
        assert!(resolve(&a, &all, &mut Vec::new(), &mut errors).is_none());
        assert!(errors.iter().any(|e| e.contains("circular")));
        assert_eq!(suggest_key("backgrond"), Some("background"));
        assert_eq!(suggest_key("syntax.keywrd"), Some("syntax.keyword"));
    }

    #[test]
    fn colors_parse() {
        assert_eq!(parse_color("#fff"), Some(rgba(0xffffffff)));
        assert_eq!(parse_color("#d97757"), Some(rgba(0xd97757ff)));
        assert_eq!(parse_color("#d977572b"), Some(rgba(0xd977572b)));
        assert_eq!(parse_color("red"), None);
    }
}
