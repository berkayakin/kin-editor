//! Snippets: the tab stops of an inserted snippet (from completion or the
//! user's own), and user snippets in VS Code's format. Those live in
//! `snippets/` in the config folder: `<language id>.json` (`rust.json`,
//! `typescript.json`…) for one language, `*.code-snippets` for several (with
//! an optional `"scope": "rust,go"`). Comments and trailing commas are fine.

use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::SystemTime,
};

use serde_json::Value;

use crate::{document::Patch, settings::strip_jsonc, syntax::Language, theme::config_dir};

/// The tab stops of a snippet being filled in, in Tab order; the last is
/// where it ends (`$0`). A stop typed in several places lists them all, and
/// they're selected together so typing fills each.
#[derive(Clone, Debug, PartialEq)]
pub struct SnippetStops {
    pub stops: Vec<Vec<Range<usize>>>,
    pub current: usize,
}

impl SnippetStops {
    /// Follows an edit replacing `range` with `new_len` bytes. The current
    /// stop grows with typing in it; an edit cutting across a stop ends the
    /// snippet (returns false).
    pub fn shift(&mut self, patch: &Patch) -> bool {
        let move_by = |offset: usize, delta: isize| (offset as isize + delta) as usize;
        for (ix, stop) in self.stops.iter_mut().enumerate() {
            for r in stop {
                let (mut start, near) = patch.around(r.clone());
                let mut end = start;
                for (range, new_len) in near {
                    let delta = Patch::change(range, *new_len);
                    if ix == self.current && range.start >= r.start && range.end <= r.end {
                        end += delta;
                    } else if range.end <= r.start {
                        start += delta;
                        end += delta;
                    } else if range.start < r.end {
                        return false;
                    }
                }
                *r = move_by(r.start, start)..move_by(r.end, end);
            }
        }
        true
    }
}

/// A user snippet: what to type, then Tab.
#[derive(Clone, Debug, PartialEq)]
pub struct UserSnippet {
    pub prefix: String,
    pub body: String,
    pub description: String,
}

/// VS Code's language id for a language, which names its snippet file.
fn language_id(language: Language) -> &'static str {
    match language {
        Language::JavaScript => "javascript",
        Language::TypeScript => "typescript",
        Language::Tsx => "typescriptreact",
        Language::Html => "html",
        Language::Css => "css",
        Language::Rust => "rust",
        Language::Go => "go",
        Language::Python => "python",
        Language::Php => "php",
        Language::Shell => "shellscript",
        Language::Json => "json",
        Language::Yaml => "yaml",
        Language::Toml => "toml",
        Language::Markdown => "markdown",
        Language::PlainText => "plaintext",
        Language::Extension(_) => language.extension().map_or("plaintext", |l| l.id),
    }
}

pub fn snippets_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("snippets"))
}

/// The user's snippets for `language`, the user's own and extensions' (Zed
/// and VS Code use the same files). Asked for on every completion request
/// and Tab, so files are read again only when they changed (see `Cache`):
/// edits to them still apply right away.
pub fn user_snippets(language: Language) -> Vec<UserSnippet> {
    let mut dirs: Vec<PathBuf> = snippets_dir().into_iter().collect();
    dirs.extend(crate::extensions::snippet_dirs());
    snippets_in(&dirs, language_id(language))
}

/// The snippets for language `id` in the files of `dirs`.
fn snippets_in(dirs: &[PathBuf], id: &str) -> Vec<UserSnippet> {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    for dir in dirs {
        for path in cache.files_in(dir) {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // A language's own file, or one for several (`scope` says which).
            let global = name.ends_with(".code-snippets");
            if !global && name != format!("{id}.json") {
                continue;
            }
            let snippets = cache.snippets(&path);
            out.extend(snippets.iter().filter(|s| !global || s.applies_to(id)).map(|s| s.snippet.clone()));
        }
    }
    out.sort_by(|a, b| a.prefix.cmp(&b.prefix));
    out
}

/// Snippet folders and files as last read, to read again only what changed.
#[derive(Default)]
struct Cache {
    /// Each folder's snippet files, as of its modification time (adding,
    /// removing or renaming a file changes it); None: list it again.
    dirs: HashMap<PathBuf, (Option<SystemTime>, Vec<PathBuf>)>,
    /// Each file's snippets, as of its modification time and size; None:
    /// read it again.
    files: HashMap<PathBuf, (Option<(SystemTime, u64)>, Vec<Scoped>)>,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

/// Whether a modification time is old enough that a change made after it
/// gets a later one: file systems' clocks tick coarsely (Linux's every few
/// milliseconds), so a change right after a read could keep its time.
fn settled(modified: SystemTime) -> bool {
    SystemTime::now().duration_since(modified).is_ok_and(|age| age > std::time::Duration::from_secs(1))
}

impl Cache {
    fn files_in(&mut self, dir: &Path) -> Vec<PathBuf> {
        let modified = std::fs::metadata(dir).and_then(|m| m.modified()).ok();
        if let Some((Some(at), files)) = self.dirs.get(dir)
            && Some(*at) == modified
        {
            return files.clone();
        }
        let files: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == "json" || e == "code-snippets"))
            .collect();
        // Files that went are forgotten.
        self.files.retain(|path, _| path.parent() != Some(dir) || files.contains(path));
        self.dirs.insert(dir.to_path_buf(), (modified.filter(|m| settled(*m)), files.clone()));
        files
    }

    /// A file's snippets (none when it doesn't read), parsed again when its
    /// modification time or size changed.
    fn snippets(&mut self, path: &Path) -> &[Scoped] {
        let Some(stamp) = std::fs::metadata(path).ok().and_then(|m| Some((m.modified().ok()?, m.len()))) else {
            self.files.remove(path);
            return &[];
        };
        if !matches!(self.files.get(path), Some((Some(at), _)) if *at == stamp) {
            let snippets = std::fs::read_to_string(path).map(|text| parse(&text)).unwrap_or_default();
            self.files.insert(path.to_path_buf(), (settled(stamp.0).then_some(stamp), snippets));
        }
        &self.files[path].1
    }
}

/// A snippet and, in a `.code-snippets` file, the language ids its `scope`
/// lists (None: every language).
#[derive(Clone, Debug)]
struct Scoped {
    scope: Option<String>,
    snippet: UserSnippet,
}

impl Scoped {
    fn applies_to(&self, id: &str) -> bool {
        self.scope.as_deref().is_none_or(|scope| scope.split(',').any(|s| s.trim() == id))
    }
}

/// Parses a snippet file: every snippet, with its scope.
fn parse(text: &str) -> Vec<Scoped> {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&strip_jsonc(text)) else { return Vec::new() };
    let lines = |v: Option<&Value>| -> Vec<String> {
        match v {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            _ => Vec::new(),
        }
    };
    let mut out = Vec::new();
    for (name, snippet) in map {
        let scope = snippet.get("scope").and_then(Value::as_str).map(str::to_string);
        let body = lines(snippet.get("body")).join("\n");
        let description = snippet.get("description").and_then(Value::as_str).map_or(name.clone(), str::to_string);
        for prefix in lines(snippet.get("prefix")) {
            out.push(Scoped { scope: scope.clone(), snippet: UserSnippet { prefix, body: body.clone(), description: description.clone() } });
        }
    }
    out
}

/// Lines after the first start at the caret line's indentation, and tabs in
/// the body become the file's indent unit, as in VS Code.
pub fn indent_body(body: &str, line_indent: &str, indent_unit: &str) -> String {
    body.replace('\t', indent_unit).replace('\n', &format!("\n{line_indent}"))
}

/// Starts a snippet in the user's snippets folder when it has none, as an example.
pub fn ensure_snippets_dir() -> std::io::Result<PathBuf> {
    let dir = snippets_dir().ok_or_else(|| std::io::Error::other("no config directory"))?;
    std::fs::create_dir_all(&dir)?;
    let has_any = std::fs::read_dir(&dir)?.flatten().next().is_some();
    if !has_any {
        std::fs::write(dir.join("example.code-snippets"), EXAMPLE)?;
    }
    Ok(dir)
}

const EXAMPLE: &str = r##"// Snippets in VS Code's format: type the prefix, then Tab. $1, $2… are the
// places Tab moves to, ${1:text} has default text, $0 is where it ends.
// Put language-only snippets in <language id>.json (rust.json, typescript.json…),
// or limit one here with "scope": "rust,go".
{
  "Print to console": {
    "scope": "javascript,typescript,typescriptreact",
    "prefix": "log",
    "body": ["console.log($1);$0"],
    "description": "console.log"
  },
  "Test function": {
    "scope": "rust",
    "prefix": "test",
    "body": ["#[test]", "fn ${1:name}() {", "\t$0", "}"]
  }
}
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_follow_typing_and_end_on_crossing_edits() {
        // "foo(a, b)": $1 = a at 4..5, $2 = b at 7..8, $0 at 9.
        let mut s = SnippetStops { stops: vec![vec![4..5], vec![7..8], vec![9..9]], current: 0 };
        // Typing "xyz" over the selected `a`.
        assert!(s.shift(&Patch::new(vec![(4..5, 3)])));
        assert_eq!(s.stops, vec![vec![4..7], vec![9..10], vec![11..11]]);
        // Typing at the end of the current stop grows it; later stops move.
        assert!(s.shift(&Patch::new(vec![(7..7, 1)])));
        assert_eq!(s.stops, vec![vec![4..8], vec![10..11], vec![12..12]]);
        // An edit across stops ends the snippet.
        assert!(!s.shift(&Patch::new(vec![(5..11, 0)])));
    }

    #[test]
    fn mirrors_move_together() {
        // `$1 = $1;` with both empty at 0 and 3.
        let mut s = SnippetStops { stops: vec![vec![0..0, 3..3], vec![4..4]], current: 0 };
        // Typing `x` in both places, the later one first (as multi-cursor edits apply).
        assert!(s.shift(&Patch::new(vec![(3..3, 1)])));
        assert!(s.shift(&Patch::new(vec![(0..0, 1)])));
        assert_eq!(s.stops, vec![vec![0..1, 4..5], vec![6..6]]);
    }

    #[test]
    fn parses_vscode_snippet_files() {
        let text = r#"{
            // A comment.
            "Log": { "prefix": ["log", "cl"], "body": ["console.log($1);", "$0"], "description": "Log it", },
            "Rust only": { "scope": "rust", "prefix": "t", "body": "x" },
        }"#;
        let all = parse(text);
        assert_eq!(all.len(), 3);
        let log = &all.iter().find(|s| s.snippet.prefix == "cl").unwrap().snippet;
        assert_eq!((log.body.as_str(), log.description.as_str()), ("console.log($1);\n$0", "Log it"));
        let prefixes = |id: &str| all.iter().filter(|s| s.applies_to(id)).map(|s| s.snippet.prefix.clone()).collect::<Vec<_>>();
        assert!(!prefixes("javascript").contains(&"t".to_string()));
        assert!(prefixes("rust").contains(&"t".to_string()));
        assert_eq!(indent_body("a {\n\t$0\n}", "    ", "  "), "a {\n      $0\n    }");
    }

    #[test]
    fn snippet_files_are_read_again_when_they_change() {
        let dir = std::env::temp_dir().join(format!("kuana-snippets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dirs = [dir.clone()];
        let prefixes = || snippets_in(&dirs, "rust").into_iter().map(|s| s.prefix).collect::<Vec<_>>();
        std::fs::write(dir.join("rust.json"), r#"{ "A": { "prefix": "a", "body": "x" } }"#).unwrap();
        std::fs::write(dir.join("go.json"), r#"{ "G": { "prefix": "g", "body": "x" } }"#).unwrap();
        assert_eq!(prefixes(), ["a"]);
        // Changed in place, then a file for several languages added.
        std::fs::write(dir.join("rust.json"), r#"{ "B": { "prefix": "bb", "body": "y" } }"#).unwrap();
        assert_eq!(prefixes(), ["bb"]);
        std::fs::write(dir.join("all.code-snippets"), r#"{ "C": { "prefix": "c", "body": "z", "scope": "rust" } }"#).unwrap();
        assert_eq!(prefixes(), ["bb", "c"]);
        // Removed.
        std::fs::remove_file(dir.join("rust.json")).unwrap();
        assert_eq!(prefixes(), ["c"]);
        // A file not changed since it was read isn't read again: one rewritten
        // behind the cache's back (same size and time) still gives what was read.
        let an_hour_ago = SystemTime::now() - std::time::Duration::from_secs(3600);
        let rewrite = |prefix: &str| {
            std::fs::write(dir.join("rust.json"), format!(r#"{{ "S": {{ "prefix": "{prefix}", "body": "x" }} }}"#)).unwrap();
            std::fs::File::options().write(true).open(dir.join("rust.json")).unwrap().set_modified(an_hour_ago).unwrap();
        };
        rewrite("aa");
        assert_eq!(prefixes(), ["aa", "c"]);
        rewrite("zz");
        assert_eq!(prefixes(), ["aa", "c"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
