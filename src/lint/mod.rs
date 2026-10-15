//! Linting runs off the main thread on a snapshot of the buffer text.

pub mod external;
mod html;
mod js;
mod markdown;
mod syntax_errors;

use std::{borrow::Cow, ops::Range, sync::Arc};

use crate::syntax::{self, Language};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    /// Byte range in the document.
    pub range: Range<usize>,
    pub severity: Severity,
    pub message: Arc<str>,
    /// Rule name, e.g. `no-unused-vars`.
    pub rule: Cow<'static, str>,
}

/// Runs the built-in linter. With `syntax_only`, style/correctness rules are
/// skipped because an external linter with project config takes over.
/// `path` locates project config (e.g. `.markdownlint.json`) and relative links.
pub fn lint(language: Language, text: &str, path: Option<&std::path::Path>, syntax_only: bool) -> Vec<Diagnostic> {
    let mut diagnostics = match language {
        Language::JavaScript => js::lint(text, 0, js::Context::File, js::Dialect::JavaScript),
        Language::TypeScript | Language::Tsx => {
            let declaration = path
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .is_some_and(|n| [".d.ts", ".d.mts", ".d.cts"].iter().any(|ext| n.ends_with(ext)));
            let dialect = match language {
                _ if declaration => js::Dialect::Declaration,
                Language::Tsx => js::Dialect::Tsx,
                _ => js::Dialect::TypeScript,
            };
            js::lint(text, 0, js::Context::File, dialect)
        }
        Language::Html => html::lint(text),
        Language::Css
        | Language::Rust
        | Language::Go
        | Language::Python
        | Language::Php
        | Language::Shell
        | Language::Json
        | Language::Yaml
        | Language::Toml
        | Language::Extension(_) => syntax::grammar(language).map_or_else(Vec::new, |g| syntax_errors::lint(&g, text)),
        Language::Markdown => markdown::lint(text, path),
        Language::PlainText => Vec::new(),
    };
    if syntax_only {
        diagnostics.retain(|d| d.rule == "syntax");
    }
    for d in &mut diagnostics {
        d.range.start = d.range.start.min(text.len());
        d.range.end = d.range.end.clamp(d.range.start, text.len());
        // Make zero-width diagnostics visible by covering at least one char.
        if d.range.is_empty() {
            let next = text[d.range.start..]
                .chars()
                .next()
                .filter(|c| *c != '\n')
                .map(char::len_utf8)
                .unwrap_or(0);
            d.range.end += next;
        }
    }
    diagnostics.sort_by_key(|d| (d.range.start, d.severity));
    diagnostics
}
