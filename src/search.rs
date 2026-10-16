//! Text search shared by the in-file find bar and Find in Files.

use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use regex::{Regex, RegexBuilder};

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

#[derive(Clone, Debug)]
pub struct SearchQuery {
    regex: Regex,
    is_regex: bool,
}

impl SearchQuery {
    /// Returns `Ok(None)` for an empty query and `Err` for an invalid regex.
    pub fn new(text: &str, options: SearchOptions) -> Result<Option<Self>, String> {
        if text.is_empty() {
            return Ok(None);
        }
        let mut pattern = if options.regex {
            text.to_string()
        } else {
            regex::escape(text)
        };
        if options.whole_word {
            pattern = format!(r"\b(?:{pattern})\b");
        }
        RegexBuilder::new(&pattern)
            .case_insensitive(!options.case_sensitive)
            .multi_line(true)
            .build()
            .map(|regex| {
                Some(Self {
                    regex,
                    is_regex: options.regex,
                })
            })
            .map_err(|e| match e {
                regex::Error::Syntax(s) => s.lines().last().unwrap_or("Invalid regex").trim().to_string(),
                other => other.to_string(),
            })
    }

    pub fn find_all(&self, text: &str) -> Vec<Range<usize>> {
        self.regex
            .find_iter(text)
            .filter(|m| !m.is_empty())
            .map(|m| m.range())
            .collect()
    }

    /// The replacement text for a match, expanding `$1`-style groups in regex mode.
    pub fn replacement_for(&self, text: &str, range: Range<usize>, replacement: &str) -> String {
        if !self.is_regex {
            return replacement.to_string();
        }
        match self.regex.captures_at(text, range.start) {
            Some(caps) if caps.get(0).map(|m| m.range()) == Some(range) => {
                let mut out = String::new();
                caps.expand(replacement, &mut out);
                out
            }
            _ => replacement.to_string(),
        }
    }

    pub fn replace_all(&self, text: &str, replacement: &str) -> (String, usize) {
        let count = self.find_all(text).len();
        let result = if self.is_regex {
            self.regex.replace_all(text, replacement).into_owned()
        } else {
            self.regex.replace_all(text, regex::NoExpand(replacement)).into_owned()
        };
        (result, count)
    }
}

#[derive(Clone)]
pub struct ProjectMatch {
    pub path: PathBuf,
    pub relative: String,
    /// Zero-based line number.
    pub line: usize,
    pub line_text: String,
    /// Match range within `line_text`.
    pub line_range: Range<usize>,
    /// Match range within the whole file.
    pub range: Range<usize>,
    pub contents: Arc<str>,
}

pub struct ProjectResults {
    pub matches: Vec<ProjectMatch>,
    pub files: usize,
    pub truncated: bool,
}

const MAX_MATCHES: usize = 2000;
const MAX_FILE_SIZE: u64 = 4 * 1024 * 1024;

/// Every file under `root`, honoring .gitignore. Paths are absolute.
pub fn project_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = walker(root)
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .collect();
    files.sort();
    files
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

/// Searches every text file of `paths` (the project's, see
/// `crate::project`). `overrides` supplies the contents of unsaved open
/// buffers so results reflect what the user sees.
pub fn search_project(
    root: &Path,
    paths: Vec<PathBuf>,
    query: &SearchQuery,
    overrides: &HashMap<PathBuf, Arc<str>>,
) -> ProjectResults {
    let mut matches = Vec::new();
    let mut files = 0;
    let mut truncated = false;
    for path in paths {
        let contents: Arc<str> = match overrides.get(&path) {
            Some(text) => text.clone(),
            None => {
                if std::fs::metadata(&path).map(|m| m.len() > MAX_FILE_SIZE).unwrap_or(true) {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else { continue };
                if bytes.iter().take(8000).any(|b| *b == 0) {
                    continue; // binary
                }
                match String::from_utf8(bytes) {
                    Ok(s) => s.into(),
                    Err(_) => continue,
                }
            }
        };
        let found = query.find_all(&contents);
        if found.is_empty() {
            continue;
        }
        files += 1;
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let mut line = 0;
        let mut line_start = 0;
        let mut scanned = 0;
        for range in found {
            // Advance line counters incrementally.
            for (i, b) in contents.as_bytes()[scanned..range.start].iter().enumerate() {
                if *b == b'\n' {
                    line += 1;
                    line_start = scanned + i + 1;
                }
            }
            scanned = range.start;
            let line_end = contents[line_start..]
                .find('\n')
                .map_or(contents.len(), |e| line_start + e);
            let match_end = range.end.min(line_end);
            matches.push(ProjectMatch {
                path: path.clone(),
                relative: relative.clone(),
                line,
                line_text: contents[line_start..line_end].to_string(),
                line_range: range.start - line_start..match_end - line_start,
                range,
                contents: contents.clone(),
            });
            if matches.len() >= MAX_MATCHES {
                truncated = true;
                break;
            }
        }
        if truncated {
            break;
        }
    }
    ProjectResults {
        matches,
        files,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options() {
        let q = |t: &str, o| SearchQuery::new(t, o).unwrap().unwrap();
        let text = "Item item items";
        assert_eq!(q("item", SearchOptions::default()).find_all(text).len(), 3);
        let cs = SearchOptions { case_sensitive: true, ..Default::default() };
        assert_eq!(q("item", cs).find_all(text).len(), 2);
        let ww = SearchOptions { whole_word: true, ..Default::default() };
        assert_eq!(q("item", ww).find_all(text).len(), 2);
        let re = SearchOptions { regex: true, ..Default::default() };
        assert_eq!(q("i\\w+s", re).find_all(text), vec![10..15]);
        assert!(SearchQuery::new("(", re).is_err());
        assert_eq!(q("a.b", SearchOptions::default()).find_all("a.b axb"), vec![0..3]);
    }

    #[test]
    fn project_files_skip_ignored_files_and_folders() {
        let dir = std::env::temp_dir().join(format!("kuana-search-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in [".git", "config", "build", "node_modules/pkg"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join(".gitignore"), "partners.yml\nbuild/\n").unwrap();
        for file in ["config/partners.yml", "config/partners.yml.example", "build/out.js", "node_modules/pkg/index.js", ".git/HEAD"] {
            std::fs::write(dir.join(file), "").unwrap();
        }
        let names = |files: Vec<PathBuf>| -> Vec<String> {
            files.iter().map(|f| f.strip_prefix(&dir).unwrap().to_string_lossy().replace('\\', "/")).collect()
        };
        assert_eq!(names(project_files(&dir)), [".gitignore", "config/partners.yml.example"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn regex_replacement_groups() {
        let re = SearchOptions { regex: true, case_sensitive: true, ..Default::default() };
        let q = SearchQuery::new(r"(\w+)@(\w+)", re).unwrap().unwrap();
        let text = "a@b c@d";
        assert_eq!(q.replacement_for(text, 4..7, "$2@$1"), "d@c");
        assert_eq!(q.replace_all(text, "$2@$1"), ("b@a d@c".to_string(), 2));
    }
}
