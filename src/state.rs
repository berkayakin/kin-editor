//! UI state remembered between launches (panel layout, last project).
//! Kept separate from `settings.json`, which holds user preferences.

use std::path::PathBuf;

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
