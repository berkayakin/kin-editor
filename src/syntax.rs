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
    TypeScript,
    Tsx,
    Html,
    Css,
    Rust,
    Go,
    Python,
    Php,
    Shell,
    Json,
    Yaml,
    Toml,
    Markdown,
    PlainText,
}

impl Language {
    /// Every language, in the order Preferences lists them.
    pub const ALL: [Language; 15] = [
        Language::JavaScript,
        Language::TypeScript,
        Language::Tsx,
        Language::Html,
        Language::Css,
        Language::Rust,
        Language::Go,
        Language::Python,
        Language::Php,
        Language::Shell,
        Language::Json,
        Language::Yaml,
        Language::Toml,
        Language::Markdown,
        Language::PlainText,
    ];

    pub fn from_path(path: &Path) -> Self {
        // Well-known files whose name, not extension, gives the language.
        match path.file_name().and_then(|n| n.to_str()).unwrap_or("") {
            ".bashrc" | ".bash_profile" | ".bash_aliases" | ".bash_logout" | ".profile" | ".zshrc"
            | ".zshenv" | ".zprofile" | ".zlogin" | ".envrc" => return Language::Shell,
            "Cargo.lock" | "poetry.lock" | "uv.lock" | "Pipfile" => return Language::Toml,
            ".babelrc" | ".eslintrc" | ".swcrc" => return Language::Json,
            _ => {}
        }
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("js" | "mjs" | "cjs" | "jsx") => Language::JavaScript,
            Some("ts" | "mts" | "cts") => Language::TypeScript,
            Some("tsx") => Language::Tsx,
            Some("html" | "htm" | "xhtml") => Language::Html,
            Some("css") => Language::Css,
            Some("rs") => Language::Rust,
            Some("go") => Language::Go,
            Some("py" | "pyw" | "pyi") => Language::Python,
            Some("php" | "phtml") => Language::Php,
            Some("sh" | "bash" | "zsh") => Language::Shell,
            Some("json" | "jsonc") => Language::Json,
            Some("yaml" | "yml") => Language::Yaml,
            Some("toml") => Language::Toml,
            Some("md" | "markdown" | "mdown" | "mkd") => Language::Markdown,
            _ => Language::PlainText,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Language::JavaScript => "JavaScript",
            Language::TypeScript => "TypeScript",
            Language::Tsx => "TSX",
            Language::Html => "HTML",
            Language::Css => "CSS",
            Language::Rust => "Rust",
            Language::Go => "Go",
            Language::Python => "Python",
            Language::Php => "PHP",
            Language::Shell => "Shell Script",
            Language::Json => "JSON",
            Language::Yaml => "YAML",
            Language::Toml => "TOML",
            Language::Markdown => "Markdown",
            Language::PlainText => "Plain Text",
        }
    }

    pub fn is_lintable(self) -> bool {
        self != Language::PlainText
    }

    pub fn line_comment(self) -> Option<&'static str> {
        match self {
            Language::JavaScript
            | Language::TypeScript
            | Language::Tsx
            | Language::Rust
            | Language::Go
            | Language::Php
            // JSONC (tsconfig, VS Code settings); the grammar accepts comments.
            | Language::Json => Some("// "),
            Language::Python | Language::Shell | Language::Yaml | Language::Toml => Some("# "),
            Language::Html | Language::Css | Language::Markdown | Language::PlainText => None,
        }
    }

    pub fn block_comment(self) -> Option<(&'static str, &'static str)> {
        match self {
            Language::JavaScript
            | Language::TypeScript
            | Language::Tsx
            | Language::Css
            | Language::Rust
            | Language::Go
            | Language::Php
            | Language::Json => Some(("/* ", " */")),
            Language::Html | Language::Markdown => Some(("<!-- ", " -->")),
            Language::Python | Language::Shell | Language::Yaml | Language::Toml | Language::PlainText => None,
        }
    }
}

/// The tree-sitter grammar for `language`, used for highlighting and for
/// syntax-error linting.
pub fn grammar(language: Language) -> Option<TsLanguage> {
    Some(match language {
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::Html => tree_sitter_html::LANGUAGE.into(),
        Language::Css => tree_sitter_css::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        // The PHP grammar that also accepts HTML around `<?php` tags.
        Language::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        Language::Shell => tree_sitter_bash::LANGUAGE.into(),
        Language::Json => tree_sitter_json::LANGUAGE.into(),
        Language::Yaml => tree_sitter_yaml::LANGUAGE.into(),
        Language::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
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
            (_, "string.special.key") => Highlight::Property,
            (_, "tag.error") => Highlight::Tag,
            // Markdown (nvim-treesitter style "text.*" captures).
            (_, "text.title" | "text.strong") => Highlight::Keyword,
            (_, "text.literal") => Highlight::String,
            (_, "text.uri") => Highlight::Property,
            (_, "text.reference") => Highlight::Function,
            (_, "text.emphasis") => Highlight::Type,
            // CSS at-rules capture under their own names.
            ("keyword" | "charset" | "import" | "keyframes" | "media" | "namespace" | "supports", _) => {
                Highlight::Keyword
            }
            ("string", _) => Highlight::String,
            ("number", _) => Highlight::Number,
            ("constant" | "boolean" | "label", _) => Highlight::Constant,
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

/// The grammar's own query captures keys before strings; here the later
/// pattern wins, so keys must come after `(string)` to get their own color.
const JSON_HIGHLIGHTS: &str = r#"
(string) @string
(pair key: (string) @property)
(number) @number
[(null) (true) (false)] @constant.builtin
(escape_sequence) @escape
(comment) @comment
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
            "{}\n{}\n{}",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            JS_EXTRA_HIGHLIGHTS
        ),
    )
});

/// TypeScript's query only adds types and keywords on top of JavaScript's.
fn typescript_config(language: TsLanguage, jsx: bool) -> LangConfig {
    let jsx = if jsx { tree_sitter_javascript::JSX_HIGHLIGHT_QUERY } else { "" };
    LangConfig::new(
        language,
        &format!(
            "{}\n{}\n{}\n{}",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            jsx,
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            JS_EXTRA_HIGHLIGHTS
        ),
    )
}

static TYPESCRIPT: LazyLock<LangConfig> =
    LazyLock::new(|| typescript_config(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), false));

static TSX: LazyLock<LangConfig> =
    LazyLock::new(|| typescript_config(tree_sitter_typescript::LANGUAGE_TSX.into(), true));

static CSS: LazyLock<LangConfig> =
    LazyLock::new(|| LangConfig::new(tree_sitter_css::LANGUAGE.into(), tree_sitter_css::HIGHLIGHTS_QUERY));

static SHELL: LazyLock<LangConfig> =
    LazyLock::new(|| LangConfig::new(tree_sitter_bash::LANGUAGE.into(), tree_sitter_bash::HIGHLIGHT_QUERY));

static JSON: LazyLock<LangConfig> =
    LazyLock::new(|| LangConfig::new(tree_sitter_json::LANGUAGE.into(), JSON_HIGHLIGHTS));

static YAML: LazyLock<LangConfig> =
    LazyLock::new(|| LangConfig::new(tree_sitter_yaml::LANGUAGE.into(), tree_sitter_yaml::HIGHLIGHTS_QUERY));

static TOML: LazyLock<LangConfig> =
    LazyLock::new(|| LangConfig::new(tree_sitter_toml_ng::LANGUAGE.into(), tree_sitter_toml_ng::HIGHLIGHTS_QUERY));

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

// ---- Outline -------------------------------------------------------------------

/// A named definition in a file, for File Structure.
#[derive(Clone, Debug, PartialEq)]
pub struct Symbol {
    pub name: String,
    /// What it is: "function", "class", "heading"…
    pub kind: &'static str,
    /// Start of the name, where navigation lands.
    pub offset: usize,
    /// Nesting level among the other symbols.
    pub depth: usize,
    /// The whole definition (header and body).
    pub range: Range<usize>,
}

/// Outline queries capture `@name` inside a `@definition.<kind>` node, the
/// convention of tree-sitter's tags queries.
struct OutlineConfig {
    query: Query,
    /// Per capture index: `Some(kind)` for definitions, `None` otherwise.
    kinds: Vec<Option<&'static str>>,
    name: u32,
}

impl OutlineConfig {
    fn new(language: TsLanguage, source: &str) -> Self {
        let query = Query::new(&language, source).expect("invalid outline query");
        let kinds = query
            .capture_names()
            .iter()
            .map(|n| n.strip_prefix("definition.").map(symbol_kind))
            .collect();
        let name = query.capture_index_for_name("name").expect("outline query without @name");
        Self { query, kinds, name }
    }
}

fn symbol_kind(kind: &str) -> &'static str {
    match kind {
        "function" => "function",
        "method" => "method",
        "class" => "class",
        "interface" => "interface",
        "module" => "module",
        "macro" => "macro",
        "constant" => "constant",
        "type" => "type",
        "enum" => "enum",
        "implementation" => "impl",
        "heading" => "heading",
        "selector" => "selector",
        "keyframes" => "keyframes",
        "table" => "table",
        "key" => "key",
        _ => "symbol",
    }
}

/// Tags queries group structs, enums and type aliases as "class"; the node
/// says which one it is.
fn node_kind_label(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "struct_item" | "struct_type" => "struct",
        "enum_item" | "enum_declaration" => "enum",
        "union_item" => "union",
        "trait_item" | "trait_declaration" => "trait",
        "type_item" | "type_alias_declaration" | "type_spec" => "type",
        "interface_declaration" | "interface_type" => "interface",
        "const_item" | "static_item" => "constant",
        _ => return None,
    })
}

/// Rust's tags query only references impl blocks; as containers they make
/// methods easy to find.
const RUST_OUTLINE: &str = r#"
(impl_item type: (_) @name) @definition.implementation
"#;

/// Classes, type aliases and enums, whose names TypeScript parses as types.
const TYPESCRIPT_OUTLINE: &str = r#"
(class_declaration name: (type_identifier) @name) @definition.class
(type_alias_declaration name: (type_identifier) @name) @definition.type
(enum_declaration name: (identifier) @name) @definition.enum
"#;

const MARKDOWN_OUTLINE: &str = r#"
(section (atx_heading heading_content: (inline) @name)) @definition.heading
(section (setext_heading heading_content: (paragraph) @name)) @definition.heading
"#;

const CSS_OUTLINE: &str = r#"
(rule_set (selectors) @name) @definition.selector
(keyframes_statement (keyframes_name) @name) @definition.keyframes
"#;

const SHELL_OUTLINE: &str = r#"
(function_definition name: (word) @name) @definition.function
"#;

const JSON_OUTLINE: &str = r#"
(pair key: (string (string_content) @name)) @definition.key
"#;

const YAML_OUTLINE: &str = r#"
(block_mapping_pair key: (flow_node) @name) @definition.key
"#;

const TOML_OUTLINE: &str = r#"
(table [(bare_key) (dotted_key) (quoted_key)] @name) @definition.table
(table_array_element [(bare_key) (dotted_key) (quoted_key)] @name) @definition.table
"#;

fn outline_config(language: Language) -> Option<&'static OutlineConfig> {
    static JS: LazyLock<OutlineConfig> =
        LazyLock::new(|| OutlineConfig::new(tree_sitter_javascript::LANGUAGE.into(), tree_sitter_javascript::TAGS_QUERY));
    static TS: LazyLock<OutlineConfig> = LazyLock::new(|| {
        OutlineConfig::new(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            &format!("{}\n{}\n{}", tree_sitter_javascript::TAGS_QUERY, tree_sitter_typescript::TAGS_QUERY, TYPESCRIPT_OUTLINE),
        )
    });
    static TSX: LazyLock<OutlineConfig> = LazyLock::new(|| {
        OutlineConfig::new(
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            &format!("{}\n{}\n{}", tree_sitter_javascript::TAGS_QUERY, tree_sitter_typescript::TAGS_QUERY, TYPESCRIPT_OUTLINE),
        )
    });
    static RUST: LazyLock<OutlineConfig> = LazyLock::new(|| {
        OutlineConfig::new(tree_sitter_rust::LANGUAGE.into(), &format!("{}\n{}", tree_sitter_rust::TAGS_QUERY, RUST_OUTLINE))
    });
    static GO: LazyLock<OutlineConfig> =
        LazyLock::new(|| OutlineConfig::new(tree_sitter_go::LANGUAGE.into(), tree_sitter_go::TAGS_QUERY));
    static PYTHON: LazyLock<OutlineConfig> =
        LazyLock::new(|| OutlineConfig::new(tree_sitter_python::LANGUAGE.into(), tree_sitter_python::TAGS_QUERY));
    static PHP: LazyLock<OutlineConfig> =
        LazyLock::new(|| OutlineConfig::new(tree_sitter_php::LANGUAGE_PHP.into(), tree_sitter_php::TAGS_QUERY));
    static MARKDOWN: LazyLock<OutlineConfig> =
        LazyLock::new(|| OutlineConfig::new(tree_sitter_md::LANGUAGE.into(), MARKDOWN_OUTLINE));
    static CSS: LazyLock<OutlineConfig> = LazyLock::new(|| OutlineConfig::new(tree_sitter_css::LANGUAGE.into(), CSS_OUTLINE));
    static SHELL: LazyLock<OutlineConfig> =
        LazyLock::new(|| OutlineConfig::new(tree_sitter_bash::LANGUAGE.into(), SHELL_OUTLINE));
    static JSON: LazyLock<OutlineConfig> =
        LazyLock::new(|| OutlineConfig::new(tree_sitter_json::LANGUAGE.into(), JSON_OUTLINE));
    static YAML: LazyLock<OutlineConfig> =
        LazyLock::new(|| OutlineConfig::new(tree_sitter_yaml::LANGUAGE.into(), YAML_OUTLINE));
    static TOML: LazyLock<OutlineConfig> =
        LazyLock::new(|| OutlineConfig::new(tree_sitter_toml_ng::LANGUAGE.into(), TOML_OUTLINE));
    Some(match language {
        Language::JavaScript => &JS,
        Language::TypeScript => &TS,
        Language::Tsx => &TSX,
        Language::Rust => &RUST,
        Language::Go => &GO,
        Language::Python => &PYTHON,
        Language::Php => &PHP,
        Language::Markdown => &MARKDOWN,
        Language::Css => &CSS,
        Language::Shell => &SHELL,
        Language::Json => &JSON,
        Language::Yaml => &YAML,
        Language::Toml => &TOML,
        Language::Html | Language::PlainText => return None,
    })
}

/// Huge data files (lockfiles) have more keys than anyone browses.
const MAX_SYMBOLS: usize = 5000;

fn config(language: Language) -> Option<&'static LangConfig> {
    Some(match language {
        Language::JavaScript => &JS,
        Language::TypeScript => &TYPESCRIPT,
        Language::Tsx => &TSX,
        Language::Html => &HTML,
        Language::Css => &CSS,
        Language::Rust => &RUST,
        Language::Go => &GO,
        Language::Python => &PYTHON,
        Language::Php => &PHP,
        Language::Shell => &SHELL,
        Language::Json => &JSON,
        Language::Yaml => &YAML,
        Language::Toml => &TOML,
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

    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
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

    /// The bracket at `offset` (or just before it) and its partner. Brackets
    /// in strings and comments aren't grammar tokens, so they never match.
    pub fn matching_bracket(&self, offset: usize) -> Option<(usize, usize)> {
        // The bracket after the cursor wins, as in JetBrains.
        for at in [Some(offset), offset.checked_sub(1)].into_iter().flatten() {
            for tree in [self.injection_tree.as_ref(), self.tree.as_ref()].into_iter().flatten() {
                if let Some(partner) = bracket_partner(tree.root_node(), at) {
                    return Some((at, partner));
                }
            }
        }
        None
    }

    /// The file's definitions in document order, nested by containment.
    pub fn symbols(&self, rope: &Rope) -> Vec<Symbol> {
        let (Some(tree), Some(config)) = (self.tree.as_ref(), outline_config(self.language)) else {
            return Vec::new();
        };
        // (definition range, kind, name range)
        let mut found: Vec<(Range<usize>, &'static str, Range<usize>)> = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&config.query, tree.root_node(), RopeProvider(rope));
        while let Some(m) = matches.next() {
            let mut def = None;
            let mut name = None;
            for c in m.captures() {
                if c.index == config.name {
                    name = Some(c.node.byte_range());
                } else if let Some(kind) = config.kinds[c.index as usize] {
                    def = Some((c.node.byte_range(), node_kind_label(c.node.kind()).unwrap_or(kind)));
                }
            }
            if let (Some((range, kind)), Some(name)) = (def, name) {
                found.push((range, kind, name));
            }
            if found.len() >= MAX_SYMBOLS * 2 {
                break;
            }
        }
        // Several patterns can match one definition (a Rust method is also a
        // function); the first, more specific one wins.
        found.sort_by_key(|(range, _, _)| (range.start, std::cmp::Reverse(range.end)));
        found.dedup_by(|b, a| a.0 == b.0);

        let mut symbols = Vec::new();
        let mut open: Vec<usize> = Vec::new(); // ends of enclosing definitions
        for (range, kind, name) in found.into_iter().take(MAX_SYMBOLS) {
            while open.last().is_some_and(|&end| end <= range.start) {
                open.pop();
            }
            let text = rope.byte_slice(name.clone()).to_string();
            let mut name_text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if name_text.len() > 100 {
                name_text.truncate(name_text.floor_char_boundary(100));
                name_text.push('…');
            }
            if !name_text.is_empty() {
                symbols.push(Symbol { name: name_text, kind, offset: name.start, depth: open.len(), range: range.clone() });
            }
            open.push(range.end);
        }
        symbols
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

/// The offset of the bracket matching the bracket token at `at`: the nearest
/// sibling of the other kind, since a pair shares a parent node.
fn bracket_partner(root: Node, at: usize) -> Option<usize> {
    let node = root.descendant_for_byte_range(at, at + 1)?;
    if node.is_named() || node.byte_range() != (at..at + 1) {
        return None;
    }
    let (partner, forward) = match node.kind() {
        "(" => (")", true),
        "[" => ("]", true),
        "{" => ("}", true),
        ")" => ("(", false),
        "]" => ("[", false),
        "}" => ("{", false),
        _ => return None,
    };
    let mut sibling = node;
    loop {
        sibling = if forward { sibling.next_sibling()? } else { sibling.prev_sibling()? };
        if !sibling.is_named() && sibling.kind() == partner {
            return Some(sibling.start_byte());
        }
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
        for language in ALL_HIGHLIGHTED {
            let config = config(language).unwrap();
            assert!(!config.captures.is_empty(), "{language:?}");
        }
    }

    const ALL_HIGHLIGHTED: [Language; 14] = [
        Language::JavaScript,
        Language::TypeScript,
        Language::Tsx,
        Language::Html,
        Language::Css,
        Language::Rust,
        Language::Go,
        Language::Python,
        Language::Php,
        Language::Shell,
        Language::Json,
        Language::Yaml,
        Language::Toml,
        Language::Markdown,
    ];

    fn highlights_of(path: &str, src: &str) -> Vec<(String, Highlight)> {
        let language = Language::from_path(Path::new(path));
        let rope = Rope::from_str(src);
        let syntax = Syntax::new(language, &rope);
        syntax
            .highlights(&rope, 0..src.len())
            .into_iter()
            .map(|(r, h)| (src[r].to_string(), h))
            .collect()
    }

    fn has(spans: &[(String, Highlight)], text: &str, h: Highlight) -> bool {
        spans.iter().any(|(t, x)| t == text && *x == h)
    }

    fn outline(path: &str, src: &str) -> Vec<(String, &'static str, usize)> {
        let language = Language::from_path(Path::new(path));
        let rope = Rope::from_str(src);
        Syntax::new(language, &rope).symbols(&rope).into_iter().map(|s| (s.name, s.kind, s.depth)).collect()
    }

    #[test]
    fn outline_queries_compile() {
        for language in ALL_HIGHLIGHTED {
            if language != Language::Html {
                assert!(outline_config(language).is_some(), "{language:?}");
            }
        }
    }

    #[test]
    fn builds_outlines() {
        let rust = outline("a.rs", "struct Point;\nimpl Point {\n    fn new() -> Self { Point }\n}\nfn main() {}\n");
        assert_eq!(
            rust,
            [
                ("Point".to_string(), "struct", 0),
                ("Point".to_string(), "impl", 0),
                ("new".to_string(), "method", 1),
                ("main".to_string(), "function", 0)
            ]
        );
        let ts = outline("a.ts", "interface P { x: number }\nclass A {\n  run() {}\n}\nexport function f() {}\n");
        assert!(ts.contains(&("P".to_string(), "interface", 0)), "{ts:?}");
        assert!(ts.contains(&("A".to_string(), "class", 0)), "{ts:?}");
        assert!(ts.contains(&("run".to_string(), "method", 1)), "{ts:?}");
        assert!(ts.contains(&("f".to_string(), "function", 0)), "{ts:?}");
        let md = outline("a.md", "# Title\n\nText\n\n## Part one\n\n## Part two\n");
        assert_eq!(
            md,
            [
                ("Title".to_string(), "heading", 0),
                ("Part one".to_string(), "heading", 1),
                ("Part two".to_string(), "heading", 1)
            ]
        );
        let toml = outline("Cargo.toml", "[package]\nname = \"x\"\n[dependencies]\n");
        assert_eq!(toml.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), ["package", "dependencies"]);
        let css = outline("a.css", ".a, .b { color: red; }\n");
        assert_eq!(css, [(".a, .b".to_string(), "selector", 0)]);
        let json = outline("a.json", "{\"a\": {\"b\": 1}}\n");
        assert_eq!(json, [("a".to_string(), "key", 0), ("b".to_string(), "key", 1)]);
    }

    #[test]
    fn matches_brackets_outside_strings() {
        let src = "fn f(a: [u8; 2]) { let s = \"(\"; }\n";
        let rope = Rope::from_str(src);
        let syntax = Syntax::new(Language::Rust, &rope);
        let at = |c: char, nth: usize| src.match_indices(c).nth(nth).unwrap().0;
        assert_eq!(syntax.matching_bracket(at('(', 0)), Some((at('(', 0), at(')', 0))));
        // After a closing bracket, the cursor's left neighbor matches.
        assert_eq!(syntax.matching_bracket(at(')', 0) + 1), Some((at(')', 0), at('(', 0))));
        assert_eq!(syntax.matching_bracket(at('[', 0)), Some((at('[', 0), at(']', 0))));
        assert_eq!(syntax.matching_bracket(at('{', 0)), Some((at('{', 0), at('}', 0))));
        // The "(" inside the string literal isn't a bracket.
        assert_eq!(syntax.matching_bracket(at('(', 1)), None);
        assert_eq!(syntax.matching_bracket(at('l', 0)), None);
    }

    #[test]
    fn detects_languages_by_name_and_extension() {
        for (path, language) in [
            ("a.ts", Language::TypeScript),
            ("a.d.ts", Language::TypeScript),
            ("a.mts", Language::TypeScript),
            ("App.tsx", Language::Tsx),
            ("style.css", Language::Css),
            ("run.sh", Language::Shell),
            (".zshrc", Language::Shell),
            ("package.json", Language::Json),
            ("tsconfig.jsonc", Language::Json),
            ("ci.yml", Language::Yaml),
            ("Cargo.toml", Language::Toml),
            ("Cargo.lock", Language::Toml),
            ("yarn.lock", Language::PlainText),
        ] {
            assert_eq!(Language::from_path(Path::new(path)), language, "{path}");
        }
    }

    #[test]
    fn new_languages_highlight() {
        let ts = highlights_of("a.ts", "interface Point { x: number }\nconst p: Point = { x: 1 };\n");
        assert!(has(&ts, "interface", Highlight::Keyword), "{ts:?}");
        assert!(has(&ts, "number", Highlight::Type), "{ts:?}");
        assert!(has(&ts, "const", Highlight::Keyword), "{ts:?}");
        let tsx = highlights_of("a.tsx", "const a = <div className=\"x\" />;\n");
        assert!(has(&tsx, "div", Highlight::Tag), "{tsx:?}");
        assert!(has(&tsx, "className", Highlight::Attribute), "{tsx:?}");
        let css = highlights_of("a.css", "@media screen { .a { color: red; } }\n");
        assert!(has(&css, "@media", Highlight::Keyword), "{css:?}");
        assert!(has(&css, "color", Highlight::Property), "{css:?}");
        let json = highlights_of("a.json", "{\"name\": \"kin\", \"n\": 1, \"ok\": true}\n");
        assert!(has(&json, "\"name\"", Highlight::Property), "{json:?}");
        assert!(has(&json, "\"kin\"", Highlight::String), "{json:?}");
        assert!(has(&json, "true", Highlight::Constant), "{json:?}");
        let yaml = highlights_of("a.yaml", "name: kin\non: true\n");
        assert!(has(&yaml, "name", Highlight::Property), "{yaml:?}");
        assert!(has(&yaml, "kin", Highlight::String), "{yaml:?}");
        let toml = highlights_of("a.toml", "[package]\nname = \"kin\"\n");
        assert!(has(&toml, "\"kin\"", Highlight::String), "{toml:?}");
        let sh = highlights_of("a.sh", "# hi\nif [ -n \"$X\" ]; then echo ok; fi\n");
        assert!(has(&sh, "# hi", Highlight::Comment), "{sh:?}");
        assert!(has(&sh, "if", Highlight::Keyword), "{sh:?}");
    }
}
