//! Project trust (Restricted Mode). A cloned repository can make an editor
//! run its code without being asked: a language server builds it, ESLint and
//! Prettier load its JavaScript config, `node_modules/.bin` and venv tools are
//! its own programs, a venv's activate script is a shell script, and Git runs
//! `core.fsmonitor` from its config. Until the folder is trusted, Kin does
//! none of that; editing, Git and anything run explicitly (Run, Debug) work.
//!
//! Trusting a folder trusts everything inside it, so trusting a parent folder
//! covers every project in it. Trusted folders live in `state.json`; the
//! `trust_all_projects` setting turns the checks off without saving anything.

use std::{
    path::{Path, PathBuf},
    sync::{
        RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::state::SessionState;

/// Trusted folders, loaded from `state.json` on first use.
static TRUSTED: RwLock<Option<Vec<PathBuf>>> = RwLock::new(None);
static TRUST_ALL: AtomicBool = AtomicBool::new(false);

/// Bumped when trust changes, so windows start or stop what it allows.
pub struct TrustChanged(pub u64);
impl gpui::Global for TrustChanged {}

pub fn set_trust_all(on: bool) {
    TRUST_ALL.store(on, Ordering::Relaxed);
}

/// Whether Kin may run what `path` (a project folder or a file) configures.
pub fn is_trusted(path: &Path) -> bool {
    TRUST_ALL.load(Ordering::Relaxed)
        || trusted_by(path).is_some()
        // Kin's own settings, keymap and themes.
        || crate::theme::config_dir().is_some_and(|dir| path.starts_with(dir))
}

/// The trusted folder that covers `path`.
pub fn trusted_by(path: &Path) -> Option<PathBuf> {
    with_trusted(|folders| folders.iter().filter(|f| path.starts_with(f)).min_by_key(|f| f.as_os_str().len()).cloned())
}

fn with_trusted<T>(f: impl FnOnce(&Vec<PathBuf>) -> T) -> T {
    {
        let trusted = TRUSTED.read().unwrap_or_else(|e| e.into_inner());
        if let Some(folders) = trusted.as_ref() {
            return f(folders);
        }
    }
    let mut trusted = TRUSTED.write().unwrap_or_else(|e| e.into_inner());
    f(trusted.get_or_insert_with(|| SessionState::load().trusted_folders))
}

/// Trusts `folder` and everything in it, and remembers it.
pub fn trust(folder: &Path, cx: &mut gpui::App) {
    change(cx, |folders| {
        if !folders.iter().any(|f| folder.starts_with(f)) {
            // Folders inside it are covered now.
            folders.retain(|f| !f.starts_with(folder));
            folders.push(folder.to_path_buf());
        }
    });
}

/// Stops trusting `folder` (an entry, as `trusted_by` gives it).
pub fn untrust(folder: &Path, cx: &mut gpui::App) {
    change(cx, |folders| folders.retain(|f| f != folder));
}

fn change(cx: &mut gpui::App, edit: impl FnOnce(&mut Vec<PathBuf>)) {
    let mut state = SessionState::load();
    {
        let mut trusted = TRUSTED.write().unwrap_or_else(|e| e.into_inner());
        // The file has the latest list (another window may have changed it).
        let folders = trusted.insert(state.trusted_folders.clone());
        edit(folders);
        folders.sort();
        state.trusted_folders = folders.clone();
    }
    if let Err(err) = state.save() {
        eprintln!("couldn't save trusted folders: {err}");
    }
    changed(cx);
}

/// Tells windows that trust changed.
pub fn changed(cx: &mut gpui::App) {
    let next = cx.try_global::<TrustChanged>().map_or(1, |t| t.0 + 1);
    cx.set_global(TrustChanged(next));
}

/// Every trusted folder.
pub fn folders() -> Vec<PathBuf> {
    with_trusted(|folders| folders.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_cover_their_contents() {
        *TRUSTED.write().unwrap() = Some(vec![PathBuf::from("/work/clients")]);
        assert!(is_trusted(Path::new("/work/clients/acme")));
        assert!(is_trusted(Path::new("/work/clients/acme/src/main.rs")));
        assert!(!is_trusted(Path::new("/work/clients-old")));
        assert!(!is_trusted(Path::new("/work")));
        assert_eq!(trusted_by(Path::new("/work/clients/acme")), Some(PathBuf::from("/work/clients")));
    }
}
