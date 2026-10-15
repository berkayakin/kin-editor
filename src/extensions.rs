//! Extensions: folders in `<config>/extensions/` that add languages, themes
//! and snippets without rebuilding Kin. The layout is Zed's, so Zed's
//! language extensions work as they are:
//!
//! ```text
//! my-extension/
//!   extension.toml               id, name, version, description, authors, repository;
//!                                [grammars.<name>]: repository + rev (fetched with git),
//!                                or sources in grammars/<name>/;
//!                                [language_servers.<id>]: languages, command, args
//!   languages/<lang>/config.toml name, grammar, path_suffixes, line_comments,
//!                                block_comment, tab_size, hard_tabs, formatter
//!   languages/<lang>/highlights.scm, outline.scm (else the grammar's queries/)
//!   themes/*.json                Kin themes
//!   snippets/*.json              VS Code style snippets, `<language id>.json`
//! ```
//!
//! What Zed extensions compute in WebAssembly (where a language server is)
//! is plain configuration here: `command` and `args`, or `language_servers`
//! in settings. A grammar is tree-sitter's generated C (`src/parser.c`, maybe
//! a scanner), compiled once with the system compiler into a library beside
//! it and loaded at run time.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{LazyLock, Mutex, OnceLock, RwLock},
};

use gpui::{App, Global};
use toml::{Table, Value};
use tree_sitter::Language as TsLanguage;

use crate::{
    lsp::servers::{self, ServerSpec},
    syntax::{LangConfig, LanguageQueries, OutlineConfig},
    theme::config_dir,
};

/// A language's comment tokens from its `config.toml`, spaced for
/// inserting: `line_comments` (the first) and `block_comment`, as
/// `["/*", "*/"]` or Zed's newer `{ start = "/*", end = "*/" }`.
pub(crate) fn comment_tokens(config: &Table) -> (Option<String>, Option<(String, String)>) {
    let line = match config.get("line_comments").or(config.get("line_comment")) {
        Some(Value::Array(items)) => items.first().and_then(Value::as_str).map(str::to_string),
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    let block = match config.get("block_comment") {
        Some(Value::Array(items)) => match (items.first().and_then(Value::as_str), items.get(1).and_then(Value::as_str)) {
            (Some(start), Some(end)) => Some((start.to_string(), end.to_string())),
            _ => None,
        },
        Some(Value::Table(t)) => match (t.get("start").and_then(Value::as_str), t.get("end").and_then(Value::as_str)) {
            (Some(start), Some(end)) => Some((start.to_string(), end.to_string())),
            _ => None,
        },
        _ => None,
    };
    (line.map(|c| format!("{} ", c.trim_end())), block.map(|(start, end)| (format!("{} ", start.trim_end()), format!(" {}", end.trim_start()))))
}

pub fn extensions_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("extensions"))
}

/// Language servers that Zed extensions name, with the command their code
/// would run (found on PATH here, never downloaded).
const KNOWN_SERVERS: &[(&str, &str, &[&str])] = &[
    ("astro-language-server", "astro-ls", &["--stdio"]),
    ("basedpyright", "basedpyright-langserver", &["--stdio"]),
    ("biome", "biome", &["lsp-proxy"]),
    ("clangd", "clangd", &[]),
    ("clojure-lsp", "clojure-lsp", &[]),
    ("csharp", "csharp-ls", &[]),
    ("dart", "dart", &["language-server", "--protocol=lsp"]),
    ("deno", "deno", &["lsp"]),
    ("docker-language-server", "docker-language-server", &["start", "--stdio"]),
    ("dockerfile-language-server", "docker-langserver", &["--stdio"]),
    ("elixir-ls", "elixir-ls", &[]),
    ("elm-language-server", "elm-language-server", &[]),
    ("erlang-ls", "erlang_ls", &[]),
    ("expert", "expert", &["--stdio"]),
    ("gleam", "gleam", &["lsp"]),
    ("hls", "haskell-language-server-wrapper", &["--lsp"]),
    ("jdtls", "jdtls", &[]),
    ("kotlin-language-server", "kotlin-language-server", &[]),
    ("kotlin-lsp", "kotlin-lsp", &["--stdio"]),
    ("lexical", "lexical", &[]),
    ("lua-language-server", "lua-language-server", &[]),
    ("metals", "metals", &[]),
    ("next-ls", "nextls", &["--stdio"]),
    ("nil", "nil", &[]),
    ("nixd", "nixd", &[]),
    ("ocaml-lsp", "ocamllsp", &[]),
    ("omnisharp", "OmniSharp", &["-lsp"]),
    ("ruby-lsp", "ruby-lsp", &[]),
    ("ruff", "ruff", &["server"]),
    ("solargraph", "solargraph", &["stdio"]),
    ("sourcekit-lsp", "sourcekit-lsp", &[]),
    ("svelte-language-server", "svelteserver", &["--stdio"]),
    ("tailwindcss-language-server", "tailwindcss-language-server", &["--stdio"]),
    ("taplo", "taplo", &["lsp", "stdio"]),
    ("terraform-ls", "terraform-ls", &["serve"]),
    ("vue-language-server", "vue-language-server", &["--stdio"]),
    ("zls", "zls", &[]),
];

/// An installed extension, as the Extensions page shows it.
#[derive(Clone, Debug)]
pub struct ExtensionInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub authors: Vec<String>,
    pub path: PathBuf,
    pub languages: Vec<String>,
    pub themes: usize,
    pub snippets: bool,
    /// Language servers, by name.
    pub servers: Vec<String>,
    /// Grammars still being fetched or compiled.
    pub building: Vec<String>,
    /// What went wrong: a manifest that doesn't parse, a grammar that didn't build.
    pub problems: Vec<String>,
    /// How to install its servers, where it says.
    installers: Vec<&'static crate::lsp::install::Installer>,
    /// Debug adapters it declares (`[debug_adapters.<id>]`).
    adapters: Vec<&'static crate::debugger::launch::ExtensionAdapter>,
}

/// A language an extension adds.
pub struct LanguageDef {
    pub name: &'static str,
    /// For snippet files and language servers: "gleam".
    pub id: &'static str,
    /// File extensions and whole file names ("Dockerfile").
    suffixes: Vec<String>,
    pub line_comment: Option<&'static str>,
    pub block_comment: Option<(&'static str, &'static str)>,
    pub tab_size: Option<usize>,
    pub hard_tabs: Option<bool>,
    /// A command reading the file on stdin and writing it formatted.
    pub formatter: Option<Vec<String>>,
    pub server: Option<&'static ServerSpec>,
    extension: String,
    grammar: Option<Grammar>,
    highlights: String,
    outline: Option<String>,
    /// The language folder, for its other queries (`injections.scm` …).
    dir: PathBuf,
    /// `tasks.json`: commands for `runnables.scm` tags (and untagged ones).
    pub tasks: Vec<Task>,
    /// Set once the grammar's library is loaded (None: it failed to).
    loaded: OnceLock<Option<Loaded>>,
}

struct Loaded {
    language: TsLanguage,
    highlights: &'static LangConfig,
    outline: Option<&'static OutlineConfig>,
    queries: &'static LanguageQueries,
}

/// A task from a language's `tasks.json`, as in Zed.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct Task {
    pub label: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// The `runnables.scm` tags it runs for.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Where a grammar's sources and compiled library are.
#[derive(Clone, Debug)]
pub struct Grammar {
    /// Its name, which also names the C function: `tree_sitter_<name>`.
    name: String,
    /// The folder with `parser.c`.
    src: PathBuf,
    library: PathBuf,
    /// A prebuilt WebAssembly grammar (`grammars/<name>.wasm`, as in the
    /// extensions Zed installs): used as is, so no C compiler is needed.
    wasm: PathBuf,
    /// Fetched into `checkout` at `rev`, when the extension doesn't bundle it.
    repository: Option<String>,
    rev: Option<String>,
    checkout: PathBuf,
}

impl LanguageDef {
    pub fn grammar(&self) -> Option<TsLanguage> {
        self.load().map(|l| l.language.clone())
    }

    pub(crate) fn highlight_config(&self) -> Option<&'static LangConfig> {
        self.load().map(|l| l.highlights)
    }

    pub(crate) fn outline_config(&self) -> Option<&'static OutlineConfig> {
        self.load().and_then(|l| l.outline)
    }

    pub(crate) fn queries(&self) -> Option<&'static LanguageQueries> {
        self.load().map(|l| l.queries)
    }

    /// Loads the grammar the first time it's needed and built.
    fn load(&self) -> Option<&Loaded> {
        if let Some(loaded) = self.loaded.get() {
            return loaded.as_ref();
        }
        let grammar = self.grammar.as_ref()?;
        // Not built yet: try again when it is.
        if !grammar.wasm.is_file() && (!grammar.library.is_file() || BUILDING.lock().unwrap().contains(&grammar.library)) {
            return None;
        }
        let loaded = match self.load_now(grammar) {
            Ok(loaded) => Some(loaded),
            Err(err) => {
                add_problem(&self.extension, format!("{}: {err}", self.name));
                None
            }
        };
        self.loaded.get_or_init(|| loaded).as_ref()
    }

    fn load_now(&self, grammar: &Grammar) -> Result<Loaded, String> {
        let language = if grammar.wasm.is_file() {
            load_wasm(&grammar.wasm, &grammar.name)?
        } else {
            load_library(&grammar.library, &grammar.name)?
        };
        // A query that doesn't fit the grammar leaves the language unhighlighted
        // (it still parses, for outlines and syntax errors).
        let highlights = LangConfig::build(language.clone(), &self.highlights).or_else(|err| {
            add_problem(&self.extension, format!("{}: {err}", self.name));
            LangConfig::build(language.clone(), "")
        })?;
        let outline = self.outline.as_deref().and_then(|source| match OutlineConfig::build(language.clone(), source) {
            Ok(config) => Some(&*Box::leak(Box::new(config))),
            Err(err) => {
                add_problem(&self.extension, format!("{}: {err}", self.name));
                None
            }
        });
        let mut problems = Vec::new();
        let queries = LanguageQueries::build(&language, |file| std::fs::read_to_string(self.dir.join(file)).ok(), &mut problems);
        for problem in problems {
            add_problem(&self.extension, format!("{}: {problem}", self.name));
        }
        Ok(Loaded { language, highlights: Box::leak(Box::new(highlights)), outline, queries: Box::leak(Box::new(queries)) })
    }

    fn matches(&self, path: &Path) -> bool {
        let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
        self.suffixes.iter().any(|s| name == s.as_str() || name.strip_suffix(s.as_str()).is_some_and(|rest| rest.ends_with('.')))
    }
}

#[derive(Default)]
struct Registry {
    extensions: Vec<ExtensionInfo>,
    /// By `Language::Extension` index. Names keep their index across reloads,
    /// so open files keep their language; a removed one is None.
    languages: Vec<Option<&'static LanguageDef>>,
    /// Servers by the language name they serve (built-in languages too).
    /// Per language, the servers extensions bring for it.
    servers: HashMap<String, Vec<&'static ServerSpec>>,
    theme_dirs: Vec<PathBuf>,
    snippet_dirs: Vec<PathBuf>,
}

static REGISTRY: LazyLock<RwLock<Registry>> = LazyLock::new(Default::default);
/// Libraries being built, which mustn't be loaded half-written or built twice.
static BUILDING: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);

pub fn language(ix: u16) -> Option<&'static LanguageDef> {
    REGISTRY.read().unwrap().languages.get(ix as usize).copied().flatten()
}

pub fn languages() -> Vec<u16> {
    let registry = REGISTRY.read().unwrap();
    (0..registry.languages.len()).filter(|&i| registry.languages[i].is_some()).map(|i| i as u16).collect()
}

pub fn language_for_path(path: &Path) -> Option<u16> {
    let registry = REGISTRY.read().unwrap();
    registry.languages.iter().position(|l| l.is_some_and(|l| l.matches(path))).map(|i| i as u16)
}

/// A language that names this exact file (`docker-compose.yml`), which is
/// more specific than a built-in language's extension.
pub fn language_for_file_name(path: &Path) -> Option<u16> {
    let name = path.file_name()?.to_string_lossy();
    let registry = REGISTRY.read().unwrap();
    registry.languages.iter().position(|l| l.is_some_and(|l| l.suffixes.iter().any(|s| *s == name))).map(|i| i as u16)
}

/// An extension's server for a language (built-in or not), by its name.
/// The servers extensions bring for `language` (one per extension: its
/// alternatives for that language, the first installed one runs).
pub fn servers_for(language: &str) -> Vec<&'static ServerSpec> {
    REGISTRY.read().unwrap().servers.get(language).cloned().unwrap_or_default()
}

pub fn installed() -> Vec<ExtensionInfo> {
    REGISTRY.read().unwrap().extensions.clone()
}

/// An icon theme an extension brings (`icon_themes/*.json`, Zed's format).
pub struct IconThemeInfo {
    pub name: String,
    file: PathBuf,
    index: usize,
}

pub fn icon_themes() -> Vec<IconThemeInfo> {
    let mut out = Vec::new();
    for extension in installed() {
        let Ok(entries) = std::fs::read_dir(extension.path.join("icon_themes")) else { continue };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        files.sort();
        for file in files {
            let Some(value) = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&crate::settings::strip_jsonc(&t)).ok()) else { continue };
            for (index, theme) in value["themes"].as_array().into_iter().flatten().enumerate() {
                if let Some(name) = theme["name"].as_str() {
                    out.push(IconThemeInfo { name: name.to_string(), file: file.clone(), index });
                }
            }
        }
    }
    out
}

/// Applies the `icon_theme` setting: an extension's icon theme by name, or
/// the built-in icons.
pub fn apply_icon_theme(cx: &App) {
    let name = crate::settings::Settings::get(cx).icon_theme.clone();
    let themes = icon_themes();
    let chosen = name.and_then(|n| themes.into_iter().find(|t| t.name == n));
    crate::file_icons::select(chosen.as_ref().map(|t| (t.file.as_path(), t.index)));
}

pub fn theme_dirs() -> Vec<PathBuf> {
    REGISTRY.read().unwrap().theme_dirs.clone()
}

pub fn snippet_dirs() -> Vec<PathBuf> {
    REGISTRY.read().unwrap().snippet_dirs.clone()
}

fn add_problem(extension: &str, problem: String) {
    if let Some(info) = REGISTRY.write().unwrap().extensions.iter_mut().find(|e| e.id == extension) {
        info.problems.push(problem);
    }
}

// ---- Loading -----------------------------------------------------------------------

/// Bumped when extensions change (a reload, a grammar built), so workspaces
/// refresh the languages of open files; `message` is news for a toast.
#[derive(Default)]
pub struct ExtensionsChanged {
    pub generation: u64,
    pub message: Option<String>,
}

impl Global for ExtensionsChanged {}

/// Reads every extension, then fetches and compiles the grammars that need
/// it in the background.
pub fn reload(cx: &mut App) {
    let jobs = load();
    apply_icon_theme(cx);
    changed(None, cx);
    // At startup the themes load after this.
    if cx.has_global::<crate::theme::Theme>() {
        crate::theme::reload_themes(cx);
    }
    if jobs.is_empty() {
        return;
    }
    cx.spawn(async move |cx| {
        for (extension, grammar) in jobs {
            let name = grammar.name.clone();
            let built = cx.background_executor().spawn(async move { build(&grammar) }).await;
            let message = match built {
                Ok(()) => format!("{name}: grammar built"),
                Err(err) => {
                    add_problem(&extension, format!("grammar {name}: {err}"));
                    format!("{name}: the grammar didn't build ({err})")
                }
            };
            set_building(&extension, &name, false);
            cx.update(|cx| changed(Some(message), cx)).ok();
        }
    })
    .detach();
}

fn changed(message: Option<String>, cx: &mut App) {
    let generation = cx.try_global::<ExtensionsChanged>().map_or(0, |c| c.generation) + 1;
    cx.set_global(ExtensionsChanged { generation, message });
}

fn set_building(extension: &str, grammar: &str, building: bool) {
    if let Some(info) = REGISTRY.write().unwrap().extensions.iter_mut().find(|e| e.id == extension) {
        info.building.retain(|g| g != grammar);
        if building {
            info.building.push(grammar.to_string());
        }
    }
}

/// Replaces the registry with what's on disk now. Returns the grammars to
/// build (with their extension's id).
fn load() -> Vec<(String, Grammar)> {
    let mut dirs: Vec<PathBuf> = extensions_dir()
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|entries| entries.flatten().map(|e| e.path()).filter(|p| p.join("extension.toml").is_file()).collect())
        .unwrap_or_default();
    dirs.sort();

    let mut extensions = Vec::new();
    let mut defs: Vec<LanguageDef> = Vec::new();
    let mut servers: HashMap<String, Vec<&'static ServerSpec>> = HashMap::new();
    let mut jobs = Vec::new();
    for dir in dirs {
        let (info, languages, ext_servers, grammars) = read_extension(&dir);
        for grammar in grammars.values() {
            if needs_build(grammar) && !BUILDING.lock().unwrap().contains(&grammar.library) {
                jobs.push((info.id.clone(), grammar.clone()));
            }
        }
        for (language, spec) in ext_servers {
            servers.entry(language).or_default().push(spec);
        }
        defs.extend(languages);
        extensions.push(info);
    }
    for (id, grammar) in &jobs {
        if let Some(info) = extensions.iter_mut().find(|e| &e.id == id) {
            info.building.push(grammar.name.clone());
        }
    }

    let mut registry = REGISTRY.write().unwrap();
    let mut slots: Vec<Option<&'static LanguageDef>> = vec![None; registry.languages.len()];
    for mut def in defs {
        def.server = servers.get(def.name).and_then(|s| s.first().copied()).or(def.server);
        let def: &'static LanguageDef = Box::leak(Box::new(def));
        // A language keeps its index; an old one with the same name is replaced.
        let old = registry.languages.iter().position(|l| l.is_some_and(|l| l.name == def.name));
        match old {
            Some(ix) if slots[ix].is_none() => slots[ix] = Some(def),
            _ => slots.push(Some(def)),
        }
    }
    registry.theme_dirs = extensions.iter().map(|e| e.path.join("themes")).filter(|d| d.is_dir()).collect();
    registry.snippet_dirs = extensions.iter().map(|e| e.path.join("snippets")).filter(|d| d.is_dir()).collect();
    registry.languages = slots;
    registry.servers = servers;
    crate::lsp::install::set_extension_installers(extensions.iter().flat_map(|e| e.installers.iter().copied()).collect());
    crate::debugger::launch::set_extension_adapters(extensions.iter().flat_map(|e| e.adapters.iter().copied()).collect());
    registry.extensions = extensions;
    jobs
}

type Read = (ExtensionInfo, Vec<LanguageDef>, HashMap<String, &'static ServerSpec>, HashMap<String, Grammar>);

/// One extension's manifest, languages, servers and grammars.
fn read_extension(dir: &Path) -> Read {
    let folder = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut info = ExtensionInfo {
        id: folder.clone(),
        name: folder,
        version: String::new(),
        description: String::new(),
        authors: Vec::new(),
        path: dir.to_path_buf(),
        languages: Vec::new(),
        themes: 0,
        snippets: dir.join("snippets").is_dir(),
        servers: Vec::new(),
        building: Vec::new(),
        problems: Vec::new(),
        installers: Vec::new(),
        adapters: Vec::new(),
    };
    let manifest = match read_toml(&dir.join("extension.toml")) {
        Ok(manifest) => manifest,
        Err(err) => {
            info.problems.push(err);
            return (info, Vec::new(), HashMap::new(), HashMap::new());
        }
    };
    let text = |key: &str| manifest.get(key).and_then(Value::as_str).map(str::to_string);
    info.id = text("id").unwrap_or(info.id);
    info.name = text("name").unwrap_or(info.name);
    info.version = text("version").unwrap_or_default();
    info.description = text("description").unwrap_or_default();
    info.authors = strings(manifest.get("authors"));
    info.themes = std::fs::read_dir(dir.join("themes"))
        .map(|entries| entries.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).count())
        .unwrap_or(0);

    let mut grammars = HashMap::new();
    for (name, source) in manifest.get("grammars").and_then(Value::as_table).into_iter().flatten() {
        let field = |key: &str| source.get(key).and_then(Value::as_str).map(str::to_string);
        let checkout = dir.join("grammars").join(name);
        let src = match field("path") {
            Some(path) => checkout.join(path).join("src"),
            None => checkout.join("src"),
        };
        let library = dir.join("grammars").join(format!("{name}.{}", std::env::consts::DLL_EXTENSION));
        let grammar = Grammar {
            name: name.clone(),
            src,
            library,
            wasm: dir.join("grammars").join(format!("{name}.wasm")),
            repository: field("repository"),
            rev: field("rev").or_else(|| field("commit")),
            checkout,
        };
        grammars.insert(name.clone(), grammar);
    }

    // Debug adapters: Zed extensions start theirs in code; here it's the
    // `command` (with `{port}` in `args` when `tcp`).
    for (id, adapter) in manifest.get("debug_adapters").and_then(Value::as_table).into_iter().flatten() {
        let Some(command) = adapter.get("command").and_then(Value::as_str) else {
            info.problems.push(format!("debug adapter {id}: it needs a command"));
            continue;
        };
        info.adapters.push(Box::leak(Box::new(crate::debugger::launch::ExtensionAdapter {
            id: leak(id.clone()),
            aliases: strings(adapter.get("aliases")).into_iter().map(leak).collect(),
            command: command.to_string(),
            args: strings(adapter.get("args")),
            tcp: adapter.get("tcp").and_then(Value::as_bool).unwrap_or(false),
        })));
    }

    // Per language, every server the extension lists for it: one spec with
    // them all as candidates, so the first that's installed runs.
    type Candidate = (&'static str, &'static [&'static str]);
    type Json = Option<&'static serde_json::Value>;
    // (server ids, candidates, install hint, root markers, initialization options, settings)
    #[allow(clippy::type_complexity)]
    let mut per_language: HashMap<String, (Vec<String>, Vec<Candidate>, Option<&'static str>, Vec<&'static str>, Json, Json)> =
        HashMap::new();
    for (id, server) in manifest.get("language_servers").and_then(Value::as_table).into_iter().flatten() {
        let name = server.get("name").and_then(Value::as_str).unwrap_or(id).to_string();
        let mut languages = strings(server.get("languages"));
        languages.extend(server.get("language").and_then(Value::as_str).map(str::to_string));
        // Zed extensions find their server in code; here it's the `command`,
        // else a well-known server's, else the server's id as a program.
        let (command, args): (String, Vec<String>) = match server.get("command").and_then(Value::as_str) {
            Some(command) => (command.to_string(), strings(server.get("args"))),
            None => match KNOWN_SERVERS.iter().find(|(known, _, _)| known == id) {
                Some((_, command, args)) => (command.to_string(), args.iter().map(|a| a.to_string()).collect()),
                None => (id.clone(), Vec::new()),
            },
        };
        info.servers.push(format!("{name} ({command})"));
        if let Some(table) = server.get("installer").and_then(Value::as_table) {
            match crate::lsp::install::Installer::from_manifest(leak(command.clone()), table) {
                Ok(installer) => info.installers.push(Box::leak(Box::new(installer))),
                Err(err) => info.problems.push(format!("language server {id}: {err}")),
            }
        }
        let candidate: Candidate = (leak(command), Box::leak(args.into_iter().map(leak).collect::<Vec<_>>().into_boxed_slice()));
        let install = server.get("install").and_then(Value::as_str).map(leak);
        let markers: Vec<&'static str> = strings(server.get("root_markers")).into_iter().map(leak).collect();
        // Kin's own keys: what Zed extensions' code would pass the server.
        let json = |key: &str| -> Json {
            let value = serde_json::to_value(server.get(key)?).ok()?;
            Some(Box::leak(Box::new(value)))
        };
        let (initialization_options, settings) = (json("initialization_options"), json("settings"));
        for language in languages {
            let entry = per_language.entry(language).or_default();
            entry.0.push(id.clone());
            entry.1.push(candidate);
            entry.2 = entry.2.or(install);
            entry.3.extend(markers.iter().copied());
            entry.4 = entry.4.or(initialization_options);
            entry.5 = entry.5.or(settings);
        }
    }
    let mut servers = HashMap::new();
    for (language, (ids, candidates, install, markers, initialization_options, settings)) in per_language {
        let names: Vec<&str> = candidates.iter().map(|(command, _)| *command).collect();
        let install = install.unwrap_or_else(|| {
            leak(format!("Install {}, or set a command in language_servers (for the {} extension)", names.join(" or "), info.name))
        });
        let spec: &'static ServerSpec = Box::leak(Box::new(ServerSpec {
            // Languages listing the same servers share one process per root
            // (Tailwind for HTML, CSS and TSX).
            id: leak(format!("{}:{}", info.id, ids.join("+"))),
            candidates: Box::leak(candidates.into_boxed_slice()),
            install,
            root_markers: Box::leak(markers.into_boxed_slice()),
            initialization_options,
            settings,
        }));
        servers.insert(language, spec);
    }

    let mut languages = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir.join("languages"))
        .map(|entries| entries.flatten().map(|e| e.path()).filter(|p| p.join("config.toml").is_file()).collect())
        .unwrap_or_default();
    entries.sort();
    for language_dir in entries {
        match read_language(&language_dir, &grammars, &info.id) {
            Ok(def) => {
                info.languages.push(def.name.to_string());
                languages.push(def);
            }
            Err(err) => info.problems.push(format!("{}: {err}", language_dir.display())),
        }
    }
    (info, languages, servers, grammars)
}

fn read_language(dir: &Path, grammars: &HashMap<String, Grammar>, extension: &str) -> Result<LanguageDef, String> {
    let config = read_toml(&dir.join("config.toml"))?;
    let text = |key: &str| config.get(key).and_then(Value::as_str).map(str::to_string);
    let name = text("name").ok_or("config.toml has no name")?;
    let grammar = text("grammar").map(|g| grammars.get(&g).cloned().ok_or(format!("grammar {g} isn't in extension.toml"))).transpose()?;
    // The language's own queries, else the ones its grammar ships.
    let query = |file: &str, fallback: &str| {
        std::fs::read_to_string(dir.join(file))
            .ok()
            .or_else(|| grammar.as_ref().and_then(|g| std::fs::read_to_string(g.src.parent()?.join("queries").join(fallback)).ok()))
    };
    let (line_comment, block_comment) = comment_tokens(&config);
    let id = text("language_id").unwrap_or_else(|| name.to_lowercase().replace(' ', "-"));
    let formatter = config.get("formatter").map(|v| strings(Some(v))).filter(|c| !c.is_empty());
    Ok(LanguageDef {
        name: leak(name),
        id: leak(id),
        suffixes: strings(config.get("path_suffixes")),
        line_comment: line_comment.map(leak),
        block_comment: block_comment.map(|(start, end)| (leak(start), leak(end))),
        tab_size: config.get("tab_size").and_then(Value::as_integer).map(|n| n.clamp(1, 16) as usize),
        hard_tabs: config.get("hard_tabs").and_then(Value::as_bool),
        formatter,
        server: None,
        extension: extension.to_string(),
        highlights: query("highlights.scm", "highlights.scm").unwrap_or_default(),
        outline: query("outline.scm", "tags.scm"),
        dir: dir.to_path_buf(),
        tasks: std::fs::read_to_string(dir.join("tasks.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&crate::settings::strip_jsonc(&text)).ok())
            .unwrap_or_default(),
        grammar,
        loaded: OnceLock::new(),
    })
}

fn read_toml(path: &Path) -> Result<Table, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    text.parse::<Table>().map_err(|e| format!("{}: {}", path.display(), e.message()))
}

fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Registry entries live as long as Kin (and are few).
fn leak(s: impl Into<String>) -> &'static str {
    Box::leak(s.into().into_boxed_str())
}

// ---- Installing --------------------------------------------------------------------

/// Links a folder in (like Zed's dev extensions): it's used in place, so its
/// author's edits apply on Reload. Returns the extension's name.
/// Where Zed keeps the extensions it installed: packaged, so their grammars
/// are prebuilt WASM and need no compiler here.
fn zed_installed_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dir = if cfg!(target_os = "macos") {
        home?.join("Library/Application Support/Zed")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Zed")
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| Some(home?.join(".local/share")))?.join("zed")
    };
    Some(dir.join("extensions").join("installed"))
}

/// Zed's installed extensions that Kin doesn't have yet: (name, folder).
pub fn zed_extensions() -> Vec<(String, PathBuf)> {
    let have: Vec<String> = installed().into_iter().map(|e| e.id).collect();
    let mut found: Vec<(String, PathBuf)> = zed_installed_dir()
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter_map(|path| {
            let manifest = read_toml(&path.join("extension.toml")).ok()?;
            let id = manifest.get("id").and_then(Value::as_str).map(str::to_string).or_else(|| Some(path.file_name()?.to_string_lossy().into_owned()))?;
            if have.contains(&id) {
                return None;
            }
            let name = manifest.get("name").and_then(Value::as_str).map(str::to_string).unwrap_or(id);
            Some((name, path))
        })
        .collect();
    found.sort();
    found
}

pub fn install_folder(source: &Path) -> Result<String, String> {
    let manifest = read_toml(&source.join("extension.toml")).map_err(|_| "That folder has no extension.toml".to_string())?;
    let id = manifest.get("id").and_then(Value::as_str).map(str::to_string).or_else(|| Some(source.file_name()?.to_string_lossy().into_owned()));
    let id = id.ok_or("The extension has no id")?;
    let target = target_for(&id)?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(source, &target).map_err(|e| e.to_string())?;
    #[cfg(not(unix))]
    copy_dir(source, &target).map_err(|e| e.to_string())?;
    Ok(manifest.get("name").and_then(Value::as_str).unwrap_or(&id).to_string())
}

/// Clones an extension's repository (a Zed extension's works). Blocking.
pub fn install_git(url: &str) -> Result<String, String> {
    let dir = extensions_dir().ok_or("no config folder")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let temp = dir.join(format!(".installing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    let temp_name = temp.to_string_lossy().into_owned();
    crate::git::run(&dir, &["clone", "--depth", "1", url.trim(), &temp_name])?;
    let installed = (|| {
        let manifest = read_toml(&temp.join("extension.toml")).map_err(|_| "The repository has no extension.toml".to_string())?;
        let id = manifest.get("id").and_then(Value::as_str).ok_or("The extension has no id")?.to_string();
        let target = target_for(&id)?;
        std::fs::rename(&temp, &target).map_err(|e| e.to_string())?;
        Ok(manifest.get("name").and_then(Value::as_str).unwrap_or(&id).to_string())
    })();
    let _ = std::fs::remove_dir_all(&temp);
    installed
}

fn target_for(id: &str) -> Result<PathBuf, String> {
    if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
        return Err(format!("\"{id}\" isn't a usable extension id"));
    }
    let dir = extensions_dir().ok_or("no config folder")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let target = dir.join(id);
    if target.exists() || target.is_symlink() {
        return Err(format!("{id} is installed already"));
    }
    Ok(target)
}

#[cfg(not(unix))]
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let path = entry.path();
        if path.is_dir() { copy_dir(&path, &to.join(entry.file_name()))? } else { std::fs::copy(&path, to.join(entry.file_name())).map(|_| ())? }
    }
    Ok(())
}

/// Removes an extension: a linked folder is only unlinked; an installed one
/// goes to the Trash.
pub fn uninstall(path: &Path) -> Result<(), String> {
    if path.is_symlink() {
        std::fs::remove_file(path).map_err(|e| e.to_string())
    } else {
        trash::delete(path).map_err(|e| e.to_string())
    }
}

// ---- Grammars ----------------------------------------------------------------------

/// Not compiled yet, or its sources changed since. A prebuilt WASM grammar
/// never needs building.
fn needs_build(grammar: &Grammar) -> bool {
    if grammar.wasm.is_file() {
        return false;
    }
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (modified(&grammar.library), modified(&grammar.src.join("parser.c"))) {
        (None, _) => true,
        (Some(built), Some(source)) => source > built,
        (Some(_), None) => false,
    }
}

/// Fetches the grammar if the extension doesn't bundle it, then compiles it.
/// Blocking.
fn build(grammar: &Grammar) -> Result<(), String> {
    BUILDING.lock().unwrap().insert(grammar.library.clone());
    let result = fetch(grammar).and_then(|()| compile(grammar));
    BUILDING.lock().unwrap().remove(&grammar.library);
    result
}

/// Like Zed: a shallow fetch of exactly `rev`, or the default branch.
fn fetch(grammar: &Grammar) -> Result<(), String> {
    let Some(repository) = grammar.repository.as_deref() else { return Ok(()) };
    let dir = &grammar.checkout;
    let git = |args: &[&str]| crate::git::run(dir, args);
    if grammar.src.join("parser.c").is_file() {
        let head = git(&["rev-parse", "HEAD"]).unwrap_or_default();
        if grammar.rev.as_deref().is_none_or(|rev| head.trim() == rev) {
            return Ok(());
        }
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    if !dir.join(".git").exists() {
        git(&["init", "--quiet"])?;
        git(&["remote", "add", "origin", repository])?;
    }
    let rev = grammar.rev.as_deref().unwrap_or("HEAD");
    git(&["fetch", "--depth", "1", "origin", rev]).map_err(|e| format!("couldn't fetch {repository}: {e}"))?;
    git(&["checkout", "--quiet", "--force", "FETCH_HEAD"])?;
    Ok(())
}

/// `cc -shared` over `parser.c` and the scanner (C, or C++ with `c++`), into
/// a temporary file renamed into place, so a half-written one is never loaded.
fn compile(grammar: &Grammar) -> Result<(), String> {
    let src = &grammar.src;
    if !src.join("parser.c").is_file() {
        return Err(format!("no parser.c in {}", src.display()));
    }
    let cpp = src.join("scanner.cc").is_file();
    let compiler = std::env::var("CC").ok().filter(|_| !cpp).unwrap_or_else(|| if cpp { "c++".into() } else { "cc".into() });
    let temp = grammar.library.with_extension("building");
    let mut command = crate::process::command(&compiler);
    command.env("PATH", servers::search_path()).args(["-shared", "-fPIC", "-O2", "-w"]).arg("-I").arg(src);
    if cpp {
        command.args(["-x", "c"]).arg(src.join("parser.c")).args(["-x", "c++"]).arg(src.join("scanner.cc"));
    } else {
        command.arg(src.join("parser.c"));
        if src.join("scanner.c").is_file() {
            command.arg(src.join("scanner.c"));
        }
    }
    let output = command
        .arg("-o")
        .arg(&temp)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("couldn't run {compiler} (a C compiler is needed: xcode-select --install on macOS): {e}"))?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&temp);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.lines().find(|l| l.contains("error")).unwrap_or("the compiler failed").trim().to_string());
    }
    std::fs::rename(&temp, &grammar.library).map_err(|e| e.to_string())
}

/// One engine for every WASM grammar: a language loaded with it works in any
/// parser holding a store made from the same engine.
static WASM_ENGINE: LazyLock<tree_sitter::wasmtime::Engine> = LazyLock::new(Default::default);
/// The store grammars are loaded into; it lives as long as they do.
static WASM_LOADER: Mutex<Option<tree_sitter::WasmStore>> = Mutex::new(None);

/// A WebAssembly grammar, run by wasmtime.
fn load_wasm(path: &Path, name: &str) -> Result<TsLanguage, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut loader = WASM_LOADER.lock().unwrap_or_else(|e| e.into_inner());
    if loader.is_none() {
        *loader = Some(tree_sitter::WasmStore::new(&WASM_ENGINE).map_err(|e| e.message)?);
    }
    let store = loader.as_mut().expect("just set");
    let language = store.load_language(&name.replace('-', "_"), &bytes).map_err(|e| format!("couldn't load {}: {}", path.display(), e.message))?;
    check_abi(&language)?;
    Ok(language)
}

/// Readies `parser` for `language`: a WASM grammar runs in a store of its own.
pub fn prepare_parser(parser: &mut tree_sitter::Parser, language: &TsLanguage) {
    if !language.is_wasm() {
        return;
    }
    // A parser from the pool keeps its store (making one is the slow part).
    if let Some(store) = parser.take_wasm_store().or_else(|| tree_sitter::WasmStore::new(&WASM_ENGINE).ok()) {
        parser.set_wasm_store(store).ok();
    }
}

fn check_abi(language: &TsLanguage) -> Result<(), String> {
    let abi = language.abi_version();
    if !(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION..=tree_sitter::LANGUAGE_VERSION).contains(&abi) {
        return Err(format!(
            "generated for tree-sitter ABI {abi}; Kin reads {}–{}",
            tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION,
            tree_sitter::LANGUAGE_VERSION
        ));
    }
    Ok(())
}

/// The grammar from its library: `tree_sitter_<name>` returns it.
fn load_library(path: &Path, name: &str) -> Result<TsLanguage, String> {
    let symbol = format!("tree_sitter_{}", name.replace('-', "_"));
    // SAFETY: the library is a tree-sitter grammar compiled from its sources;
    // the symbol is the function the tree-sitter CLI generates for it.
    let language = unsafe {
        let library = libloading::Library::new(path).map_err(|e| e.to_string())?;
        let function: libloading::Symbol<unsafe extern "C" fn() -> *const ()> =
            library.get(symbol.as_bytes()).map_err(|_| format!("the grammar has no {symbol}"))?;
        let language = TsLanguage::new(tree_sitter_language::LanguageFn::from_raw(*function));
        // Its code must stay loaded as long as the grammar is used.
        std::mem::forget(library);
        language
    };
    check_abi(&language)?;
    Ok(language)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kin-ext-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: PathBuf, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn reads_zed_style_extensions() {
        let dir = temp_dir("read");
        write(
            dir.join("extension.toml"),
            r#"
id = "gleam"
name = "Gleam"
version = "0.3.1"
description = "Gleam support"
authors = ["A <a@example.com>"]

[grammars.gleam]
repository = "https://example.com/tree-sitter-gleam"
rev = "abc123"

[language_servers.gleam]
name = "Gleam LSP"
languages = ["Gleam"]
command = "gleam"
args = ["lsp"]

[language_servers.other]
name = "Needs code"
language = "Gleam"

[debug_adapters.gleam-dap]
command = "gleam-dap"
args = ["--listen", "{port}"]
tcp = true
aliases = ["Gleam"]
"#,
        );
        write(
            dir.join("languages/gleam/config.toml"),
            r#"
name = "Gleam"
grammar = "gleam"
path_suffixes = ["gleam", "Gleamfile"]
line_comments = ["// ", "/// "]
block_comment = { start = "/*", end = "*/" }
tab_size = 2
formatter = ["gleam", "format", "--stdin"]
"#,
        );
        write(dir.join("languages/gleam/highlights.scm"), "(identifier) @variable\n");
        write(dir.join("themes/gleam.json"), "{}");
        let (info, languages, servers, grammars) = read_extension(&dir);
        assert_eq!((info.id.as_str(), info.name.as_str(), info.version.as_str()), ("gleam", "Gleam", "0.3.1"));
        assert_eq!(info.languages, ["Gleam"]);
        assert_eq!(info.themes, 1);
        assert_eq!(info.servers, ["Gleam LSP (gleam)", "Needs code (other)"]);
        assert!(info.problems.is_empty(), "{:?}", info.problems);
        let dap = info.adapters[0];
        assert_eq!((dap.id, dap.aliases.as_slice(), dap.command.as_str(), dap.tcp), ("gleam-dap", &["Gleam"][..], "gleam-dap", true));

        let gleam = &languages[0];
        assert_eq!((gleam.name, gleam.id), ("Gleam", "gleam"));
        assert_eq!((gleam.line_comment, gleam.block_comment), (Some("// "), Some(("/* ", " */"))));
        assert_eq!((gleam.tab_size, gleam.formatter.as_deref()), (Some(2), Some(&["gleam".to_string(), "format".into(), "--stdin".into()][..])));
        assert!(gleam.matches(Path::new("src/app.gleam")) && gleam.matches(Path::new("Gleamfile")));
        assert!(!gleam.matches(Path::new("app.gleamx")) && !gleam.matches(Path::new("notgleam")));
        assert_eq!(gleam.highlights, "(identifier) @variable\n");

        // Both servers, the first installed one runs: its command, else the id.
        let spec = servers["Gleam"];
        assert_eq!(spec.candidates, &[("gleam", &["lsp"][..]), ("other", &[][..])]);
        let grammar = &grammars["gleam"];
        assert_eq!((grammar.repository.as_deref(), grammar.rev.as_deref()), (Some("https://example.com/tree-sitter-gleam"), Some("abc123")));
        assert!(grammar.src.ends_with("grammars/gleam/src"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_broken_manifests() {
        let dir = temp_dir("broken");
        write(dir.join("extension.toml"), "id = \"x\"\nname = ");
        let (info, languages, _, _) = read_extension(&dir);
        assert!(languages.is_empty());
        assert_eq!(info.problems.len(), 1, "{:?}", info.problems);
        write(dir.join("extension.toml"), "id = \"x\"\nname = \"X\"\n");
        write(dir.join("languages/x/config.toml"), "name = \"X\"\ngrammar = \"missing\"\n");
        let (info, languages, _, _) = read_extension(&dir);
        assert!(languages.is_empty() && info.problems[0].contains("grammar missing isn't in extension.toml"), "{:?}", info.problems);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Install from Git and from a folder, the registry they make, and
    /// removing a linked one. Points the config folder elsewhere, so it runs
    /// alone.
    #[test]
    #[ignore = "changes XDG_CONFIG_HOME and needs git; run alone with --ignored"]
    fn installs_and_removes_extensions() {
        let config = temp_dir("config");
        // SAFETY: this test runs on its own (it's ignored by default).
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &config) };

        let repo = temp_dir("ext-repo");
        write(repo.join("extension.toml"), "id = \"demo-git\"\nname = \"Demo Git\"\nversion = \"1.0.0\"\n");
        write(repo.join("languages/demo/config.toml"), "name = \"DemoLang\"\npath_suffixes = [\"demo\"]\nline_comments = [\"-- \"]\n");
        write(repo.join("snippets/demolang.json"), "{ \"Hello\": { \"prefix\": \"hi\", \"body\": \"hello\" } }");
        write(repo.join("themes/demo.json"), r##"{ "name": "Demo", "themes": [{ "name": "Demo Dark", "appearance": "dark", "style": { "editor.background": "#000000" } }] }"##);
        let git = |args: &[&str]| crate::git::run(&repo, args).unwrap();
        git(&["init", "--quiet"]);
        git(&["add", "-A"]);
        git(&["-c", "user.name=k", "-c", "user.email=k@k", "commit", "--quiet", "-m", "ext"]);
        let url = format!("file://{}", repo.display());
        assert_eq!(install_git(&url), Ok("Demo Git".to_string()));
        assert!(install_git(&url).unwrap_err().contains("installed already"));

        let folder = temp_dir("ext-folder");
        write(folder.join("extension.toml"), "id = \"linked\"\nname = \"Linked\"\n");
        assert_eq!(install_folder(&folder), Ok("Linked".to_string()));
        assert!(install_folder(&config).is_err(), "a folder without extension.toml");

        assert!(load().is_empty());
        let names: Vec<String> = installed().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["Demo Git", "Linked"]);
        let language = crate::syntax::Language::from_path(Path::new("x.demo"));
        assert_eq!((language.name(), language.line_comment()), ("DemoLang", Some("-- ")));
        assert_eq!(crate::snippets::user_snippets(language).len(), 1);
        assert!(crate::theme::ThemeRegistry::load().find("Demo Dark").is_some());

        let linked = installed().into_iter().find(|e| e.name == "Linked").unwrap();
        uninstall(&linked.path).unwrap();
        assert!(!linked.path.exists() && folder.join("extension.toml").is_file(), "only the link goes");
        load();
        assert_eq!(installed().len(), 1);
        for dir in [config, repo, folder] {
            std::fs::remove_dir_all(dir).ok();
        }
    }

    /// The whole path a Zed extension's grammar takes: fetched from a
    /// repository at a revision, compiled, loaded, then highlighting a file.
    #[test]
    #[ignore = "needs git, a C compiler and the tree-sitter-json crate's sources; run with --ignored"]
    fn fetches_builds_and_loads_a_grammar() {
        let home = std::env::var("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap()).join(".cargo"));
        let sources = std::fs::read_dir(home.join("registry/src"))
            .unwrap()
            .flatten()
            .flat_map(|index| std::fs::read_dir(index.path()).into_iter().flatten().flatten())
            .map(|e| e.path())
            .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("tree-sitter-json-")))
            .expect("tree-sitter-json in the cargo registry");

        let repo = temp_dir("grammar-repo");
        let git = |args: &[&str]| crate::git::run(&repo, args).unwrap();
        for entry in std::fs::read_dir(sources.join("src")).unwrap().flatten() {
            if entry.path().is_file() {
                write(repo.join("src").join(entry.file_name()), &std::fs::read_to_string(entry.path()).unwrap());
            }
        }
        write(repo.join("src/tree_sitter/parser.h"), &std::fs::read_to_string(sources.join("src/tree_sitter/parser.h")).unwrap());
        write(repo.join("queries/highlights.scm"), "(string) @string\n(number) @number\n");
        git(&["init", "--quiet"]);
        git(&["add", "-A"]);
        git(&["-c", "user.name=k", "-c", "user.email=k@k", "commit", "--quiet", "-m", "grammar"]);
        let rev = git(&["rev-parse", "HEAD"]).trim().to_string();

        let ext = temp_dir("grammar-ext");
        write(ext.join("extension.toml"), &format!("id = \"json5\"\nname = \"JSON5\"\n[grammars.json]\nrepository = \"file://{}\"\nrev = \"{rev}\"\n", repo.display()));
        write(ext.join("languages/json5/config.toml"), "name = \"JSON5\"\ngrammar = \"json\"\npath_suffixes = [\"json5\"]\n");
        let (info, languages, _, grammars) = read_extension(&ext);
        assert!(info.problems.is_empty(), "{:?}", info.problems);
        let grammar = &grammars["json"];
        assert!(needs_build(grammar));
        build(grammar).unwrap();
        assert!(!needs_build(grammar));

        // The language takes the grammar's own highlights query.
        let def: &'static LanguageDef = Box::leak(Box::new(read_language(&ext.join("languages/json5"), &grammars, "json5").unwrap()));
        let language = def.grammar().expect("the grammar loads");
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse("{\"a\": 1}", None).unwrap();
        assert_eq!(tree.root_node().kind(), "document");
        assert!(def.highlight_config().is_some());
        drop(languages);
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&ext).ok();
    }
}
