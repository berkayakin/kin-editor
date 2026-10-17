//! Zed extensions' own code: `extension.wasm`, a WebAssembly component built
//! against Zed's extension API, run with wasmtime. Kuana runs it for language
//! servers only: the command that starts one (the code may download the
//! server first), its initialization options and its workspace
//! configuration. API 0.6.0 and 0.7.0 share one interface (`wit/`, from Zed's
//! `zed_extension_api` crate); other extensions keep Kuana's guess
//! (`extensions::KNOWN_SERVERS`).
//!
//! The code runs in WASI's sandbox with one folder of its own,
//! `<config>/extensions/work/<id>/` (its current directory, where servers are
//! downloaded to). Everything else goes through the host functions here,
//! which use what Kuana has: `curl` for HTTP and GitHub, its Node.js and npm,
//! the augmented PATH, the `lsp.<id>` settings (by Zed's server id). Running a
//! program (`process`) needs the project to be trusted. Slash commands,
//! context servers, the docs database and debug adapters answer with errors.

use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Child, ChildStdout, Stdio},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use serde_json::{Value, json};
use wasmtime::{
    Engine, Store,
    component::{Component as WasmComponent, HasSelf, Linker, Resource, ResourceTable},
};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::{lsp::servers, settings::LspSettings};

wasmtime::component::bindgen!({
    world: "extension",
    path: "src/extensions/wit/since_v0.6.0",
    with: {
        "worktree": ProjectRoot,
        "zed:extension/http-client/http-response-stream": ResponseStream,
    },
});

use zed::extension::{
    dap::{TcpArguments, TcpArgumentsTemplate},
    github::{GithubRelease, GithubReleaseAsset, GithubReleaseOptions},
    http_client::{HttpMethod, HttpRequest, HttpResponse, RedirectPolicy},
    platform::{Architecture, Os},
    process::Output,
};

/// Whether Kuana runs an extension's code: an `extension.wasm` built for
/// extension API 0.6 or 0.7 (`[lib] version` in its manifest).
pub fn supported(manifest: &toml::Table, dir: &Path) -> bool {
    let version = manifest.get("lib").and_then(|lib| lib.get("version")).and_then(toml::Value::as_str).unwrap_or_default();
    let mut parts = version.split('.');
    matches!((parts.next(), parts.next()), (Some("0"), Some("6" | "7"))) && dir.join("extension.wasm").is_file()
}

/// An extension whose code Kuana runs.
#[derive(Debug)]
pub struct WasmExtension {
    pub id: String,
    /// Its folder, with `extension.wasm`.
    pub dir: PathBuf,
}

/// An extension's servers for a language, which its code starts: Zed's ids
/// for them, in the manifest's order. The first one the code can start runs.
#[derive(Debug)]
pub struct WasmServers {
    pub extension: &'static WasmExtension,
    pub ids: &'static [&'static str],
}

/// Kuana's settings as the extension's code may read them, as of the call.
#[derive(Clone, Default, Debug)]
pub struct CallSettings {
    /// `lsp.<id>`, by Zed's server id.
    pub lsp: HashMap<String, LspSettings>,
    /// Tab sizes by language name (the server's language).
    pub tab_sizes: Vec<(String, usize)>,
}

/// How to start one of an extension's servers, from its code.
#[derive(Debug)]
pub struct Started {
    /// Zed's id for the server (its `lsp.<id>` settings).
    pub server: &'static str,
    pub command: servers::Command,
    pub initialization_options: Option<Value>,
    pub settings: Option<Value>,
}

impl WasmServers {
    /// Asks the extension's code to start its first server it can, for the
    /// project at `root`. Blocking: the code may download the server.
    pub fn start(&self, root: &Path, settings: &CallSettings) -> Result<Started, String> {
        let mut first_error = None;
        for &server in self.ids {
            match self.extension.start(server, root, settings) {
                Ok(started) => return Ok(started),
                Err(err) => {
                    eprintln!("kuana: {} extension: {err}", self.extension.id);
                    first_error.get_or_insert(err);
                }
            }
        }
        Err(first_error.unwrap_or_else(|| format!("the {} extension has no language server", self.extension.id)))
    }

    /// What this extension's servers add to another server's initialization
    /// options and workspace configuration (`target`, by Zed's id): Vue's
    /// TypeScript plugin for vtsls, say.
    pub fn additional(&self, target: &str, root: &Path, settings: &CallSettings) -> (Option<Value>, Option<Value>) {
        let mut options = None;
        let mut configuration = None;
        for &server in self.ids.iter().filter(|id| **id != target) {
            let ask = |f: &dyn Fn(&Extension, &mut Store<State>, Resource<ProjectRoot>) -> wasmtime::Result<Result<Option<String>, String>>| {
                match self.extension.call(root, settings, f).and_then(json_answer) {
                    Ok(value) => value,
                    Err(err) => {
                        eprintln!("kuana: {} extension, {server} for {target}: {err}", self.extension.id);
                        None
                    }
                }
            };
            if let Some(value) = ask(&|e, store, project| e.call_language_server_additional_initialization_options(store, server, target, project)) {
                servers::merge(options.get_or_insert(Value::Null), &value);
            }
            if let Some(value) = ask(&|e, store, project| e.call_language_server_additional_workspace_configuration(store, server, target, project)) {
                servers::merge(configuration.get_or_insert(Value::Null), &value);
            }
        }
        (options, configuration)
    }
}

impl WasmExtension {
    fn start(&self, server: &'static str, root: &Path, settings: &CallSettings) -> Result<Started, String> {
        // The code's own errors go with the server's name.
        let own = |err: String| format!("{server}: {err}");
        let command = self.call(root, settings, &|e, store, project| e.call_language_server_command(store, server, project))?.map_err(own)?;
        let initialization_options =
            json_answer(self.call(root, settings, &|e, store, project| e.call_language_server_initialization_options(store, server, project))?)
                .map_err(own)?;
        let configuration =
            json_answer(self.call(root, settings, &|e, store, project| e.call_language_server_workspace_configuration(store, server, project))?)
                .map_err(own)?;
        let work = work_dir(&self.id)?;
        Ok(Started {
            server,
            command: servers::Command { program: program_path(&work, &command.command), args: command.args, env: command.env },
            initialization_options,
            settings: configuration,
        })
    }

    /// Runs `f` on the extension's instance (made first if it has none, or
    /// its `extension.wasm` changed), with the project at `root` as its
    /// worktree. A trap ends the instance; the next call makes a new one.
    fn call<T>(
        &self,
        root: &Path,
        settings: &CallSettings,
        f: &dyn Fn(&Extension, &mut Store<State>, Resource<ProjectRoot>) -> wasmtime::Result<T>,
    ) -> Result<T, String> {
        let slot = INSTANCES.lock().unwrap_or_else(|e| e.into_inner()).entry(self.id.clone()).or_default().clone();
        let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
        let wasm = self.dir.join("extension.wasm");
        let modified = std::fs::metadata(&wasm).and_then(|m| m.modified()).ok();
        if slot.as_ref().is_none_or(|instance| instance.modified != modified || instance.wasm != wasm) {
            *slot = None;
            *slot = Some(Instance::new(self, wasm, modified)?);
        }
        let instance = slot.as_mut().expect("just made");
        let store = &mut instance.store;
        store.data_mut().call = Some(Call { root: root.to_path_buf(), settings: settings.clone() });
        let project = store.data_mut().table.push(ProjectRoot { root: root.to_path_buf() }).map_err(|e| e.to_string())?;
        let rep = project.rep();
        let result = f(&instance.bindings, store, project);
        store.data_mut().table.delete(Resource::<ProjectRoot>::new_own(rep)).ok();
        store.data_mut().call = None;
        result.map_err(|trap| {
            *slot = None;
            format!("the {} extension's code failed: {trap:#}", self.id)
        })
    }
}

/// A JSON answer (`option<string>`) parsed.
fn json_answer(answer: Result<Option<String>, String>) -> Result<Option<Value>, String> {
    match answer? {
        Some(text) => serde_json::from_str(&text).map(Some).map_err(|e| format!("the extension's JSON didn't parse: {e}")),
        None => Ok(None),
    }
}

/// A command the code returned: absolute, or relative to its work folder.
/// A bare name the folder doesn't have is looked up on PATH.
fn program_path(work: &Path, command: &str) -> PathBuf {
    let path = Path::new(command);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let inside = work.join(path);
    if path.components().count() == 1 && !inside.exists() {
        return servers::which(command).unwrap_or(inside);
    }
    inside
}

/// Where an extension's code keeps its files (and runs).
pub fn work_dir(id: &str) -> Result<PathBuf, String> {
    if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
        return Err(format!("\"{id}\" isn't a usable extension id"));
    }
    let dir = super::extensions_dir().ok_or("no config folder")?.join("work").join(id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't make {}: {e}", dir.display()))?;
    Ok(dir)
}

// ---- Instances -------------------------------------------------------------------------

static ENGINE: LazyLock<Engine> = LazyLock::new(|| {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    Engine::new(&config).expect("wasmtime's default configuration")
});

static LINKER: LazyLock<Result<Linker<State>, String>> = LazyLock::new(|| {
    let mut linker = Linker::new(&ENGINE);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|e| e.to_string())?;
    Extension::add_to_linker::<State, HasSelf<State>>(&mut linker, |state| state).map_err(|e| e.to_string())?;
    Ok(linker)
});

/// Each extension's instance, kept between calls (its code remembers things,
/// such as where it put a server). One call at a time per extension.
static INSTANCES: LazyLock<Mutex<HashMap<String, Arc<Mutex<Option<Instance>>>>>> = LazyLock::new(Default::default);

struct Instance {
    store: Store<State>,
    bindings: Extension,
    wasm: PathBuf,
    modified: Option<SystemTime>,
}

impl Instance {
    /// Compiles and instantiates the extension's code, then lets it set up
    /// (`init-extension`).
    fn new(extension: &WasmExtension, wasm: PathBuf, modified: Option<SystemTime>) -> Result<Self, String> {
        let failed = |e: wasmtime::Error| format!("couldn't run the {} extension's code: {e:#}", extension.id);
        let work = work_dir(&extension.id)?;
        let component = WasmComponent::from_file(&ENGINE, &wasm).map_err(failed)?;
        let linker = LINKER.as_ref().map_err(|e| format!("couldn't set up extensions' code: {e}"))?;
        // Its current directory is the work folder (the API reads PWD), which
        // it reaches by that path and as `.`.
        let work_name = work.to_string_lossy().into_owned();
        let mut wasi = WasiCtxBuilder::new();
        wasi.inherit_stdout().inherit_stderr().allow_blocking_current_thread(true).env("PWD", &work_name);
        wasi.preopened_dir(&work, ".", DirPerms::all(), FilePerms::all()).map_err(failed)?;
        wasi.preopened_dir(&work, &work_name, DirPerms::all(), FilePerms::all()).map_err(failed)?;
        let state = State { wasi: wasi.build(), table: ResourceTable::new(), extension: extension.id.clone(), work, call: None };
        let mut store = Store::new(&ENGINE, state);
        let bindings = Extension::instantiate(&mut store, &component, linker).map_err(failed)?;
        bindings.call_init_extension(&mut store).map_err(failed)?;
        Ok(Instance { store, bindings, wasm, modified })
    }
}

// ---- Host ------------------------------------------------------------------------------

/// What the host functions work with.
pub struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    extension: String,
    /// The extension's own folder (its WASI sandbox).
    work: PathBuf,
    /// The call in progress.
    call: Option<Call>,
}

struct Call {
    /// The project the server is for.
    root: PathBuf,
    settings: CallSettings,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

/// The `worktree` an extension's code gets: the server's project folder.
pub struct ProjectRoot {
    root: PathBuf,
}

/// An HTTP response being read (`fetch-stream`): curl writing the body.
pub struct ResponseStream {
    child: Child,
    body: ChildStdout,
    headers: PathBuf,
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        std::fs::remove_file(&self.headers).ok();
    }
}

impl State {
    fn root(&self, project: &Resource<ProjectRoot>) -> Result<&Path, String> {
        self.table.get(project).map(|p| p.root.as_path()).map_err(|e| e.to_string())
    }

    /// A path the code names in its work folder, which it can't leave
    /// (through `..`, or a link it made): its folder is made, and must lead
    /// there too.
    fn in_work(&self, path: &str) -> Result<PathBuf, String> {
        let inside = inside(&self.work, path)?;
        let parent = inside.parent().filter(|_| inside != self.work).unwrap_or(&self.work);
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        self.check_real(parent, path)?;
        Ok(inside)
    }

    fn check_real(&self, path: &Path, named: &str) -> Result<(), String> {
        let real = |p: &Path| std::fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()));
        if real(path)?.starts_with(real(&self.work)?) { Ok(()) } else { Err(format!("{named} is outside the extension's folder")) }
    }
}

/// `path` (relative to `base`, or absolute in it) inside `base`, unless `..`
/// leads out.
fn inside(base: &Path, path: &str) -> Result<PathBuf, String> {
    let given = Path::new(path);
    let relative = match given.strip_prefix(base) {
        Ok(rest) => rest,
        Err(_) if given.is_absolute() => return Err(format!("{path} is outside {}", base.display())),
        Err(_) => given,
    };
    let mut out = base.to_path_buf();
    let mut depth = 0;
    for part in relative.components() {
        match part {
            Component::Normal(name) => {
                out.push(name);
                depth += 1;
            }
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => {
                out.pop();
                depth -= 1;
            }
            _ => return Err(format!("{path} is outside {}", base.display())),
        }
    }
    Ok(out)
}

impl ExtensionImports for State {
    fn get_settings(&mut self, _location: Option<SettingsLocation>, category: String, key: Option<String>) -> Result<String, String> {
        let settings = self.call.as_ref().map(|c| &c.settings).ok_or("settings are only read during a call")?;
        match category.as_str() {
            "lsp" => {
                let entry = key.as_deref().and_then(|id| settings.lsp.get(id)).cloned().unwrap_or_default();
                Ok(lsp_settings_json(&entry).to_string())
            }
            "language" => {
                let tab_size = settings.tab_sizes.iter().find(|(name, _)| Some(name) == key.as_ref()).map_or(4, |(_, size)| *size);
                Ok(json!({ "tab_size": tab_size }).to_string())
            }
            other => Err(format!("Kuana has no {other} settings for extensions")),
        }
    }

    fn download_file(&mut self, url: String, file_path: String, file_type: DownloadedFileType) -> Result<(), String> {
        let destination = self.in_work(&file_path)?;
        let name = url.rsplit('/').next().unwrap_or_default();
        download(&url, &destination, &self.work, file_type, &format!("{name} ({} extension)", self.extension))
    }

    fn make_file_executable(&mut self, filepath: String) -> Result<(), String> {
        let path = self.in_work(&filepath)?;
        if !path.is_file() {
            return Err(format!("{filepath} isn't a file"));
        }
        // Not a link it made to a file elsewhere.
        self.check_real(&path, &filepath)?;
        crate::lsp::install::make_executable(&path)
    }

    fn set_language_server_installation_status(&mut self, server: String, status: LanguageServerInstallationStatus) {
        let status = match status {
            LanguageServerInstallationStatus::None => return,
            LanguageServerInstallationStatus::Downloading => "downloading".to_string(),
            LanguageServerInstallationStatus::CheckingForUpdate => "checking for updates".to_string(),
            LanguageServerInstallationStatus::Failed(err) => format!("failed: {err}"),
        };
        eprintln!("kuana: {} extension, {server}: {status}", self.extension);
    }
}

/// `lsp.<id>` as the API's `LspSettings` (JSON).
fn lsp_settings_json(entry: &LspSettings) -> Value {
    let binary = entry.binary.as_ref().map(|b| json!({ "path": b.path, "arguments": b.arguments, "env": null }));
    json!({ "binary": binary, "initialization_options": entry.initialization_options, "settings": entry.settings })
}

impl HostWorktree for State {
    fn id(&mut self, project: Resource<ProjectRoot>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        self.root(&project).ok().hash(&mut hasher);
        hasher.finish()
    }

    fn root_path(&mut self, project: Resource<ProjectRoot>) -> String {
        self.root(&project).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default()
    }

    fn read_text_file(&mut self, project: Resource<ProjectRoot>, path: String) -> Result<String, String> {
        let file = inside(self.root(&project)?, &path)?;
        std::fs::read_to_string(&file).map_err(|e| format!("{path}: {e}"))
    }

    fn which(&mut self, _project: Resource<ProjectRoot>, name: String) -> Option<String> {
        servers::which(&name).map(|p| p.to_string_lossy().into_owned())
    }

    fn shell_env(&mut self, _project: Resource<ProjectRoot>) -> EnvVars {
        let path = servers::search_path().to_string_lossy().into_owned();
        let mut env: EnvVars = std::env::vars().filter(|(key, _)| key != "PATH").collect();
        env.push(("PATH".into(), path));
        env
    }

    fn drop(&mut self, project: Resource<ProjectRoot>) -> wasmtime::Result<()> {
        self.table.delete(project)?;
        Ok(())
    }
}

impl HostProject for State {
    fn worktree_ids(&mut self, _project: Resource<Project>) -> Vec<u64> {
        Vec::new()
    }

    fn drop(&mut self, _project: Resource<Project>) -> wasmtime::Result<()> {
        Ok(())
    }
}

impl HostKeyValueStore for State {
    fn insert(&mut self, _store: Resource<KeyValueStore>, _key: String, _value: String) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }

    fn drop(&mut self, _store: Resource<KeyValueStore>) -> wasmtime::Result<()> {
        Ok(())
    }
}

const UNSUPPORTED: &str = "Kuana runs extensions' language servers only";

impl zed::extension::common::Host for State {}
impl zed::extension::context_server::Host for State {}
impl zed::extension::lsp::Host for State {}
impl zed::extension::slash_command::Host for State {}

impl zed::extension::dap::Host for State {
    fn resolve_tcp_template(&mut self, _template: TcpArgumentsTemplate) -> Result<TcpArguments, String> {
        Err(UNSUPPORTED.into())
    }
}

impl zed::extension::platform::Host for State {
    fn current_platform(&mut self) -> (Os, Architecture) {
        let os = match std::env::consts::OS {
            "macos" => Os::Mac,
            "windows" => Os::Windows,
            _ => Os::Linux,
        };
        let arch = match std::env::consts::ARCH {
            "aarch64" => Architecture::Aarch64,
            "x86" => Architecture::X86,
            _ => Architecture::X8664,
        };
        (os, arch)
    }
}

impl zed::extension::process::Host for State {
    fn run_command(&mut self, command: Command) -> Result<Output, String> {
        // Extensions run tools the project may configure (its own binaries).
        let root = self.call.as_ref().map(|c| c.root.clone()).ok_or("programs only run during a call")?;
        if !crate::trust::is_trusted(&root) {
            return Err(format!("{} isn't trusted, so extensions don't run programs for it", root.display()));
        }
        let output = crate::process::command(program_path(&self.work, &command.command))
            .args(&command.args)
            .env("PATH", servers::search_path())
            .envs(command.env)
            .current_dir(&self.work)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("couldn't run {}: {e}", command.command))?;
        Ok(Output { status: output.status.code(), stdout: output.stdout, stderr: output.stderr })
    }
}

impl zed::extension::nodejs::Host for State {
    fn node_binary_path(&mut self) -> Result<String, String> {
        crate::lsp::install::node().map(|p| p.to_string_lossy().into_owned())
    }

    fn npm_package_latest_version(&mut self, package: String) -> Result<String, String> {
        crate::lsp::install::npm_latest(npm_name(&package)?)
    }

    fn npm_package_installed_version(&mut self, package: String) -> Result<Option<String>, String> {
        let manifest = self.work.join("node_modules").join(npm_name(&package)?).join("package.json");
        let Ok(text) = std::fs::read_to_string(&manifest) else { return Ok(None) };
        let value: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", manifest.display()))?;
        Ok(value["version"].as_str().map(str::to_string))
    }

    fn npm_install_package(&mut self, package: String, version: String) -> Result<(), String> {
        if version.is_empty() || !version.chars().all(|c| c.is_ascii_alphanumeric() || ".-+_".contains(c)) {
            return Err(format!("\"{version}\" isn't a version"));
        }
        crate::lsp::install::npm_install(&self.work, npm_name(&package)?, &version)
    }
}

/// A package name as npm writes them (`name`, `@scope/name`), nothing that
/// leaves `node_modules` or reads as an option.
fn npm_name(package: &str) -> Result<&str, String> {
    let name = package.strip_prefix('@').map_or(package, |scoped| scoped);
    let parts: Vec<&str> = name.split('/').collect();
    let part = |p: &&str| !p.starts_with(['.', '-', '_']) && !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c));
    let ok = parts.iter().all(part) && parts.len() == if package.starts_with('@') { 2 } else { 1 };
    if ok { Ok(package) } else { Err(format!("\"{package}\" isn't an npm package name")) }
}

// ---- GitHub and HTTP -------------------------------------------------------------------

impl zed::extension::github::Host for State {
    fn latest_github_release(&mut self, repo: String, options: GithubReleaseOptions) -> Result<GithubRelease, String> {
        let releases = github(&format!("{}/releases?per_page=100", repo_url(&repo)?))?;
        releases
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["draft"] != true && (options.pre_release || r["prerelease"] != true))
            .filter_map(release)
            .find(|r| !options.require_assets || !r.assets.is_empty())
            .ok_or_else(|| format!("{repo} has no release"))
    }

    fn github_release_by_tag_name(&mut self, repo: String, tag: String) -> Result<GithubRelease, String> {
        let mut url = url::Url::parse(&format!("{}/releases/tags/", repo_url(&repo)?)).map_err(|e| e.to_string())?;
        url.path_segments_mut().map_err(|_| "not a URL")?.pop_if_empty().push(&tag);
        release(&github(url.as_str())?).ok_or_else(|| format!("{repo} has no release {tag}"))
    }
}

fn repo_url(repo: &str) -> Result<String, String> {
    let valid = |part: &str| !part.is_empty() && !part.starts_with('.') && part.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    match repo.split_once('/') {
        Some((owner, name)) if valid(owner) && valid(name) => Ok(format!("https://api.github.com/repos/{owner}/{name}")),
        _ => Err(format!("\"{repo}\" isn't a GitHub repository (owner/name)")),
    }
}

fn github(url: &str) -> Result<Value, String> {
    serde_json::from_slice(&crate::updater::fetch(url)?).map_err(|e| format!("GitHub's answer didn't parse: {e}"))
}

fn release(value: &Value) -> Option<GithubRelease> {
    let assets = value["assets"].as_array()?.iter().filter_map(|asset| {
        Some(GithubReleaseAsset { name: asset["name"].as_str()?.to_string(), download_url: asset["browser_download_url"].as_str()?.to_string() })
    });
    Some(GithubRelease { version: value["tag_name"].as_str()?.to_string(), assets: assets.collect() })
}

impl zed::extension::http_client::Host for State {
    fn fetch(&mut self, request: HttpRequest) -> Result<HttpResponse, String> {
        let headers = header_file();
        let result = (|| {
            let mut child = curl(&request, &headers)?.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("couldn't run curl: {e}"))?;
            send_body(&mut child, request.body)?;
            let output = child.wait_with_output().map_err(|e| format!("curl failed: {e}"))?;
            if !output.status.success() {
                return Err(curl_error(&output.stderr));
            }
            // With --head, curl prints the headers where the body goes.
            let body = if request.method == HttpMethod::Head { Vec::new() } else { output.stdout };
            Ok(HttpResponse { headers: read_headers(&headers), body })
        })();
        std::fs::remove_file(&headers).ok();
        result
    }

    fn fetch_stream(&mut self, request: HttpRequest) -> Result<Resource<ResponseStream>, String> {
        let headers = header_file();
        let mut child = curl(&request, &headers)?.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("couldn't run curl: {e}"))?;
        send_body(&mut child, request.body)?;
        let body = child.stdout.take().ok_or("curl gave no output")?;
        self.table.push(ResponseStream { child, body, headers }).map_err(|e| e.to_string())
    }
}

impl zed::extension::http_client::HostHttpResponseStream for State {
    fn next_chunk(&mut self, stream: Resource<ResponseStream>) -> Result<Option<Vec<u8>>, String> {
        let stream = self.table.get_mut(&stream).map_err(|e| e.to_string())?;
        let mut chunk = vec![0; 64 * 1024];
        let n = stream.body.read(&mut chunk).map_err(|e| e.to_string())?;
        if n > 0 {
            chunk.truncate(n);
            return Ok(Some(chunk));
        }
        let status = stream.child.wait().map_err(|e| e.to_string())?;
        if status.success() {
            return Ok(None);
        }
        let mut stderr = Vec::new();
        if let Some(mut pipe) = stream.child.stderr.take() {
            pipe.read_to_end(&mut stderr).ok();
        }
        Err(curl_error(&stderr))
    }

    fn drop(&mut self, stream: Resource<ResponseStream>) -> wasmtime::Result<()> {
        self.table.delete(stream)?;
        Ok(())
    }
}

/// curl for an extension's request, its response headers written to `headers`.
fn curl(request: &HttpRequest, headers: &Path) -> Result<std::process::Command, String> {
    if !(request.url.starts_with("https://") || request.url.starts_with("http://")) {
        return Err(format!("{} isn't an HTTP address", request.url));
    }
    let mut command = crate::process::command("curl");
    command.args(["--silent", "--show-error", "--proto", "=http,https", "--proto-redir", "=http,https", "--connect-timeout", "20"]);
    command.args(["--max-time", "600", "--dump-header"]).arg(headers);
    match request.redirect_policy {
        RedirectPolicy::NoFollow => {}
        RedirectPolicy::FollowLimit(limit) => {
            command.args(["--location", "--max-redirs", &limit.to_string()]);
        }
        RedirectPolicy::FollowAll => {
            command.args(["--location", "--max-redirs", "-1"]);
        }
    }
    let method = match request.method {
        HttpMethod::Head => None,
        HttpMethod::Get => Some("GET"),
        HttpMethod::Post => Some("POST"),
        HttpMethod::Put => Some("PUT"),
        HttpMethod::Delete => Some("DELETE"),
        HttpMethod::Options => Some("OPTIONS"),
        HttpMethod::Patch => Some("PATCH"),
    };
    match method {
        Some(method) => command.args(["--request", method]),
        None => command.arg("--head"),
    };
    for (name, value) in &request.headers {
        if format!("{name}{value}").contains(['\r', '\n']) || name.contains(':') {
            return Err(format!("the header {name} isn't valid"));
        }
        command.arg("--header").arg(format!("{name}: {value}"));
    }
    if request.body.is_some() {
        command.args(["--data-binary", "@-"]);
    }
    command.arg("--url").arg(&request.url).stdin(if request.body.is_some() { Stdio::piped() } else { Stdio::null() });
    Ok(command)
}

/// Writes a request's body to curl, which reads it all before sending.
fn send_body(child: &mut Child, body: Option<Vec<u8>>) -> Result<(), String> {
    if let (Some(body), Some(mut stdin)) = (body, child.stdin.take()) {
        stdin.write_all(&body).map_err(|e| format!("couldn't send the request: {e}"))?;
    }
    Ok(())
}

fn header_file() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!("kuana-headers-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
}

/// The last response's headers (after redirects) from curl's `--dump-header`.
fn read_headers(path: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let last = text.split("\r\n\r\n").filter(|block| !block.trim().is_empty()).last().unwrap_or_default();
    last.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect()
}

fn curl_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    format!("the request failed: {}", text.trim().trim_start_matches("curl: "))
}

/// Downloads `url` to `destination` in the work folder, unpacked as the code
/// asked: a file, or a folder for an archive.
fn download(url: &str, destination: &Path, work: &Path, file_type: DownloadedFileType, label: &str) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temp = work.join(format!(".download-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
    let name = destination.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    crate::updater::download(url, &temp, label, None)?;
    let unpacked = (|| match file_type {
        DownloadedFileType::Uncompressed => {
            remove_link(destination);
            std::fs::rename(&temp, destination).map_err(|e| e.to_string())
        }
        DownloadedFileType::Gzip => {
            remove_link(destination);
            let mut input = flate2::read::GzDecoder::new(std::fs::File::open(&temp).map_err(|e| e.to_string())?);
            let mut output = std::fs::File::create(destination).map_err(|e| e.to_string())?;
            std::io::copy(&mut input, &mut output).map(|_| ()).map_err(|e| format!("couldn't unpack {name}: {e}"))
        }
        DownloadedFileType::GzipTar | DownloadedFileType::Zip => {
            std::fs::create_dir_all(destination).map_err(|e| e.to_string())?;
            crate::lsp::install::unzip(&temp, destination)
        }
    })();
    std::fs::remove_file(&temp).ok();
    unpacked
}

/// A link where a file goes is replaced, not written through.
fn remove_link(path: &Path) {
    if path.is_symlink() {
        std::fs::remove_file(path).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_api_0_6_and_0_7() {
        let dir = std::env::temp_dir().join(format!("kuana-wasm-supported-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = |version: &str| -> toml::Table { format!("[lib]\nkind = \"Rust\"\nversion = \"{version}\"\n").parse().unwrap() };
        assert!(!supported(&manifest("0.7.0"), &dir), "no extension.wasm");
        std::fs::write(dir.join("extension.wasm"), b"").unwrap();
        assert!(supported(&manifest("0.7.0"), &dir) && supported(&manifest("0.6.0"), &dir));
        assert!(!supported(&manifest("0.5.0"), &dir) && !supported(&manifest("0.1.0"), &dir) && !supported(&manifest("1.7.0"), &dir));
        assert!(!supported(&toml::Table::new(), &dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paths_stay_in_their_folder() {
        let base = Path::new("/work/php");
        assert_eq!(inside(base, "phpactor-1/phpactor.phar"), Ok(PathBuf::from("/work/php/phpactor-1/phpactor.phar")));
        assert_eq!(inside(base, "./a/../b"), Ok(PathBuf::from("/work/php/b")));
        assert_eq!(inside(base, "/work/php/x"), Ok(PathBuf::from("/work/php/x")));
        assert!(inside(base, "../other").is_err() && inside(base, "a/../../x").is_err());
        assert!(inside(base, "/etc/passwd").is_err() && inside(base, "/work/php/../x").is_err());
        assert_eq!(program_path(base, "/usr/bin/dart"), PathBuf::from("/usr/bin/dart"));
        assert_eq!(program_path(base, "v1/server"), PathBuf::from("/work/php/v1/server"));
    }

    #[test]
    fn names_and_repositories() {
        assert!(npm_name("intelephense").is_ok() && npm_name("@zed-industries/vscode-langservers-extracted").is_ok());
        assert!(npm_name("../x").is_err() && npm_name("@a/../b").is_err() && npm_name("--global").is_err());
        assert!(npm_name("a/b").is_err() && npm_name("@a").is_err() && npm_name("").is_err());
        assert_eq!(repo_url("phpactor/phpactor"), Ok("https://api.github.com/repos/phpactor/phpactor".to_string()));
        assert!(repo_url("a").is_err() && repo_url("a/b/c").is_err() && repo_url("../x").is_err() && repo_url("a/b?x").is_err());
    }

    #[test]
    fn github_releases() {
        let value = json!({ "tag_name": "v1", "assets": [{ "name": "x.gz", "browser_download_url": "https://x/x.gz" }] });
        let release = release(&value).unwrap();
        assert_eq!((release.version.as_str(), release.assets[0].name.as_str()), ("v1", "x.gz"));
    }

    #[test]
    fn settings_in_the_apis_shape() {
        let entry = LspSettings {
            settings: Some(json!({ "a": 1 })),
            binary: Some(crate::settings::LspBinary { path: Some("/bin/dart".into()), arguments: None }),
            ..Default::default()
        };
        assert_eq!(
            lsp_settings_json(&entry),
            json!({ "binary": { "path": "/bin/dart", "arguments": null, "env": null }, "initialization_options": null, "settings": { "a": 1 } })
        );
    }

    /// Zed's own extensions, as Zed installed them (copied, never changed):
    /// Dart's server from PATH, HTML's installed with npm, PHP's phpactor
    /// downloaded from GitHub and intelephense from npm. Points the config
    /// folder elsewhere, so it runs alone:
    /// `cargo test runs_zed_extensions -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs Zed's dart, html and php extensions, the network, node and dart; changes XDG_CONFIG_HOME"]
    fn runs_zed_extensions() {
        let zed = super::super::zed_installed_dir().expect("Zed's extensions folder");
        let config = std::env::temp_dir().join(format!("kuana-wasm-zed-{}", std::process::id()));
        // SAFETY: this test runs on its own (it's ignored by default).
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &config) };
        let extensions = super::super::extensions_dir().unwrap();
        std::fs::create_dir_all(&extensions).unwrap();
        let project = config.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let settings = CallSettings::default();
        let extension = |id: &str| -> &'static WasmExtension {
            let dir = extensions.join(id);
            let copied = std::process::Command::new("cp").arg("-R").arg(zed.join(id)).arg(&dir).status().unwrap();
            assert!(copied.success());
            let start = std::time::Instant::now();
            WasmComponent::from_file(&ENGINE, dir.join("extension.wasm")).unwrap();
            eprintln!("{id}: compiled in {:?}", start.elapsed());
            Box::leak(Box::new(WasmExtension { id: id.to_string(), dir }))
        };
        let timed = |extension: &WasmExtension, server: &'static str| {
            let start = std::time::Instant::now();
            let started = extension.start(server, &project, &settings);
            eprintln!("{server}: {:?} in {:?}", started.as_ref().map(|s| &s.command), start.elapsed());
            started
        };

        // Dart: `which`, then the user's `lsp.dart` settings.
        let dart = timed(extension("dart"), "dart").unwrap();
        assert!(dart.command.program.ends_with("dart") && dart.command.args == ["language-server", "--protocol=lsp"], "{dart:?}");
        assert!(dart.settings.is_some());

        // HTML: npm into its work folder, run by node (an absolute path from
        // its current directory, which must be the work folder).
        let html = extension("html");
        let started = timed(html, "vscode-html-language-server").unwrap();
        let work = work_dir("html").unwrap();
        assert_eq!(started.command.program.file_name().unwrap(), "node", "{started:?}");
        assert!(Path::new(&started.command.args[0]).starts_with(&work) && Path::new(&started.command.args[0]).is_file(), "{started:?}");
        assert_eq!(started.initialization_options, Some(json!({ "provideFormatter": true })));
        // Asked again, the same instance finds it installed.
        timed(html, "vscode-html-language-server").unwrap();

        // PHP: phpactor.phar from its latest GitHub release, made executable.
        let php = extension("php");
        let phpactor = timed(php, "phpactor").unwrap();
        let program = &phpactor.command.program;
        assert!(program.starts_with(work_dir("php").unwrap()) && program.ends_with("phpactor.phar"), "{phpactor:?}");
        use std::os::unix::fs::PermissionsExt;
        assert!(std::fs::metadata(program).unwrap().permissions().mode() & 0o111 != 0);
        let servers = WasmServers { extension: php, ids: &["intelephense", "phpactor"] };
        let first = servers.start(&project, &settings).unwrap();
        assert_eq!(first.server, "intelephense");
        assert_eq!(servers.additional("vtsls", &project, &settings), (None, None));
        std::fs::remove_dir_all(&config).ok();
    }

    #[test]
    fn response_headers_after_redirects() {
        let path = header_file();
        std::fs::write(&path, "HTTP/2 302\r\nlocation: x\r\n\r\nHTTP/2 200\r\ncontent-type: text/plain\r\nx-a: b: c\r\n\r\n").unwrap();
        assert_eq!(read_headers(&path), [("content-type".to_string(), "text/plain".to_string()), ("x-a".into(), "b: c".into())]);
        std::fs::remove_file(&path).ok();
    }
}
