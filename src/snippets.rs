//! Snippets: the tab stops of an inserted snippet (from completion or the
//! user's own), and user snippets in VS Code's format. Those live in
//! `snippets/` in the config folder: `<language id>.json` (`rust.json`,
//! `typescript.json`…) for one language, `*.code-snippets` for several (with
//! an optional `"scope": "rust,go"`). Comments and trailing commas are fine.

use std::{ops::Range, path::PathBuf};

use serde_json::Value;

use crate::{settings::strip_jsonc, syntax::Language, theme::config_dir};

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
    pub fn shift(&mut self, range: &Range<usize>, new_len: usize) -> bool {
        let delta = new_len as isize - range.len() as isize;
        let move_by = |offset: &mut usize| *offset = (*offset as isize + delta) as usize;
        for (ix, stop) in self.stops.iter_mut().enumerate() {
            for r in stop {
                if ix == self.current && range.start >= r.start && range.end <= r.end {
                    move_by(&mut r.end);
                } else if range.end <= r.start {
                    move_by(&mut r.start);
                    move_by(&mut r.end);
                } else if range.start < r.end {
                    return false;
                }
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

/// The user's snippets for `language`. Read on each use: the files are small,
/// and edits to them apply right away.
pub fn user_snippets(language: Language) -> Vec<UserSnippet> {
    // The user's own and extensions' (Zed and VS Code use the same files).
    let mut dirs: Vec<PathBuf> = snippets_dir().into_iter().collect();
    dirs.extend(crate::extensions::snippet_dirs());
    let id = language_id(language);
    let mut out = Vec::new();
    for entry in dirs.iter().filter_map(|dir| std::fs::read_dir(dir).ok()).flatten().flatten() {
        let path = entry.path();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let global = name.ends_with(".code-snippets");
        if !global && name != format!("{id}.json") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            out.extend(parse(&text, global.then_some(id)));
        }
    }
    out.sort_by(|a, b| a.prefix.cmp(&b.prefix));
    out
}

/// Parses a snippet file. `scope_for`: in a `.code-snippets` file, keep only
/// snippets without a scope or whose scope lists this language id.
fn parse(text: &str, scope_for: Option<&str>) -> Vec<UserSnippet> {
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
        if let (Some(id), Some(scope)) = (scope_for, snippet.get("scope").and_then(Value::as_str))
            && !scope.split(',').any(|s| s.trim() == id)
        {
            continue;
        }
        let body = lines(snippet.get("body")).join("\n");
        let description = snippet.get("description").and_then(Value::as_str).map_or(name.clone(), str::to_string);
        for prefix in lines(snippet.get("prefix")) {
            out.push(UserSnippet { prefix, body: body.clone(), description: description.clone() });
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
        assert!(s.shift(&(4..5), 3));
        assert_eq!(s.stops, vec![vec![4..7], vec![9..10], vec![11..11]]);
        // Typing at the end of the current stop grows it; later stops move.
        assert!(s.shift(&(7..7), 1));
        assert_eq!(s.stops, vec![vec![4..8], vec![10..11], vec![12..12]]);
        // An edit across stops ends the snippet.
        assert!(!s.shift(&(5..11), 0));
    }

    #[test]
    fn mirrors_move_together() {
        // `$1 = $1;` with both empty at 0 and 3.
        let mut s = SnippetStops { stops: vec![vec![0..0, 3..3], vec![4..4]], current: 0 };
        // Typing `x` in both places, the later one first (as multi-cursor edits apply).
        assert!(s.shift(&(3..3), 1));
        assert!(s.shift(&(0..0), 1));
        assert_eq!(s.stops, vec![vec![0..1, 4..5], vec![6..6]]);
    }

    #[test]
    fn parses_vscode_snippet_files() {
        let text = r#"{
            // A comment.
            "Log": { "prefix": ["log", "cl"], "body": ["console.log($1);", "$0"], "description": "Log it", },
            "Rust only": { "scope": "rust", "prefix": "t", "body": "x" },
        }"#;
        let all = parse(text, None);
        assert_eq!(all.len(), 3);
        let log = all.iter().find(|s| s.prefix == "cl").unwrap();
        assert_eq!((log.body.as_str(), log.description.as_str()), ("console.log($1);\n$0", "Log it"));
        let js = parse(text, Some("javascript"));
        assert!(js.iter().all(|s| s.prefix != "t"));
        assert!(parse(text, Some("rust")).iter().any(|s| s.prefix == "t"));
        assert_eq!(indent_body("a {\n\t$0\n}", "    ", "  "), "a {\n      $0\n    }");
    }
}
