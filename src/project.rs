//! The project's files, scanned once in the background and then kept up to
//! date from the watcher's events (`apply`), instead of walking the project
//! again for each use. Go to File / Search Everywhere list `openable` (git-
//! ignored files too, but nothing inside ignored folders), Find in Files
//! searches `searchable` (not ignored).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use ignore::gitignore::Gitignore;

#[derive(Clone, Default)]
pub struct ProjectSnapshot {
    root: PathBuf,
    /// Every file in a folder that isn't ignored, and whether it's ignored
    /// itself (a `.env` in the project folder is).
    files: BTreeMap<PathBuf, bool>,
    /// The project's .gitignore (and .git/info/exclude), for changes.
    ignore: Option<Gitignore>,
    openable: Arc<Vec<PathBuf>>,
}

/// What a batch of changes did.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    Unchanged,
    Changed,
    /// The ignore rules changed: scan again.
    Rescan,
}

impl ProjectSnapshot {
    /// Reads the whole project (slow on big ones: run it in the background).
    pub fn scan(root: &Path) -> Self {
        let mut files = BTreeMap::new();
        add_tree(root, &mut files);
        let mut snapshot = Self { root: root.to_path_buf(), files, ignore: project_ignore(root), openable: Arc::default() };
        snapshot.openable = Arc::new(snapshot.files.keys().cloned().collect());
        snapshot
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Files to offer in Go to File, sorted.
    pub fn openable(&self) -> Arc<Vec<PathBuf>> {
        self.openable.clone()
    }

    /// Files Find in Files reads, sorted: none that are ignored.
    pub fn searchable(&self) -> Vec<PathBuf> {
        self.files.iter().filter(|(_, ignored)| !**ignored).map(|(path, _)| path.clone()).collect()
    }

    /// Takes in paths the watcher reported as created, removed or renamed:
    /// a file is added or dropped, a folder read (or dropped) with what's in it.
    pub fn apply<'a>(&mut self, paths: impl IntoIterator<Item = &'a Path>) -> Applied {
        let mut changed = false;
        for path in paths {
            if !path.starts_with(&self.root) || path == self.root || skipped(&self.root, path) {
                continue;
            }
            if path.file_name().is_some_and(|n| n == ".gitignore") || path.ends_with(".git/info/exclude") {
                return Applied::Rescan;
            }
            // Gone (or renamed away): it and anything under it.
            let under: Vec<PathBuf> = self.files.range(path.to_path_buf()..).take_while(|(p, _)| p.starts_with(path)).map(|(p, _)| p.clone()).collect();
            changed |= !under.is_empty();
            for p in under {
                self.files.remove(&p);
            }
            let Ok(meta) = std::fs::metadata(path) else { continue };
            // In an ignored folder: Go to File doesn't list it.
            if path.parent().is_some_and(|parent| parent != self.root && self.ignored(parent, true)) {
                continue;
            }
            if meta.is_dir() {
                if !self.ignored(path, true) {
                    add_tree(path, &mut self.files);
                    changed = true;
                }
            } else if meta.is_file() && path.file_name().is_some_and(|n| n != ".DS_Store") {
                self.files.insert(path.to_path_buf(), self.ignored(path, false));
                changed = true;
            }
        }
        if changed {
            self.openable = Arc::new(self.files.keys().cloned().collect());
            Applied::Changed
        } else {
            Applied::Unchanged
        }
    }

    fn ignored(&self, path: &Path, is_dir: bool) -> bool {
        self.ignore.as_ref().is_some_and(|i| i.matched_path_or_any_parents(path, is_dir).is_ignore())
    }
}

/// Folders never read: Git's own, dependencies, and macOS's litter.
fn skipped(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| rel.components().any(|c| matches!(c.as_os_str().to_str(), Some(".git" | "node_modules" | ".DS_Store"))))
}

/// The files under `dir`: in folders the ignore rules let in, every file,
/// marked when it's ignored itself.
fn add_tree(dir: &Path, files: &mut BTreeMap<PathBuf, bool>) {
    let mut walked = std::collections::HashSet::new();
    let mut dirs = Vec::new();
    for entry in walker(dir).filter_map(Result::ok) {
        match entry.file_type() {
            Some(t) if t.is_dir() => dirs.push(entry.into_path()),
            Some(t) if t.is_file() => {
                walked.insert(entry.into_path());
            }
            _ => {}
        }
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_file()) && entry.file_name() != ".DS_Store" {
                let path = entry.path();
                let ignored = !walked.contains(&path);
                files.insert(path, ignored);
            }
        }
    }
}

fn walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| {
            let name = e.file_name();
            name != ".git" && name != "node_modules" && name != ".DS_Store"
        })
        .build()
}

/// The project's .gitignore and .git/info/exclude.
pub fn project_ignore(root: &Path) -> Option<Gitignore> {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    builder.add(root.join(".gitignore"));
    builder.add(root.join(".git/info/exclude"));
    builder.build().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(files: &[PathBuf], root: &Path) -> Vec<String> {
        files.iter().map(|p| p.strip_prefix(root).unwrap().display().to_string().replace('\\', "/")).collect()
    }

    #[test]
    fn keeps_up_with_changes_without_scanning_again() {
        let root = std::env::temp_dir().join(format!("kin-project-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        // .gitignore counts in a Git repository.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "build/\n.env\n").unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        std::fs::write(root.join(".env"), "").unwrap();
        std::fs::write(root.join("build/out.o"), "").unwrap();
        let mut snapshot = ProjectSnapshot::scan(&root);
        // Ignored files are openable, not searchable; ignored folders neither.
        assert_eq!(names(&snapshot.openable(), &root), [".env", ".gitignore", "src/a.rs"]);
        assert_eq!(names(&snapshot.searchable(), &root), [".gitignore", "src/a.rs"]);

        // A new folder with files, a new file, a removed one.
        std::fs::create_dir_all(root.join("lib/deep")).unwrap();
        std::fs::write(root.join("lib/deep/b.rs"), "").unwrap();
        std::fs::write(root.join("src/c.rs"), "").unwrap();
        std::fs::remove_file(root.join("src/a.rs")).unwrap();
        std::fs::write(root.join("build/more.o"), "").unwrap();
        let changed = [root.join("lib"), root.join("src/c.rs"), root.join("src/a.rs"), root.join("build/more.o")];
        assert_eq!(snapshot.apply(changed.iter().map(PathBuf::as_path)), Applied::Changed);
        assert_eq!(names(&snapshot.openable(), &root), [".env", ".gitignore", "lib/deep/b.rs", "src/c.rs"]);

        // A folder renamed away takes its files along.
        std::fs::rename(root.join("lib"), root.join("vendor")).unwrap();
        let moved = [root.join("lib"), root.join("vendor")];
        snapshot.apply(moved.iter().map(PathBuf::as_path));
        assert_eq!(names(&snapshot.openable(), &root), [".env", ".gitignore", "src/c.rs", "vendor/deep/b.rs"]);

        // New ignore rules mean scanning again.
        let gitignore = [root.join(".gitignore")];
        assert_eq!(snapshot.apply(gitignore.iter().map(PathBuf::as_path)), Applied::Rescan);
        std::fs::remove_dir_all(&root).ok();
    }
}
