//! Tree-sitter powered syntax highlighting.
//!
//! Trees are re-parsed incrementally on every edit; highlight queries only run
//! over the byte range that is actually on screen, so cost is proportional to
//! the viewport rather than the file.

use std::{
    collections::HashMap,
    ops::{ControlFlow, Deref, DerefMut, Range},
    path::Path,
    sync::{LazyLock, Mutex, RwLock},
    time::{Duration, Instant},
};

use ropey::Rope;
use tree_sitter::{
    InputEdit, Language as TsLanguage, Node, ParseOptions, Parser, Query, QueryCursor, StreamingIterator,
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
    /// A language an extension adds: an index into `crate::extensions`'
    /// registry, stable while Kin runs.
    Extension(u16),
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

    /// The built-in languages, then the extensions', then plain text.
    pub fn all() -> Vec<Language> {
        let mut all: Vec<Language> = Language::ALL.into_iter().filter(|l| *l != Language::PlainText).collect();
        all.extend(crate::extensions::languages().into_iter().map(Language::Extension));
        all.push(Language::PlainText);
        all
    }

    /// An extension's language naming this very file, else the built-in
    /// language for `path`, else an extension's by extension.
    pub fn from_path(path: &Path) -> Self {
        if let Some(ix) = crate::extensions::language_for_file_name(path) {
            return Language::Extension(ix);
        }
        match crate::languages::for_path(path) {
            Some(language) => language,
            None => crate::extensions::language_for_path(path).map_or(Language::PlainText, Language::Extension),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Language::PlainText => "Plain Text",
            Language::Extension(ix) => crate::extensions::language(ix).map_or("Plain Text", |l| l.name),
            _ => crate::languages::def(self).map_or("Plain Text", |d| d.name),
        }
    }

    /// The extension's definition, for an extension language.
    pub fn extension(self) -> Option<&'static crate::extensions::LanguageDef> {
        match self {
            Language::Extension(ix) => crate::extensions::language(ix),
            _ => None,
        }
    }

    pub fn is_lintable(self) -> bool {
        self != Language::PlainText
    }

    pub fn line_comment(self) -> Option<&'static str> {
        match self {
            Language::Extension(_) => self.extension().and_then(|l| l.line_comment),
            _ => crate::languages::def(self).and_then(|d| d.line_comment),
        }
    }

    pub fn block_comment(self) -> Option<(&'static str, &'static str)> {
        match self {
            Language::Extension(_) => self.extension().and_then(|l| l.block_comment),
            _ => crate::languages::def(self).and_then(|d| d.block_comment),
        }
    }
}

/// The tree-sitter grammar for `language`, used for highlighting and for
/// syntax-error linting.
pub fn grammar(language: Language) -> Option<TsLanguage> {
    match language {
        Language::Extension(_) => language.extension()?.grammar(),
        _ => Some(crate::languages::def(language)?.grammar()),
    }
}

/// What a highlight query captured: the capture's name ("keyword.control",
/// "function.method"), interned. A theme colors a name by the longest dotted
/// prefix it defines (`Theme::highlight_color`), else by its `base`: the
/// few kinds every theme has a color for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Highlight(u16);

/// The base kinds' capture names, in the order of their `Highlight` ids.
const BASE: [&str; 17] = [
    "", "keyword", "string", "number", "constant", "function", "function.method", "property", "type", "variable",
    "variable.builtin", "comment", "operator", "punctuation", "tag", "attribute", "escape",
];

struct Names {
    /// Per id: the name and its base kind.
    list: Vec<(&'static str, Highlight)>,
    ids: HashMap<&'static str, u16>,
}

static NAMES: LazyLock<RwLock<Names>> = LazyLock::new(|| {
    let list: Vec<(&'static str, Highlight)> = BASE.iter().enumerate().map(|(i, n)| (*n, Highlight(i as u16))).collect();
    let ids = list.iter().enumerate().map(|(i, (n, _))| (*n, i as u16)).collect();
    RwLock::new(Names { list, ids })
});

// The base kinds, named like the variants they used to be.
#[allow(non_upper_case_globals)]
impl Highlight {
    pub const None: Highlight = Highlight(0);
    pub const Keyword: Highlight = Highlight(1);
    pub const String: Highlight = Highlight(2);
    pub const Number: Highlight = Highlight(3);
    pub const Constant: Highlight = Highlight(4);
    pub const Function: Highlight = Highlight(5);
    pub const Method: Highlight = Highlight(6);
    pub const Property: Highlight = Highlight(7);
    pub const Type: Highlight = Highlight(8);
    pub const Variable: Highlight = Highlight(9);
    pub const VariableBuiltin: Highlight = Highlight(10);
    pub const Comment: Highlight = Highlight(11);
    pub const Operator: Highlight = Highlight(12);
    pub const Punctuation: Highlight = Highlight(13);
    pub const Tag: Highlight = Highlight(14);
    pub const Attribute: Highlight = Highlight(15);
    pub const Escape: Highlight = Highlight(16);
}

impl Highlight {
    /// The id of a capture name (queries are compiled once, so names are few).
    pub fn from_capture(name: &str) -> Self {
        if let Some(&id) = NAMES.read().unwrap().ids.get(name) {
            return Highlight(id);
        }
        let mut names = NAMES.write().unwrap();
        if let Some(&id) = names.ids.get(name) {
            return Highlight(id);
        }
        let Ok(id) = u16::try_from(names.list.len()) else { return Highlight::None };
        let name: &'static str = Box::leak(name.to_string().into_boxed_str());
        names.list.push((name, Self::base_of(name)));
        names.ids.insert(name, id);
        Highlight(id)
    }

    pub fn name(self) -> &'static str {
        NAMES.read().unwrap().list.get(self.0 as usize).map_or("", |(n, _)| n)
    }

    /// The base kind, for themes that don't color this name.
    pub fn base(self) -> Highlight {
        if (self.0 as usize) < BASE.len() {
            return self;
        }
        NAMES.read().unwrap().list.get(self.0 as usize).map_or(Highlight::None, |(_, b)| *b)
    }

    /// The base kind of the longest prefix of `name` that has one.
    fn base_of(name: &str) -> Highlight {
        let mut prefix = name;
        loop {
            if let Some(base) = Self::base_at(prefix) {
                return base;
            }
            match prefix.rfind('.') {
                Some(dot) => prefix = &prefix[..dot],
                None => return Highlight::None,
            }
        }
    }

    /// The base kind for exactly this name (one level of a dotted name).
    pub(crate) fn base_at(name: &str) -> Option<Highlight> {
        Some(match name {
            "function.method" => Highlight::Method,
            "variable.builtin" => Highlight::VariableBuiltin,
            "string.special" => Highlight::Escape,
            "string.special.key" => Highlight::Property,
            "tag.error" => Highlight::Tag,
            // Markdown (nvim-treesitter style "text.*" captures).
            "text.title" | "text.strong" => Highlight::Keyword,
            "text.literal" => Highlight::String,
            "text.uri" => Highlight::Property,
            "text.reference" => Highlight::Function,
            "text.emphasis" => Highlight::Type,
            // CSS at-rules capture under their own names.
            "keyword" | "charset" | "import" | "keyframes" | "media" | "namespace" | "supports" => Highlight::Keyword,
            "string" => Highlight::String,
            "number" => Highlight::Number,
            "constant" | "boolean" | "label" => Highlight::Constant,
            "function" => Highlight::Function,
            "property" => Highlight::Property,
            "constructor" | "type" => Highlight::Type,
            "variable" | "embedded" => Highlight::Variable,
            "comment" => Highlight::Comment,
            "operator" => Highlight::Operator,
            "punctuation" => Highlight::Punctuation,
            "tag" => Highlight::Tag,
            "attribute" => Highlight::Attribute,
            "escape" => Highlight::Escape,
            _ => return None,
        })
    }
}

pub(crate) struct LangConfig {
    language: TsLanguage,
    query: Query,
    /// Highlight per capture index, resolved once.
    captures: Vec<Highlight>,
}

impl LangConfig {
    #[cfg(test)]
    pub(crate) fn capture_names(&self) -> &[&str] {
        self.query.capture_names()
    }

    fn new(language: TsLanguage, source: &str) -> Self {
        Self::build(language, source).expect("invalid highlight query")
    }

    /// For an extension's query, which may not fit its grammar.
    pub(crate) fn build(language: TsLanguage, source: &str) -> Result<Self, String> {
        let query = Query::new(&language, source).map_err(|e| format!("highlights.scm, line {}: {}", e.row + 1, e.message))?;
        let captures = query
            .capture_names()
            .iter()
            .map(|n| Highlight::from_capture(n))
            .collect();
        Ok(Self {
            language,
            query,
            captures,
        })
    }
}

/// Markdown's inline syntax: a second grammar, injected into its blocks.
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
/// convention of tree-sitter's tags queries, or inside an `@item` (Zed's
/// `outline.scm`, whose kind comes from the node).
pub(crate) struct OutlineConfig {
    query: Query,
    /// Per capture index: `Some(kind)` for definitions, `None` otherwise.
    kinds: Vec<Option<&'static str>>,
    name: u32,
    /// Zed's `@context`: the keywords around the name (`pub`, `fn`), which
    /// say what an `@item` is.
    context: Option<u32>,
}

impl OutlineConfig {
    pub(crate) fn build(language: TsLanguage, source: &str) -> Result<Self, String> {
        let query = Query::new(&language, source).map_err(|e| format!("outline query, line {}: {}", e.row + 1, e.message))?;
        let kinds = query
            .capture_names()
            .iter()
            .map(|n| if *n == "item" { Some("symbol") } else { n.strip_prefix("definition.").map(symbol_kind) })
            .collect();
        let name = query.capture_index_for_name("name").ok_or("the outline query has no @name")?;
        let context = query.capture_index_for_name("context");
        Ok(Self { query, kinds, name, context })
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

/// A kind for an outline `@item` from the keywords before its name.
fn context_kind(words: &[String]) -> Option<&'static str> {
    words.iter().find_map(|word| {
        Some(match word.as_str() {
            "fn" | "func" | "function" | "def" | "defp" | "fun" | "sub" | "proc" => "function",
            "defmacro" | "defmacrop" | "macro" | "macro_rules!" => "macro",
            "struct" => "struct",
            "class" | "object" | "record" => "class",
            "enum" => "enum",
            "interface" | "protocol" => "interface",
            "trait" | "typeclass" => "trait",
            "impl" | "instance" | "extension" => "impl",
            "mod" | "module" | "defmodule" | "namespace" | "package" => "module",
            "type" | "typealias" | "newtype" | "data" => "type",
            "const" | "static" => "constant",
            "test" => "test",
            _ => return None,
        })
    })
}

/// A kind for an outline `@item` (any grammar), from its node's name.
fn guess_kind(node: &str) -> &'static str {
    const KINDS: [(&str, &str); 15] = [
        ("method", "method"),
        ("field", "field"),
        ("argument", "field"),
        ("constructor", "constructor"),
        ("function", "function"),
        ("class", "class"),
        ("struct", "struct"),
        ("enum", "enum"),
        ("interface", "interface"),
        ("protocol", "interface"),
        ("trait", "trait"),
        ("impl", "impl"),
        ("module", "module"),
        ("namespace", "module"),
        ("macro", "macro"),
    ];
    KINDS.iter().find(|(word, _)| node.contains(word)).map_or("symbol", |(_, kind)| kind)
}

fn outline_config(language: Language) -> Option<&'static OutlineConfig> {
    match language {
        Language::Extension(_) => language.extension()?.outline_config(),
        _ => match crate::extensions::override_for(language) {
            Some(def) => def.outline_config(),
            None => crate::languages::def(language)?.outline_config(),
        },
    }
}

/// Huge data files (lockfiles) have more keys than anyone browses.
const MAX_SYMBOLS: usize = 5000;

fn config(language: Language) -> Option<&'static LangConfig> {
    match language {
        Language::Extension(_) => language.extension()?.highlight_config(),
        _ => match crate::extensions::override_for(language) {
            Some(def) => def.highlight_config(),
            None => Some(crate::languages::def(language)?.highlight_config()),
        },
    }
}

/// Queries a language brings besides highlights and outline, in Zed's format: `injections.scm`, `brackets.scm`, `indents.scm`,
/// `textobjects.scm` and `runnables.scm`.
pub(crate) struct LanguageQueries {
    injections: Option<InjectionQuery>,
    brackets: Option<Query>,
    indents: Option<Query>,
    textobjects: Option<Query>,
    pub(crate) runnables: Option<Query>,
}

struct InjectionQuery {
    query: Query,
    /// `@injection.content` (or the older `@content`).
    content: Option<u32>,
    /// `@injection.language` (or `@language`): the node naming the language.
    language: Option<u32>,
}

impl LanguageQueries {
    #[cfg(test)]
    pub(crate) fn has_injections(&self) -> bool {
        self.injections.is_some()
    }

    /// Compiles the queries that are there; one that doesn't fit the grammar
    /// is reported and left out.
    pub(crate) fn build(language: &TsLanguage, source: impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> Self {
        let mut compile = |file: &str| {
            let text = source(file)?;
            Query::new(language, &text).map_err(|e| problems.push(format!("{file}, line {}: {}", e.row + 1, e.message))).ok()
        };
        let injections = compile("injections.scm").map(|query| {
            let index = |names: [&str; 2]| names.iter().find_map(|n| query.capture_index_for_name(n));
            let content = index(["injection.content", "content"]);
            let language = index(["injection.language", "language"]);
            InjectionQuery { query, content, language }
        });
        Self {
            injections,
            brackets: compile("brackets.scm"),
            indents: compile("indents.scm"),
            textobjects: compile("textobjects.scm"),
            runnables: compile("runnables.scm"),
        }
    }
}

pub(crate) fn queries(language: Language) -> Option<&'static LanguageQueries> {
    match language {
        Language::Extension(_) => language.extension()?.queries(),
        _ => match crate::extensions::override_for(language) {
            Some(def) => def.queries(),
            None => Some(crate::languages::def(language)?.queries()),
        },
    }
}

/// The highlighting for an embedded language named in an injection query:
/// a built-in language, an extension's, or one known by its file extension.
fn injected_config(name: &str) -> Option<&'static LangConfig> {
    let name = name.trim().to_lowercase();
    let builtin = match name.as_str() {
        "javascript" | "js" | "jsx" => Language::JavaScript,
        "typescript" | "ts" => Language::TypeScript,
        "tsx" => Language::Tsx,
        "html" => Language::Html,
        "css" => Language::Css,
        "rust" | "rs" => Language::Rust,
        "go" => Language::Go,
        "python" | "py" => Language::Python,
        "php" => Language::Php,
        "bash" | "sh" | "shell" | "zsh" => Language::Shell,
        "json" => Language::Json,
        "yaml" | "yml" => Language::Yaml,
        "toml" => Language::Toml,
        "markdown" | "md" => Language::Markdown,
        "markdown_inline" | "markdown-inline" => return Some(&MARKDOWN_INLINE),
        _ => {
            let extension = crate::extensions::languages().into_iter().map(Language::Extension).find(|l| {
                l.extension().is_some_and(|d| d.name.to_lowercase() == name || d.id == name)
            });
            extension.unwrap_or_else(|| Language::from_path(Path::new(&format!("file.{name}"))))
        }
    };
    config(builtin)
}

/// An embedded language's tree: every range of that language, parsed as one.
struct Injection {
    config: &'static LangConfig,
    parser: PooledParser,
    tree: Option<Tree>,
}

/// Parsers kept for reuse. A parser for a WASM grammar needs a `WasmStore`,
/// and making one compiles tree-sitter's WASM runtime again; short-lived
/// parses (the syntax-error lint, Markdown code blocks, diffs) are frequent.
static PARSERS: Mutex<Vec<Parser>> = Mutex::new(Vec::new());
const PARSER_POOL_SIZE: usize = 16;

/// A parser set to a language, from the pool; it goes back when dropped.
struct PooledParser(Option<Parser>);

impl PooledParser {
    fn new(language: &TsLanguage) -> Option<Self> {
        let mut parser = PARSERS.lock().ok().and_then(|mut pool| pool.pop()).unwrap_or_default();
        crate::extensions::prepare_parser(&mut parser, language);
        // On failure it goes back to the pool too.
        let mut pooled = PooledParser(Some(parser));
        pooled.set_language(language).ok()?;
        Some(pooled)
    }
}

impl Deref for PooledParser {
    type Target = Parser;
    fn deref(&self) -> &Parser {
        self.0.as_ref().expect("taken only on drop")
    }
}

impl DerefMut for PooledParser {
    fn deref_mut(&mut self) -> &mut Parser {
        self.0.as_mut().expect("taken only on drop")
    }
}

impl Drop for PooledParser {
    fn drop(&mut self) {
        let Some(mut parser) = self.0.take() else { return };
        parser.reset();
        if parser.set_included_ranges(&[]).is_ok()
            && let Ok(mut pool) = PARSERS.lock()
            && pool.len() < PARSER_POOL_SIZE
        {
            pool.push(parser);
        }
    }
}

/// Runs `f` with a parser for `language` from the pool.
pub fn with_parser<R>(language: &TsLanguage, f: impl FnOnce(&mut Parser) -> R) -> Option<R> {
    let mut parser = PooledParser::new(language)?;
    Some(f(&mut parser))
}

pub struct Syntax {
    language: Language,
    /// None for plain text (or a grammar that failed to load).
    parser: Option<PooledParser>,
    tree: Option<Tree>,
    /// Embedded languages (see the languages' injection queries).
    injections: Vec<Injection>,
}

impl Syntax {
    pub fn new(language: Language, rope: &Rope) -> Self {
        let mut this = Self::unparsed(language);
        this.reparse(rope);
        this
    }

    /// Parsed for at most `budget`; `None` from `tree()` until a parse finishes.
    pub fn new_within(language: Language, rope: &Rope, budget: Duration) -> (Self, bool) {
        let mut this = Self::unparsed(language);
        let done = this.reparse_within(rope, Some(budget));
        (this, done)
    }

    fn unparsed(language: Language) -> Self {
        let parser = config(language).and_then(|config| PooledParser::new(&config.language));
        Self { language, parser, tree: None, injections: Vec::new() }
    }

    pub fn language(&self) -> Language {
        self.language
    }

    /// A full parse to run in the background, from this syntax's trees (as
    /// edited since their last parse) and `rope`.
    pub fn parse_job(&self, rope: &Rope) -> impl FnOnce() -> Syntax + Send + 'static {
        let (language, rope, tree) = (self.language, rope.clone(), self.tree.clone());
        let injections: Vec<(&'static LangConfig, Option<Tree>)> = self.injections.iter().map(|i| (i.config, i.tree.clone())).collect();
        move || {
            let mut syntax = Syntax::unparsed(language);
            syntax.tree = tree;
            syntax.injections = injections
                .into_iter()
                .filter_map(|(config, tree)| Some(Injection { config, parser: PooledParser::new(&config.language)?, tree }))
                .collect();
            syntax.reparse(&rope);
            syntax
        }
    }

    /// The trees of embedded languages, then the file's own.
    fn trees(&self) -> impl Iterator<Item = &Tree> {
        self.injections.iter().filter_map(|i| i.tree.as_ref()).chain(self.tree.as_ref())
    }

    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// Informs the old trees about an edit so the next parse can reuse nodes
    /// (embedded languages' trees too).
    pub fn edit(&mut self, edit: &InputEdit) {
        for tree in self.tree.iter_mut().chain(self.injections.iter_mut().filter_map(|i| i.tree.as_mut())) {
            tree.edit(edit);
        }
    }

    pub fn reparse(&mut self, rope: &Rope) {
        self.reparse_within(rope, None);
    }

    /// Parses again, giving up after `budget`: false then, and the trees are
    /// the old ones, edited, until a later parse finishes (see `parse_job`).
    pub fn reparse_within(&mut self, rope: &Rope, budget: Option<Duration>) -> bool {
        let Some(parser) = self.parser.as_mut() else { return true };
        let started = Instant::now();
        let mut in_time = |_: &tree_sitter::ParseState| match budget {
            Some(budget) if started.elapsed() > budget => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        };
        let options = ParseOptions::new().progress_callback(&mut in_time);
        match parser.parse_with_options(&mut |byte, _| rope_chunk(rope, byte), self.tree.as_ref(), Some(options)) {
            Some(tree) => self.tree = Some(tree),
            None if budget.is_some() => {
                // Halted: start over next time rather than resume on other text.
                parser.reset();
                return false;
            }
            None => self.tree = None,
        }

        let wanted = self.tree.as_ref().map(|tree| injected_ranges(self.language, tree, rope)).unwrap_or_default();
        let mut old = std::mem::take(&mut self.injections);
        for (config, ranges) in wanted {
            let mut injection = match old.iter().position(|i| std::ptr::eq(i.config, config)) {
                Some(ix) => old.swap_remove(ix),
                None => {
                    let Some(parser) = PooledParser::new(&config.language) else { continue };
                    Injection { config, parser, tree: None }
                }
            };
            if injection.parser.set_included_ranges(&ranges).is_err() {
                injection.tree = None;
                self.injections.push(injection);
                continue;
            }
            // Reuses the old tree, edited like the main one; changed ranges are fine.
            let mut in_time = |_: &tree_sitter::ParseState| match budget {
                Some(budget) if started.elapsed() > budget => ControlFlow::Break(()),
                _ => ControlFlow::Continue(()),
            };
            let options = ParseOptions::new().progress_callback(&mut in_time);
            let parsed = injection.parser.parse_with_options(&mut |byte, _| rope_chunk(rope, byte), injection.tree.as_ref(), Some(options));
            let halted = parsed.is_none() && budget.is_some();
            if halted {
                injection.parser.reset();
            } else {
                injection.tree = parsed;
            }
            self.injections.push(injection);
            if halted {
                // The rest keep their edited trees too.
                self.injections.append(&mut old);
                return false;
            }
        }
        true
    }

    /// Whether a line break at `offset` (on `row`) should indent the next
    /// line, by the language's `indents.scm`: an `@indent` node starts on
    /// this row and goes on past the caret. None without such a query.
    pub fn indent_after(&self, rope: &Rope, offset: usize, row: usize) -> Option<bool> {
        let query = queries(self.language)?.indents.as_ref()?;
        let tree = self.tree.as_ref()?;
        let indent = query.capture_index_for_name("indent")?;
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(offset.saturating_sub(1)..offset + 1);
        let mut captures = cursor.captures(query, tree.root_node(), RopeProvider(rope));
        while let Some((m, ix)) = captures.next() {
            let c = m.captures[*ix];
            if c.index == indent && c.node.start_position().row == row && c.node.start_byte() < offset && c.node.end_byte() > offset {
                return Some(true);
            }
        }
        Some(false)
    }

    /// A function, class or comment around `offset` from `textobjects.scm`
    /// (`@function.around`, `@class.inside` …), the smallest one.
    pub fn text_object(&self, rope: &Rope, offset: usize, object: &str, around: bool) -> Option<Range<usize>> {
        let query = queries(self.language)?.textobjects.as_ref()?;
        let tree = self.tree.as_ref()?;
        let find = |kind: &str| -> Option<Range<usize>> {
            let index = query.capture_index_for_name(&format!("{object}.{kind}"))?;
            let mut cursor = QueryCursor::new();
            cursor.set_byte_range(offset..offset + 1);
            let mut best: Option<Range<usize>> = None;
            let mut captures = cursor.captures(query, tree.root_node(), RopeProvider(rope));
            while let Some((m, ix)) = captures.next() {
                let c = m.captures[*ix];
                let r = c.node.byte_range();
                if c.index == index && r.start <= offset && offset < r.end.max(r.start + 1) && best.as_ref().is_none_or(|b| r.len() < b.len()) {
                    best = Some(r);
                }
            }
            best
        };
        if around { find("around") } else { find("inside").or_else(|| find("around")) }
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
        self.trees().filter_map(grow).min_by_key(|r| r.len())
    }

    /// The bracket at `offset` (or just before it) and its partner. Brackets
    /// in strings and comments aren't grammar tokens, so they never match.
    pub fn matching_bracket(&self, rope: &Rope, offset: usize) -> Option<(usize, usize)> {
        self.bracket_pair(rope, offset).map(|(a, b)| (a.start, b.start))
    }

    /// Like `matching_bracket`, with both brackets' extents: `brackets.scm`
    /// pairs can be words (`do` … `end`).
    pub fn bracket_pair(&self, rope: &Rope, offset: usize) -> Option<(Range<usize>, Range<usize>)> {
        // The bracket after the cursor wins, as in JetBrains.
        for at in [Some(offset), offset.checked_sub(1)].into_iter().flatten() {
            if let Some(pair) = self.query_bracket(rope, at) {
                return Some(pair);
            }
            for tree in self.trees() {
                if let Some(partner) = bracket_partner(tree.root_node(), at) {
                    return Some((at..at + 1, partner..partner + 1));
                }
            }
        }
        None
    }

    /// The pair from `brackets.scm` (`@open`, `@close`) one end of which starts at `at`.
    fn query_bracket(&self, rope: &Rope, at: usize) -> Option<(Range<usize>, Range<usize>)> {
        let query = queries(self.language)?.brackets.as_ref()?;
        let tree = self.tree.as_ref()?;
        let (open, close) = (query.capture_index_for_name("open")?, query.capture_index_for_name("close")?);
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(at..at + 1);
        let mut matches = cursor.matches(query, tree.root_node(), RopeProvider(rope));
        while let Some(m) = matches.next() {
            let node = |index| m.captures.iter().find(|c| c.index == index).map(|c| c.node.byte_range());
            let (Some(o), Some(c)) = (node(open), node(close)) else { continue };
            // Quotes pair a string's ends; skip pairs that are the same token.
            if o == c {
                continue;
            }
            if o.start == at {
                return Some((o, c));
            }
            if c.start == at {
                return Some((c, o));
            }
        }
        None
    }

    /// The file's definitions in document order, nested by containment.
    pub fn symbols(&self, rope: &Rope) -> Vec<Symbol> {
        match self.tree.as_ref() {
            Some(tree) => symbols(self.language, tree, rope),
            None => Vec::new(),
        }
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
        for injection in &self.injections {
            if let Some(tree) = injection.tree.as_ref() {
                apply_query(injection.config, tree.root_node(), rope, range.clone(), &mut map);
            }
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
    // A cursor steps to the next sibling in constant time; `Node`'s sibling
    // methods start over from the parent each time, which is quadratic in a
    // big array or object. Backwards, the nearest partner before the bracket
    // is found in one pass from the first sibling.
    let parent = node.parent()?;
    let mut cursor = parent.walk();
    if forward {
        cursor.goto_first_child_for_byte(at)?;
        if cursor.node() != node {
            return None;
        }
        while cursor.goto_next_sibling() {
            let sibling = cursor.node();
            if !sibling.is_named() && sibling.kind() == partner {
                return Some(sibling.start_byte());
            }
        }
        None
    } else {
        cursor.goto_first_child();
        let mut found = None;
        loop {
            let sibling = cursor.node();
            if sibling == node {
                return found;
            }
            if !sibling.is_named() && sibling.kind() == partner {
                found = Some(sibling.start_byte());
            }
            if !cursor.goto_next_sibling() {
                return None;
            }
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
        let capture = m.captures[*ix];
        let h = config.captures[capture.index as usize];
        let node = capture.node;
        let s = node.start_byte().max(range.start) - range.start;
        let e = node.end_byte().min(range.end).saturating_sub(range.start);
        if s < e {
            map[s..e].fill(h);
        }
    }
}

/// Embedded ranges per language: what the language's `injections.scm` finds (by `injection.language`, captured or `#set!`).
fn injected_ranges(language: Language, tree: &Tree, rope: &Rope) -> Vec<(&'static LangConfig, Vec<tree_sitter::Range>)> {
    let Some(injections) = queries(language).and_then(|q| q.injections.as_ref()) else { return Vec::new() };
    let own = config(language);
    let mut out: Vec<(&'static LangConfig, Vec<tree_sitter::Range>)> = Vec::new();
    let mut found: HashMap<String, Option<&'static LangConfig>> = HashMap::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&injections.query, tree.root_node(), RopeProvider(rope));
    while let Some(m) = matches.next() {
        let named = m.captures.iter().find(|c| Some(c.index) == injections.language).map(|c| rope.byte_slice(c.node.byte_range()).to_string());
        let set = || {
            injections.query.property_settings(m.pattern_index).iter().find_map(|p| {
                matches!(p.key.as_ref(), "injection.language" | "language").then(|| p.value.as_deref().map(str::to_string)).flatten()
            })
        };
        // Looked up once per name: comments (JSDoc) and regexes match often,
        // and are usually languages Kin doesn't have.
        let Some(config) = named.or_else(set).and_then(|name| *found.entry(name).or_insert_with_key(|name| injected_config(name))) else {
            continue;
        };
        if own.is_some_and(|own| std::ptr::eq(own, config)) {
            continue;
        }
        let ranges = m.captures.iter().filter(|c| Some(c.index) == injections.content).map(|c| c.node.range());
        match out.iter_mut().find(|(c, _)| std::ptr::eq(*c, config)) {
            Some((_, list)) => list.extend(ranges),
            None => out.push((config, ranges.collect())),
        }
    }
    // A parser takes ranges in order and without overlaps.
    for (_, ranges) in &mut out {
        ranges.sort_by_key(|r| r.start_byte);
        let mut end = 0;
        ranges.retain(|r| {
            let keep = r.start_byte >= end && r.end_byte > r.start_byte;
            if keep {
                end = r.end_byte;
            }
            keep
        });
    }
    out.retain(|(_, ranges)| !ranges.is_empty());
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

/// File Structure's definitions in `tree` (see `Syntax::symbols`); takes no
/// `Syntax`, so it can run in the background on a tree's clone.
pub fn symbols(language: Language, tree: &Tree, rope: &Rope) -> Vec<Symbol> {
    let Some(config) = outline_config(language) else {
        return Vec::new();
    };
    // (definition range, kind, name range)
    let mut found: Vec<(Range<usize>, &'static str, Range<usize>)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&config.query, tree.root_node(), RopeProvider(rope));
    while let Some(m) = matches.next() {
        let mut def = None;
        let mut name = None;
        let context: Vec<String> = m
            .captures
            .iter()
            .filter(|c| Some(c.index) == config.context && c.node.byte_range().len() < 40)
            .map(|c| rope.byte_slice(c.node.byte_range()).to_string())
            .collect();
        for c in m.captures {
            if c.index == config.name {
                name = Some(c.node.byte_range());
            } else if let Some(kind) = config.kinds[c.index as usize] {
                let kind = match kind {
                    "symbol" => context_kind(&context).unwrap_or_else(|| guess_kind(c.node.kind())),
                    kind => node_kind_label(c.node.kind()).unwrap_or(kind),
                };
                def = Some((c.node.byte_range(), kind));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_trees_are_edited_and_reused() {
        let before = "<p>x</p><script>let a = 1;\nlet b = 2;</script>";
        let after = "<p>x</p><script>let a = 10;\nlet b = 2;</script>";
        let mut syntax = Syntax::new(Language::Html, &Rope::from_str(before));
        let at = before.find("1;").unwrap() + 1;
        let rope = Rope::from_str(after);
        syntax.edit(&InputEdit {
            start_byte: at,
            old_end_byte: at,
            new_end_byte: at + 1,
            start_position: tree_sitter::Point::new(0, at),
            old_end_position: tree_sitter::Point::new(0, at),
            new_end_position: tree_sitter::Point::new(0, at + 1),
        });
        syntax.reparse(&rope);
        let fresh = Syntax::new(Language::Html, &rope);
        let sexp = |s: &Syntax| s.injections.iter().map(|i| i.tree.as_ref().unwrap().root_node().to_sexp()).collect::<Vec<_>>();
        assert_eq!(sexp(&syntax), sexp(&fresh));
        let number = |s: &Syntax| s.injections[0].tree.as_ref().unwrap().root_node().descendant_for_byte_range(at, at).map(|n| n.byte_range());
        assert_eq!(number(&syntax), number(&fresh));
    }

    #[test]
    fn pooled_parsers_come_back_clean() {
        // HTML's script injection parses only included ranges; the parser
        // must read whole files again when the pool hands it out next.
        for _ in 0..3 {
            let html = Rope::from_str("<p>x</p><script>let a = 1;</script>");
            let syntax = Syntax::new(Language::Html, &html);
            assert_eq!(syntax.injections.len(), 1);
            drop(syntax);
            let js = Rope::from_str("let b = 2;\nlet c = 3;\n");
            let syntax = Syntax::new(Language::JavaScript, &js);
            let root = syntax.tree().unwrap().root_node();
            assert!(!root.has_error());
            assert_eq!(root.named_child_count(), 2, "{}", root.to_sexp());
        }
        let tree = with_parser(&config(Language::Json).unwrap().language, |p| p.parse("[1, 2]", None)).flatten();
        assert_eq!(tree.unwrap().root_node().kind(), "document");
    }

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
        spans.iter().any(|(t, x)| t == text && x.base() == h)
    }

    fn outline(path: &str, src: &str) -> Vec<(String, &'static str, usize)> {
        let language = Language::from_path(Path::new(path));
        let rope = Rope::from_str(src);
        Syntax::new(language, &rope).symbols(&rope).into_iter().map(|s| (s.name, s.kind, s.depth)).collect()
    }

    #[test]
    fn grammars_injections_highlight_embedded_code() {
        let html = highlights_of("a.html", "<style>body { color: red }</style>\n");
        assert!(has(&html, "color", Highlight::Property), "{html:?}");
        let md = highlights_of("a.md", "# Title\n\n```rust\nfn main() {}\n```\n");
        assert!(has(&md, "fn", Highlight::Keyword), "{md:?}");
        let php = highlights_of("a.php", "<p class=\"x\">hi</p>\n<?php echo 1; ?>\n");
        assert!(has(&php, "p", Highlight::Tag) && has(&php, "echo", Highlight::Keyword), "{php:?}");
        let js = highlights_of("a.js", "const s = html`<b>bold</b>`;\n");
        assert!(has(&js, "b", Highlight::Tag), "{js:?}");
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
    fn matches_brackets_of_big_arrays_quickly() {
        // 40 000 elements: stepping sibling by sibling from the parent each
        // time took seconds.
        let src = format!("[{}[1]]", "1,".repeat(40_000));
        let rope = Rope::from_str(&src);
        let syntax = Syntax::new(Language::Json, &rope);
        let (last, inner) = (src.len() - 1, src.len() - 4);
        let started = std::time::Instant::now();
        assert_eq!(syntax.matching_bracket(&rope, 0), Some((0, last)));
        assert_eq!(syntax.matching_bracket(&rope, last), Some((last, 0)));
        assert_eq!(syntax.matching_bracket(&rope, inner), Some((inner, inner + 2)));
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
    }

    #[test]
    fn matches_brackets_outside_strings() {
        let src = "fn f(a: [u8; 2]) { let s = \"(\"; }\n";
        let rope = Rope::from_str(src);
        let syntax = Syntax::new(Language::Rust, &rope);
        let at = |c: char, nth: usize| src.match_indices(c).nth(nth).unwrap().0;
        assert_eq!(syntax.matching_bracket(&rope, at('(', 0)), Some((at('(', 0), at(')', 0))));
        // After a closing bracket, the cursor's left neighbor matches.
        assert_eq!(syntax.matching_bracket(&rope, at(')', 0) + 1), Some((at(')', 0), at('(', 0))));
        assert_eq!(syntax.matching_bracket(&rope, at('[', 0)), Some((at('[', 0), at(']', 0))));
        assert_eq!(syntax.matching_bracket(&rope, at('{', 0)), Some((at('{', 0), at('}', 0))));
        // The "(" inside the string literal isn't a bracket.
        assert_eq!(syntax.matching_bracket(&rope, at('(', 1)), None);
        assert_eq!(syntax.matching_bracket(&rope, at('l', 0)), None);
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


