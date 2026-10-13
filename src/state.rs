//! UI state remembered between launches (panel layout, last project, each
//! project's open tabs). Kept separate from `settings.json`, which holds user
//! preferences.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::theme::config_dir;

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
    pub sidebar_mode: Option<String>,
    /// Recently opened project folders, newest first (the welcome screen's list).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_projects: Vec<PathBuf>,
}

impl SessionState {
    fn path() -> Option<PathBuf> {
        config_dir().map(|d| d.join("state.json"))
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
    pub active: usize,
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
                TabSession { path: Some("/work/kin/a.rs".into()), cursor: 12, unsaved: None },
                TabSession { path: None, cursor: 0, unsaved: Some("draft".into()) },
            ],
            active: 1,
        };
        let json = serde_json::to_string(&session).unwrap();
        assert_eq!(serde_json::from_str::<ProjectSession>(&json).unwrap(), session);
    }
}
