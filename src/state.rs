//! UI state remembered between launches (panel layout, last project, each
//! project's open tabs). Kept separate from `settings.json`, which holds user
//! preferences.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{document::Breakpoint, theme::config_dir};

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct SessionState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_root: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidebar_width: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problems_height: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_sidebar: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_problems: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_terminal: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidebar_mode: Option<String>,
    /// Editor font zoom in points, added to the configured size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_zoom: Option<f32>,
    /// Diffs show side by side (`false`: unified).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_side_by_side: Option<bool>,
    /// The bottom panel shows the Git log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_git_log: Option<bool>,
    /// Recently opened project folders, newest first (the welcome screen's list).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_projects: Vec<PathBuf>,
    /// Welcome Setup was finished (`false`: shown but not finished yet).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onboarded: Option<bool>,
    /// Where the last moved or resized window was, for the next launch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowPlacement>,
}

/// A window's place on screen, in points.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct WindowPlacement {
    /// The display it was on (its UUID), so it reopens there while connected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// Top-left corner, relative to that display.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub fullscreen: bool,
}

impl SessionState {
    fn path() -> Option<PathBuf> {
        config_dir().map(|d| d.join("state.json"))
    }

    /// Whether Welcome Setup should show: on a fresh install, until it's
    /// finished. An install that already has settings or state counts as set up.
    pub fn needs_onboarding() -> bool {
        let mut state = Self::load();
        if let Some(done) = state.onboarded {
            return !done;
        }
        let exists = |path: Option<PathBuf>| path.is_some_and(|p| p.exists());
        let fresh = !exists(Self::path()) && !exists(crate::settings::Settings::path());
        state.onboarded = Some(!fresh);
        if let Err(err) = state.save() {
            eprintln!("couldn't save window state: {err}");
        }
        fresh
    }

    pub fn set_onboarded() {
        let mut state = Self::load();
        state.onboarded = Some(true);
        if let Err(err) = state.save() {
            eprintln!("couldn't save window state: {err}");
        }
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path().ok_or_else(|| std::io::Error::other("no config directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)? + "\n")
    }
}

/// A project's open tabs, restored when it's opened again. Unsaved edits are
/// kept too ("hot exit"), so closing a project window never loses work.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ProjectSession {
    pub root: PathBuf,
    pub tabs: Vec<TabSession>,
    /// Index into `tabs` of the focused tab.
    pub active: usize,
    /// Split panes; absent for a single pane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<PaneLayout>,
    /// Expanded folders in the file tree, relative to `root`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expanded: Vec<PathBuf>,
    /// Line breakpoints per file, open or not.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub breakpoints: Vec<FileBreakpoints>,
}

/// A file's line breakpoints.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct FileBreakpoints {
    /// Relative to the project.
    pub path: PathBuf,
    pub breakpoints: Vec<Breakpoint>,
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct PaneLayout {
    /// Stacked rather than side by side.
    pub column: bool,
    /// Each pane's share of the space.
    pub sizes: Vec<f32>,
    /// Each pane's active tab, counted within the pane.
    pub active: Vec<usize>,
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct TabSession {
    /// None for an untitled buffer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The caret, as a byte offset.
    pub cursor: usize,
    /// The whole text, when the buffer had unsaved edits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsaved: Option<String>,
    /// Which pane the tab is in.
    #[serde(skip_serializing_if = "is_zero")]
    pub pane: usize,
    #[serde(skip_serializing_if = "is_false")]
    pub pinned: bool,
    /// Folded regions as (header row, last hidden row).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub folds: Vec<[usize; 2]>,
    /// A file's encoding when it isn't UTF-8 (a chosen one isn't guessed again).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl ProjectSession {
    /// `sessions/<folder name>-<hash of the full path>.json`: readable, and
    /// unique for same-named folders.
    fn file_name(root: &Path) -> String {
        let name: String = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .take(40)
            .collect();
        format!("{name}-{:016x}.json", fnv1a(root.to_string_lossy().as_bytes()))
    }

    fn path(root: &Path) -> Option<PathBuf> {
        config_dir().map(|d| d.join("sessions").join(Self::file_name(root)))
    }

    pub fn load(root: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path(root)?).ok()?;
        serde_json::from_str::<Self>(&text).ok().filter(|s| s.root == root)
    }

    /// Written to a temporary file first, so a crash mid-write can't leave
    /// a truncated session (and lose the unsaved text in it).
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path(&self.root).ok_or_else(|| std::io::Error::other("no config directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")?;
        std::fs::rename(tmp, path)
    }
}

/// FNV-1a: a hash that, unlike std's, is stable across Rust versions.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_file_names_are_stable_and_distinct() {
        let a = ProjectSession::file_name(Path::new("/work/kin"));
        assert_eq!(a, ProjectSession::file_name(Path::new("/work/kin")));
        assert!(a.starts_with("kin-") && a.ends_with(".json"), "{a}");
        assert_ne!(a, ProjectSession::file_name(Path::new("/other/kin")));
        assert_eq!(fnv1a(b""), 0xcbf29ce484222325);
    }

    #[test]
    fn sessions_round_trip() {
        let session = ProjectSession {
            root: "/work/kin".into(),
            tabs: vec![
                TabSession { path: Some("/work/kin/a.rs".into()), cursor: 12, unsaved: None, pane: 0, pinned: true, folds: vec![[3, 9]], encoding: Some("windows-1254".into()) },
                TabSession { path: None, cursor: 0, unsaved: Some("draft".into()), pane: 1, pinned: false, folds: Vec::new(), encoding: None },
            ],
            active: 1,
            layout: Some(PaneLayout { column: false, sizes: vec![0.4, 0.6], active: vec![0, 0] }),
            expanded: vec!["src".into(), "src/lsp".into()],
            breakpoints: vec![FileBreakpoints {
                path: "src/a.rs".into(),
                breakpoints: vec![Breakpoint::at(3), Breakpoint { condition: Some("i > 2".into()), ..Breakpoint::at(9) }],
            }],
        };
        let json = serde_json::to_string(&session).unwrap();
        assert_eq!(serde_json::from_str::<ProjectSession>(&json).unwrap(), session);
    }
}
