//! Which language server each language uses, and finding it on disk: the
//! project's (`node_modules/.bin`, a venv), then one on PATH, then a copy Kin
//! installed itself (see `install.rs`, the status bar's Install).

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use crate::syntax::Language;

pub struct ServerSpec {
    /// Shared by languages one server handles (JS/TS/TSX).
    pub id: &'static str,
    /// Executables to try, in order, with their arguments.
    pub candidates: &'static [(&'static str, &'static [&'static str])],
    /// How to install the first candidate.
    pub install: &'static str,
    /// Files that mark a project root for this server.
    pub root_markers: &'static [&'static str],
}

/// For a language whose server is only a command in `language_servers`.
pub static CUSTOM: ServerSpec = ServerSpec {
    id: "custom",
    candidates: &[],
    install: "Check the command in language_servers (settings.json)",
    root_markers: &[],
};

/// The main server for `language` (see `specs`).
pub fn spec(language: Language) -> Option<&'static ServerSpec> {
    specs(language).into_iter().next()
}

/// Every server a `language` document gets, the main one first: Kin's own
/// choice, then the ones extensions bring, which run beside it (ESLint or
/// Tailwind beside TypeScript). Requests go to the first server that can
/// answer; diagnostics, completion and code actions come from all. An
/// extension's server that's the same program as an earlier one (a choice
/// Kin already makes) isn't started twice.
pub fn specs(language: Language) -> Vec<&'static ServerSpec> {
    combine(builtin(language), crate::extensions::servers_for(language.name()))
}

fn combine(builtin: Option<&'static ServerSpec>, extensions: Vec<&'static ServerSpec>) -> Vec<&'static ServerSpec> {
    let mut out: Vec<&'static ServerSpec> = builtin.into_iter().collect();
    for spec in extensions {
        let same = |s: &&ServerSpec| s.candidates.iter().any(|(a, _)| spec.candidates.iter().any(|(b, _)| a == b));
        if !out.iter().any(same) {
            out.push(spec);
        }
    }
    out
}

/// Kin's own server for a built-in language.
fn builtin(language: Language) -> Option<&'static ServerSpec> {
    static RUST: ServerSpec = ServerSpec {
        id: "rust-analyzer",
        candidates: &[("rust-analyzer", &[])],
        install: "rustup component add rust-analyzer",
        root_markers: &["Cargo.toml"],
    };
    static GO: ServerSpec = ServerSpec {
        id: "gopls",
        candidates: &[("gopls", &[])],
        install: "go install golang.org/x/tools/gopls@latest",
        root_markers: &["go.mod", "go.work"],
    };
    static TYPESCRIPT: ServerSpec = ServerSpec {
        id: "typescript",
        candidates: &[("typescript-language-server", &["--stdio"]), ("vtsls", &["--stdio"])],
        install: "npm install -g typescript typescript-language-server",
        root_markers: &["tsconfig.json", "jsconfig.json", "package.json"],
    };
    static PYTHON: ServerSpec = ServerSpec {
        id: "python",
        candidates: &[
            ("basedpyright-langserver", &["--stdio"]),
            ("pyright-langserver", &["--stdio"]),
            ("pylsp", &[]),
        ],
        install: "pip install basedpyright",
        root_markers: &["pyproject.toml", "setup.py", "requirements.txt"],
    };
    static PHP: ServerSpec = ServerSpec {
        id: "php",
        candidates: &[("intelephense", &["--stdio"]), ("phpactor", &["language-server"])],
        install: "npm install -g intelephense",
        root_markers: &["composer.json"],
    };
    static CSS: ServerSpec = ServerSpec {
        id: "css",
        candidates: &[("vscode-css-language-server", &["--stdio"])],
        install: "npm install -g vscode-langservers-extracted",
        root_markers: &["package.json"],
    };
    static HTML: ServerSpec = ServerSpec {
        id: "html",
        candidates: &[("vscode-html-language-server", &["--stdio"])],
        install: "npm install -g vscode-langservers-extracted",
        root_markers: &["package.json"],
    };
    static JSON: ServerSpec = ServerSpec {
        id: "json",
        candidates: &[("vscode-json-language-server", &["--stdio"])],
        install: "npm install -g vscode-langservers-extracted",
        root_markers: &["package.json"],
    };
    static YAML: ServerSpec = ServerSpec {
        id: "yaml",
        candidates: &[("yaml-language-server", &["--stdio"])],
        install: "npm install -g yaml-language-server",
        root_markers: &[],
    };
    static TOML: ServerSpec = ServerSpec {
        id: "taplo",
        candidates: &[("taplo", &["lsp", "stdio"])],
        install: "cargo install taplo-cli --locked --features lsp",
        root_markers: &[],
    };
    static SHELL: ServerSpec = ServerSpec {
        id: "bash",
        candidates: &[("bash-language-server", &["start"])],
        install: "npm install -g bash-language-server",
        root_markers: &[],
    };
    static MARKDOWN: ServerSpec = ServerSpec {
        id: "marksman",
        candidates: &[("marksman", &["server"])],
        install: "brew install marksman",
        root_markers: &[],
    };
    Some(match language {
        Language::Rust => &RUST,
        Language::Go => &GO,
        Language::JavaScript | Language::TypeScript | Language::Tsx => &TYPESCRIPT,
        Language::Python => &PYTHON,
        Language::Php => &PHP,
        Language::Css => &CSS,
        Language::Html => &HTML,
        Language::Json => &JSON,
        Language::Yaml => &YAML,
        Language::Toml => &TOML,
        Language::Shell => &SHELL,
        Language::Markdown => &MARKDOWN,
        Language::PlainText | Language::Extension(_) => return None,
    })
}

/// The `languageId` for `textDocument/didOpen`.
pub fn language_id(language: Language, path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
    match language {
        Language::JavaScript if ext == "jsx" => "javascriptreact",
        Language::JavaScript => "javascript",
        Language::TypeScript => "typescript",
        Language::Tsx => "typescriptreact",
        Language::Json if ext == "jsonc" => "jsonc",
        Language::Json => "json",
        Language::Html => "html",
        Language::Css => "css",
        Language::Rust => "rust",
        Language::Go => "go",
        Language::Python => "python",
        Language::Php => "php",
        Language::Shell => "shellscript",
        Language::Yaml => "yaml",
        Language::Toml => "toml",
        Language::Markdown => "markdown",
        Language::PlainText => "plaintext",
        Language::Extension(_) => language.extension().map_or("plaintext", |l| l.id),
    }
}

/// The project root a server should use for `file`: the open folder if the
/// file is inside it, else the nearest folder with a root marker.
pub fn root_for(spec: &ServerSpec, file: &Path, workspace_root: Option<&Path>) -> PathBuf {
    if let Some(root) = workspace_root.filter(|r| file.starts_with(r)) {
        return root.to_path_buf();
    }
    let dir = file.parent().unwrap_or(file);
    dir.ancestors()
        .find(|d| spec.root_markers.iter().chain(&[".git"]).any(|m| d.join(m).exists()))
        .unwrap_or(dir)
        .to_path_buf()
}

/// A server command: the executable and its arguments.
#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// Finds an installed server: the project's `node_modules/.bin` or Python
/// venv first (a pinned version), then the search path.
pub fn find(spec: &ServerSpec, file: &Path, venv: Option<&Path>) -> Option<Command> {
    let command = |program: PathBuf, args: &[&str]| Command { program, args: args.iter().map(|a| a.to_string()).collect() };
    for (name, args) in spec.candidates {
        let local = file
            .ancestors()
            .skip(1)
            .find_map(|d| executable(&d.join("node_modules").join(".bin").join(name)))
            .or_else(|| executable(&crate::python::bin_dir(venv?).join(name)));
        if let Some(program) = local.or_else(|| which(name).filter(|p| !broken_rustup_proxy(p))) {
            return Some(command(program, args));
        }
    }
    // Only then Kin's own copies: the user's servers, any of them, come first.
    spec.candidates.iter().find_map(|(name, args)| Some(command(super::install::installed(name)?, args)))
}

/// rustup's `rust-analyzer` in `~/.cargo/bin` is there without the component
/// installed; it then only says so. Such a proxy doesn't count as a server.
fn broken_rustup_proxy(program: &Path) -> bool {
    program.file_stem().is_some_and(|n| n == "rust-analyzer")
        && program.parent().is_some_and(|dir| dir.ends_with(".cargo/bin"))
        && !crate::process::command(program)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

/// A user-configured command (`language_servers` in settings).
pub fn custom(command: &[String]) -> Option<Command> {
    let (name, args) = command.split_first()?;
    let path = Path::new(name);
    let program = if path.components().count() > 1 { executable(path)? } else { which(name)? };
    Some(Command { program, args: args.to_vec() })
}

pub fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(search_path()).find_map(|dir| executable(&dir.join(name)))
}

pub(crate) fn executable(path: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        for ext in ["exe", "cmd", "bat"] {
            let p = path.with_extension(ext);
            if p.is_file() {
                return Some(p);
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).ok()?;
        (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then(|| path.to_path_buf())
    }
}

/// PATH for finding and running servers. Apps started from the Dock or
/// Finder get a minimal PATH, so this adds the login shell's PATH and the
/// usual install folders (cargo, go, Homebrew, npm via nvm…).
pub fn search_path() -> &'static OsString {
    static PATH: LazyLock<OsString> = LazyLock::new(|| {
        let mut dirs: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
        let mut add = |dir: PathBuf| {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        };
        if let Some(shell_path) = login_shell_path() {
            std::env::split_paths(&shell_path).for_each(&mut add);
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            for sub in [".cargo/bin", "go/bin", ".local/bin", ".bun/bin", ".deno/bin", ".volta/bin"] {
                add(home.join(sub));
            }
            // nvm keeps node (and global npm packages) per version; use the newest.
            if let Ok(versions) = std::fs::read_dir(home.join(".nvm/versions/node")) {
                let mut found: Vec<PathBuf> = versions.filter_map(|e| e.ok()).map(|e| e.path().join("bin")).collect();
                found.sort();
                if let Some(newest) = found.pop() {
                    add(newest);
                }
            }
        }
        for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
            add(PathBuf::from(dir));
        }
        std::env::join_paths(dirs).unwrap_or_default()
    });
    &PATH
}

/// `$PATH` as the user's login shell sets it, with a timeout so a slow or
/// interactive shell config can't hang Kin.
#[cfg(unix)]
fn login_shell_path() -> Option<OsString> {
    use std::{io::Read, os::unix::process::CommandExt, process::Stdio, time::Duration};
    const MARK: &str = "__KIN_PATH__";
    let shell = std::env::var("SHELL").ok()?;
    let mut command = crate::process::command(shell);
    command
        .args(["-l", "-i", "-c", &format!("printf '{MARK}%s{MARK}' \"$PATH\"")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // An interactive shell opens /dev/tty and makes itself the terminal's foreground job,
    // which stops (or breaks) the shell Kin was started from. Its own session has no terminal.
    // SAFETY: setsid is async-signal-safe, as pre_exec requires.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().ok()?;
    for _ in 0..60 {
        if child.try_wait().ok()?.is_some() {
            let mut out = String::new();
            child.stdout.take()?.read_to_string(&mut out).ok()?;
            let start = out.find(MARK)? + MARK.len();
            let end = start + out[start..].find(MARK)?;
            return Some(out[start..end].into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    None
}

#[cfg(not(unix))]
fn login_shell_path() -> Option<OsString> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_but_plain_text_has_a_server() {
        for language in [Language::Rust, Language::Tsx, Language::Markdown, Language::Toml] {
            assert!(spec(language).is_some());
        }
        assert!(spec(Language::PlainText).is_none());
        assert_eq!(spec(Language::JavaScript).unwrap().id, spec(Language::Tsx).unwrap().id);
        assert_eq!(language_id(Language::JavaScript, Path::new("a.jsx")), "javascriptreact");
    }

    #[test]
    fn extension_servers_run_beside_the_built_in_one() {
        static ESLINT: ServerSpec = ServerSpec { id: "eslint", candidates: &[("vscode-eslint-language-server", &["--stdio"])], install: "", root_markers: &[] };
        static VTSLS: ServerSpec = ServerSpec { id: "ext:vtsls", candidates: &[("vtsls", &["--stdio"])], install: "", root_markers: &[] };
        let ids = |specs: Vec<&ServerSpec>| specs.iter().map(|s| s.id).collect::<Vec<_>>();
        // ESLint joins TypeScript; an extension's vtsls is one of Kin's own
        // choices for TypeScript already, so it isn't started a second time.
        assert_eq!(ids(combine(builtin(Language::TypeScript), vec![&ESLINT, &VTSLS])), ["typescript", "eslint"]);
        // A language Kin has no server for gets the extensions' alone.
        assert_eq!(ids(combine(None, vec![&VTSLS, &ESLINT])), ["ext:vtsls", "eslint"]);
    }

    #[test]
    fn roots() {
        let rust = spec(Language::Rust).unwrap();
        assert_eq!(root_for(rust, Path::new("/w/src/a.rs"), Some(Path::new("/w"))), PathBuf::from("/w"));
        let dir = std::env::temp_dir().join(format!("kin-root-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crate/src")).unwrap();
        std::fs::write(dir.join("crate/Cargo.toml"), "").unwrap();
        assert_eq!(root_for(rust, &dir.join("crate/src/main.rs"), None), dir.join("crate"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
