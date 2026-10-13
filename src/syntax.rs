//! Tree-sitter powered syntax highlighting.
//!
//! Trees are re-parsed incrementally on every edit; highlight queries only run
//! over the byte range that is actually on screen, so cost is proportional to
//! the viewport rather than the file.

use std::{ops::Range, path::Path, sync::LazyLock};

use ropey::Rope;
use tree_sitter::{
    InputEdit, Language as TsLanguage, Node, Parser, Query, QueryCursor, StreamingIterator,
    TextProvider, Tree,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    JavaScript,
    Html,
    Rust,
    Go,
    Python,
    Php,
    Markdown,
    PlainText,
}

impl Language {
    pub fn from_path(path: &Path) -> Self {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("js" | "mjs" | "cjs" | "jsx") => Language::JavaScript,
            Some("html" | "htm" | "xhtml") => Language::Html,
            Some("rs") => Language::Rust,
            Some("go") => Language::Go,
            Some("py" | "pyw" | "pyi") => Language::Python,
            Some("php" | "phtml") => Language::Php,
            Some("md" | "markdown" | "mdown" | "mkd") => Language::Markdown,
            _ => Language::PlainText,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Language::JavaScript => "JavaScript",
            Language::Html => "HTML",
            Language::Rust => "Rust",
            Language::Go => "Go",
            Language::Python => "Python",
            Language::Php => "PHP",
            Language::Markdown => "Markdown",
            Language::PlainText => "Plain Text",
        }
    }

    pub fn is_lintable(self) -> bool {
        self != Language::PlainText
    }

    pub fn line_comment(self) -> Option<&'static str> {
        match self {
            Language::JavaScript | Language::Rust | Language::Go | Language::Php => Some("// "),
            Language::Python => Some("# "),
            Language::Html | Language::Markdown | Language::PlainText => None,
        }
    }

    pub fn block_comment(self) -> Option<(&'static str, &'static str)> {
        match self {
            Language::JavaScript | Language::Rust | Language::Go | Language::Php => Some(("/* ", " */")),
            Language::Html | Language::Markdown => Some(("<!-- ", " -->")),
            Language::Python | Language::PlainText => None,
        }
    }
}

/// The tree-sitter grammar for `language`, used for highlighting and for
/// syntax-error linting.
pub fn grammar(language: Language) -> Option<TsLanguage> {
    Some(match language {
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::Html => tree_sitter_html::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        // The PHP grammar that also accepts HTML around `<?php` tags.
        Language::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        // Block structure only; inline syntax is a second grammar (see `Syntax`).
        Language::Markdown => tree_sitter_md::LANGUAGE.into(),
        Language::PlainText => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Highlight {
    #[default]
    None,
    Keyword,
    String,
    Number,
    Constant,
    Function,
    Method,
    Property,
    Type,
    Variable,
    VariableBuiltin,
    Comment,
    Operator,
    Punctuation,
    Tag,
    Attribute,
    Escape,
}

impl Highlight {
    fn from_capture(name: &str) -> Self {
        let head = name.split('.').next().unwrap_or(name);
        match (head, name) {
            (_, "function.method") => Highlight::Method,
            (_, "variable.builtin") => Highlight::VariableBuiltin,
            (_, "string.special") => Highlight::Escape,
            (_, "tag.error") => Highlight::Tag,
            // Markdown (nvim-treesitter style "text.*" captures).
            (_, "text.title" | "text.strong") => Highlight::Keyword,
            (_, "text.literal") => Highlight::String,
            (_, "text.uri") => Highlight::Property,
            (_, "text.reference") => Highlight::Function,
            (_, "text.emphasis") => Highlight::Type,
            ("keyword", _) => Highlight::Keyword,
            ("string", _) => Highlight::String,
            ("number", _) => Highlight::Number,
            ("constant", _) => Highlight::Constant,
            ("function", _) => Highlight::Function,
            ("property", _) => Highlight::Property,
            ("constructor" | "type", _) => Highlight::Type,
            ("variable" | "embedded", _) => Highlight::Variable,
            ("comment", _) => Highlight::Comment,
            ("operator", _) => Highlight::Operator,
            ("punctuation", _) => Highlight::Punctuation,
            ("tag", _) => Highlight::Tag,
            ("attribute", _) => Highlight::Attribute,
            ("escape", _) => Highlight::Escape,
            _ => Highlight::None,
        }
    }
}

const HTML_HIGHLIGHTS: &str = r#"
(tag_name) @tag
(erroneous_end_tag_name) @tag.error
(doctype) @constant
(attribute_name) @attribute
(quoted_attribute_value) @string
(attribute_value) @string
(comment) @comment
(entity) @escape
["<" ">" "</" "/>" "<!"] @punctuation.bracket
"=" @operator
"#;

const JS_EXTRA_HIGHLIGHTS: &str = r#"
(escape_sequence) @escape
"#;

/// The Rust grammar's query tags literals as `constant.builtin`; numbers read
/// better in the number color like the other languages.
const RUST_EXTRA_HIGHLIGHTS: &str = r#"
[(integer_literal) (float_literal)] @number
"#;

struct LangConfig {
    language: TsLanguage,
    query: Query,
    /// Highlight per capture index, resolved once.
    captures: Vec<Highlight>,
}

impl LangConfig {
    fn new(language: TsLanguage, source: &str) -> Self {
        let query = Query::new(&language, source).expect("invalid highlight query");
        let captures = query
            .capture_names()
            .iter()
            .map(|n| Highlight::from_capture(n))
            .collect();
        Self {
            language,
            query,
            captures,
        }
    }
}

static JS: LazyLock<LangConfig> = LazyLock::new(|| {
    LangConfig::new(
        tree_sitter_javascript::LANGUAGE.into(),
        &format!(
            "{}\n{}",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            JS_EXTRA_HIGHLIGHTS
        ),
    )
});

static HTML: LazyLock<LangConfig> =
    LazyLock::new(|| LangConfig::new(tree_sitter_html::LANGUAGE.into(), HTML_HIGHLIGHTS));

static RUST: LazyLock<LangConfig> = LazyLock::new(|| {
    LangConfig::new(
        tree_sitter_rust::LANGUAGE.into(),
        &format!("{}\n{}", tree_sitter_rust::HIGHLIGHTS_QUERY, RUST_EXTRA_HIGHLIGHTS),
    )
});

static GO: LazyLock<LangConfig> =
    LazyLock::new(|| LangConfig::new(tree_sitter_go::LANGUAGE.into(), tree_sitter_go::HIGHLIGHTS_QUERY));

static PYTHON: LazyLock<LangConfig> = LazyLock::new(|| {
    LangConfig::new(tree_sitter_python::LANGUAGE.into(), tree_sitter_python::HIGHLIGHTS_QUERY)
});

static PHP: LazyLock<LangConfig> = LazyLock::new(|| {
    LangConfig::new(tree_sitter_php::LANGUAGE_PHP.into(), tree_sitter_php::HIGHLIGHTS_QUERY)
});

static MARKDOWN: LazyLock<LangConfig> = LazyLock::new(|| {
    LangConfig::new(tree_sitter_md::LANGUAGE.into(), tree_sitter_md::HIGHLIGHT_QUERY_BLOCK)
});

static MARKDOWN_INLINE: LazyLock<LangConfig> = LazyLock::new(|| {
    LangConfig::new(tree_sitter_md::INLINE_LANGUAGE.into(), tree_sitter_md::HIGHLIGHT_QUERY_INLINE)
});

fn config(language: Language) -> Option<&'static LangConfig> {
    Some(match language {
        Language::JavaScript => &JS,
        Language::Html => &HTML,
        Language::Rust => &RUST,
        Language::Go => &GO,
        Language::Python => &PYTHON,
        Language::Php => &PHP,
        Language::Markdown => &MARKDOWN,
        Language::PlainText => return None,
    })
}

/// A second grammar parsed over parts of the main tree: JavaScript inside
/// HTML `<script>` tags, and Markdown's inline syntax inside its blocks.
fn injection(language: Language) -> Option<(&'static LangConfig, fn(Node) -> Vec<tree_sitter::Range>)> {
    match language {
        Language::Html => Some((&JS, script_ranges)),
        Language::Markdown => Some((&MARKDOWN_INLINE, markdown_inline_ranges)),
        _ => None,
    }
}

pub struct Syntax {
    language: Language,
    parser: Parser,
    tree: Option<Tree>,
    /// See [`injection`]: one tree spanning every embedded range.
    injection_parser: Option<Parser>,
    injection_tree: Option<Tree>,
}

impl Syntax {
    pub fn new(language: Language, rope: &Rope) -> Self {
        let mut parser = Parser::new();
        let mut injection_parser = None;
        if let Some(config) = config(language) {
            parser.set_language(&config.language).ok();
        }
        if let Some((inner, _)) = injection(language) {
            let mut p = Parser::new();
            p.set_language(&inner.language).ok();
            injection_parser = Some(p);
        }
        let mut this = Self {
            language,
            parser,
            tree: None,
            injection_parser,
            injection_tree: None,
        };
        this.reparse(rope);
        this
    }

    /// Informs the old tree about an edit so the next parse can reuse nodes.
    pub fn edit(&mut self, edit: &InputEdit) {
        if let Some(tree) = self.tree.as_mut() {
            tree.edit(edit);
        }
    }

    pub fn reparse(&mut self, rope: &Rope) {
        if self.language == Language::PlainText {
            return;
        }
        let tree = self.parser.parse_with_options(
            &mut |byte, _| rope_chunk(rope, byte),
            self.tree.as_ref(),
            None,
        );
        self.tree = tree;

        if let (Some(tree), Some(parser), Some((_, ranges_of))) =
            (self.tree.as_ref(), self.injection_parser.as_mut(), injection(self.language))
        {
            let ranges = ranges_of(tree.root_node());
            self.injection_tree = None;
            if !ranges.is_empty() && parser.set_included_ranges(&ranges).is_ok() {
                self.injection_tree =
                    parser.parse_with_options(&mut |byte, _| rope_chunk(rope, byte), None, None);
            }
        }
    }

    /// The smallest syntax node that strictly contains `range`, for
    /// Extend Selection. Considers embedded `<script>` trees too.
    pub fn expand(&self, range: Range<usize>) -> Option<Range<usize>> {
        let grow = |tree: &Tree| -> Option<Range<usize>> {
            let mut node = tree.root_node().descendant_for_byte_range(range.start, range.end)?;
            loop {
                let r = node.byte_range();
                if r.start <= range.start && r.end >= range.end && r != range {
                    return Some(r);
                }
                node = node.parent()?;
            }
        };
        [self.tree.as_ref(), self.injection_tree.as_ref()]
            .into_iter()
            .flatten()
            .filter_map(grow)
            .min_by_key(|r| r.len())
    }

    /// Returns non-overlapping highlighted spans (absolute byte ranges) for `range`.
    pub fn highlights(&self, rope: &Rope, range: Range<usize>) -> Vec<(Range<usize>, Highlight)> {
        let Some(tree) = self.tree.as_ref() else {
            return Vec::new();
        };
        let len = range.end.saturating_sub(range.start);
        if len == 0 {
            return Vec::new();
        }
        // A per-byte map of the visible slice. Captures are applied in query
        // order so later (more specific) patterns and nested nodes win.
        let mut map = vec![Highlight::None; len];
        let Some(config) = config(self.language) else {
            return Vec::new();
        };
        apply_query(config, tree.root_node(), rope, range.clone(), &mut map);
        if let (Some(tree), Some((inner, _))) = (self.injection_tree.as_ref(), injection(self.language)) {
            apply_query(inner, tree.root_node(), rope, range.clone(), &mut map);
        }

        let mut spans = Vec::new();
        let mut start = 0;
        for i in 1..=len {
            if i == len || map[i] != map[start] {
                if map[start] != Highlight::None {
                    spans.push((range.start + start..range.start + i, map[start]));
                }
                start = i;
            }
        }
        spans
    }
}

fn apply_query(
    config: &LangConfig,
    root: Node,
    rope: &Rope,
    range: Range<usize>,
    map: &mut [Highlight],
) {
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(range.clone());
    let mut captures = cursor.captures(&config.query, root, RopeProvider(rope));
    while let Some((m, ix)) = captures.next() {
        let capture = m.captures()[*ix];
        let h = config.captures[capture.index as usize];
        let node = capture.node;
        let s = node.start_byte().max(range.start) - range.start;
        let e = node.end_byte().min(range.end).saturating_sub(range.start);
        if s < e {
            map[s..e].fill(h);
        }
    }
}

fn script_ranges(root: Node) -> Vec<tree_sitter::Range> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "script_element" {
            for child in node.children(&mut cursor) {
                if child.kind() == "raw_text" {
                    out.push(child.range());
                }
            }
            continue;
        }
        for child in node.children(&mut cursor) {
            if child.child_count() > 0 {
                stack.push(child);
            }
        }
    }
    out.sort_by_key(|r| r.start_byte);
    out
}

/// Paragraph, heading and table-cell text, where Markdown's inline syntax
/// (emphasis, links, code spans) lives.
fn markdown_inline_ranges(root: Node) -> Vec<tree_sitter::Range> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "inline" | "pipe_table_cell") {
            out.push(node.range());
            continue;
        }
        stack.extend(node.children(&mut cursor));
    }
    out.sort_by_key(|r| r.start_byte);
    out
}

pub fn rope_chunk(rope: &Rope, byte: usize) -> &[u8] {
    if byte >= rope.len_bytes() {
        return &[];
    }
    let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
    &chunk.as_bytes()[byte - chunk_start..]
}

struct RopeProvider<'a>(&'a Rope);

impl<'a> TextProvider<&'a [u8]> for RopeProvider<'a> {
    type I = Box<dyn Iterator<Item = &'a [u8]> + 'a>;

    fn text(&mut self, node: Node) -> Self::I {
        let rope = self.0;
        let start = rope.byte_to_char(node.start_byte().min(rope.len_bytes()));
        let end = rope.byte_to_char(node.end_byte().min(rope.len_bytes()));
        Box::new(rope.slice(start..end).chunks().map(str::as_bytes))
    }
}

pub fn input_edit(
    rope_before: &Rope,
    range: Range<usize>,
    new_text: &str,
) -> InputEdit {
    let point = |rope: &Rope, byte: usize| {
        let line = rope.byte_to_line(byte);
        tree_sitter::Point::new(line, byte - rope.line_to_byte(line))
    };
    let start_position = point(rope_before, range.start);
    let old_end_position = point(rope_before, range.end);
    let new_end_byte = range.start + new_text.len();
    let newlines = new_text.matches('\n').count();
    let new_end_position = if newlines == 0 {
        tree_sitter::Point::new(start_position.row, start_position.column + new_text.len())
    } else {
        let last = new_text.rfind('\n').unwrap();
        tree_sitter::Point::new(start_position.row + newlines, new_text.len() - last - 1)
    };
    InputEdit {
        start_byte: range.start,
        old_end_byte: range.end,
        new_end_byte,
        start_position,
        old_end_position,
        new_end_position,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_queries_compile() {
        for language in [
            Language::JavaScript,
            Language::Html,
            Language::Rust,
            Language::Go,
            Language::Python,
            Language::Php,
        ] {
            let config = config(language).unwrap();
            assert!(!config.captures.is_empty(), "{language:?}");
        }
    }
}
