//! Language servers (LSP): diagnostics, completion, hover, navigation,
//! rename and code actions from the server for each language.
//!
//! An `LspStore` per window starts one server per kind and project root the
//! first time a matching file opens, keeps open documents in sync and routes
//! what servers send back. A document can have several servers (Kin's own,
//! then extensions': ESLint beside TypeScript); they all get its text, and
//! their diagnostics are combined. Editors talk to the servers directly for
//! requests: the first that can answer, or all for completion and code
//! actions; see `Editor::lsp_server_with`.

pub mod install;
pub mod protocol;
pub mod rpc;
pub mod servers;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures::StreamExt;
use gpui::{App, Context, Entity, EventEmitter, Task, WeakEntity};
use ropey::Rope;
use serde_json::{Value, json};

use crate::{
    document::LspChanges,
    editor::Editor,
    settings::Settings,
    syntax::Language,
};
use protocol::{Encoding, TextEdit, path_to_uri};
use rpc::{Client, Incoming, RpcError};

/// Servers are shared by documents of the same kind under the same root.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ServerKey {
    id: String,
    root: PathBuf,
}

enum ServerState {
    /// Looking for the executable and starting it.
    Finding,
    /// Running, waiting for `initialize` to finish; holds its spec in case
    /// it turns out not to be installed after all (rustup's proxy).
    Starting(Arc<Client>, &'static servers::ServerSpec),
    Running(Arc<Server>),
    /// Not installed (Install can fix that when the spec has an installer).
    Missing(&'static servers::ServerSpec),
    Failed(String),
}

/// A running, initialized server.
pub struct Server {
    /// Which server this is (`typescript`, an extension's): unique among a
    /// document's servers, unlike `name`.
    pub id: String,
    /// The executable's name, for the status bar and diagnostics.
    pub name: String,
    pub client: Arc<Client>,
    pub encoding: Encoding,
    capabilities: Value,
    /// `textDocumentSync` kind: 0 none, 1 full text, 2 incremental.
    sync: u64,
}

impl Server {
    /// A capability's options (`codeLensProvider` → `{ "resolveProvider": true }`).
    pub fn capability_value(&self, name: &str) -> &Value {
        self.capabilities.get(name).unwrap_or(&Value::Null)
    }

    /// Whether the server runs `command` through `workspace/executeCommand`.
    pub fn executes(&self, command: &str) -> bool {
        self.capability_value("executeCommandProvider")["commands"].as_array().is_some_and(|c| c.iter().any(|c| c == command))
    }

    pub fn capability(&self, name: &str) -> bool {
        self.capabilities.get(name).is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
    }

    /// Characters that open completion (`.`, `::`, `<`…).
    pub fn completion_triggers(&self) -> Vec<String> {
        self.trigger_list("completionProvider", "triggerCharacters")
    }

    pub fn signature_triggers(&self) -> Vec<String> {
        let mut list = self.trigger_list("signatureHelpProvider", "triggerCharacters");
        list.extend(self.trigger_list("signatureHelpProvider", "retriggerCharacters"));
        list
    }

    fn trigger_list(&self, provider: &str, key: &str) -> Vec<String> {
        self.capabilities
            .get(provider)
            .and_then(|p| p.get(key))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default()
    }

    pub fn resolves(&self, provider: &str) -> bool {
        self.capabilities.get(provider).and_then(|p| p.get("resolveProvider")).and_then(Value::as_bool).unwrap_or(false)
    }

    /// Sends a request and waits for its result.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.client.request(method, params).await
    }
}

/// A `$/progress` report, like "Indexing 40% — crate foo".
#[derive(Default, Clone)]
struct Progress {
    title: String,
    message: Option<String>,
    percentage: Option<u64>,
}

impl Progress {
    fn label(&self) -> String {
        let mut label = self.title.clone();
        if let Some(pct) = self.percentage {
            label.push_str(&format!(" {pct}%"));
        }
        if let Some(message) = &self.message {
            label.push_str(" — ");
            label.push_str(message);
        }
        label
    }
}

struct OpenDocument {
    editor: WeakEntity<Editor>,
    /// Its servers, the main one first, and whether each got `didOpen` (a
    /// server may still be starting).
    servers: Vec<(ServerKey, bool)>,
    language_id: &'static str,
}

impl OpenDocument {
    fn has(&self, key: &ServerKey) -> bool {
        self.servers.iter().any(|(k, _)| k == key)
    }

    fn opened(&self, key: &ServerKey) -> bool {
        self.servers.iter().any(|(k, opened)| k == key && *opened)
    }

    fn set_opened(&mut self, key: &ServerKey, opened: bool) {
        for (k, o) in &mut self.servers {
            if k == key {
                *o = opened;
            }
        }
    }
}

/// What the status bar shows for a file's server.
#[derive(Clone, PartialEq, Debug)]
pub enum LspStatus {
    Starting,
    /// Work in progress, e.g. "Indexing 40%".
    Busy(String),
    Ready(String),
    /// Not installed: how to install it by hand, and Kin's installer if it has one.
    Missing { command: &'static str, installer: Option<&'static install::Installer> },
    /// Kin is installing it (this command).
    Installing(&'static str),
    Failed(String),
}

/// One server's diagnostics for a document, as it published them.
pub struct LspDiagnostics {
    /// `Server::id`.
    pub id: String,
    /// `Server::name`, shown as the diagnostics' source.
    pub server: String,
    pub encoding: Encoding,
    pub params: Value,
}

pub enum LspEvent {
    /// A server asked to change files (`workspace/applyEdit`); answer with
    /// [`LspStore::respond`].
    ApplyEdit { key: ServerKey, request_id: Value, encoding: Encoding, edits: Vec<(PathBuf, Vec<TextEdit>)> },
    /// An error message from a server.
    Message(String),
    /// Kin installed this server.
    Installed(&'static str),
}

impl EventEmitter<LspEvent> for LspStore {}

pub struct LspStore {
    workspace_root: Option<PathBuf>,
    servers: HashMap<ServerKey, ServerState>,
    documents: HashMap<PathBuf, OpenDocument>,
    /// Latest diagnostics per file and server, open or not, so a file gets
    /// them when it opens later.
    diagnostics: HashMap<(PathBuf, ServerKey), Value>,
    /// Work-done progress per server, by token.
    progress: HashMap<ServerKey, HashMap<String, Progress>>,
    /// Which start of each server is current: a server replaced by a
    /// restart may still answer, start late or exit, and mustn't touch its
    /// successor's state.
    generations: HashMap<ServerKey, u64>,
    next_generation: u64,
    /// Each started server's spec, program and the configuration it was
    /// given, to answer `workspace/configuration` and notice changes.
    configs: HashMap<ServerKey, (&'static servers::ServerSpec, PathBuf, servers::ServerConfig)>,
    /// Files each server asked to hear about (`workspace/didChangeWatchedFiles`),
    /// by registration id.
    watchers: HashMap<ServerKey, Vec<(String, Vec<FileWatcher>)>>,
    /// Tests: the servers every document gets, instead of its language's.
    #[cfg(test)]
    pub(crate) test_specs: Option<Vec<&'static servers::ServerSpec>>,
    _tasks: Vec<Task<()>>,
}

impl LspStore {
    pub fn new() -> Self {
        Self {
            watchers: HashMap::new(),
            workspace_root: None,
            servers: HashMap::new(),
            documents: HashMap::new(),
            diagnostics: HashMap::new(),
            progress: HashMap::new(),
            configs: HashMap::new(),
            generations: HashMap::new(),
            next_generation: 0,
            #[cfg(test)]
            test_specs: None,
            _tasks: Vec::new(),
        }
    }

    pub fn set_root(&mut self, root: Option<PathBuf>) {
        self.workspace_root = root;
    }

    /// Starts tracking an editor's document with its language's server.
    pub fn register(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let (path, language) = {
            let e = editor.read(cx);
            match e.doc.read(cx).path.clone() {
                // An unloaded tab registers once its file is read.
                Some(path) if e.is_loaded() => (path, e.doc.read(cx).language),
                _ => return,
            }
        };
        if self.documents.get(&path).is_some_and(|d| d.editor.upgrade().as_ref() == Some(editor)) {
            return;
        }
        // Another editor of the same document (a split): the servers know it
        // already, and this one sends its edits through the same store.
        let shown = self.documents.get(&path).and_then(|d| d.editor.upgrade());
        if shown.is_some_and(|shown| shown.read(cx).shares_document(editor.read(cx))) {
            let store = cx.weak_entity();
            editor.update(cx, |e, _| e.set_lsp(store));
            return;
        }
        // Servers run the project's code (build scripts, plugins, its own binaries).
        if !crate::trust::is_trusted(&path) {
            return;
        }
        let keys = self.server_keys(language, &path, cx);
        if keys.is_empty() {
            return;
        }
        let store = cx.weak_entity();
        editor.update(cx, |e, _| e.set_lsp(store));
        self.documents.insert(
            path.clone(),
            OpenDocument {
                editor: editor.downgrade(),
                servers: keys.iter().map(|(key, _, _)| (key.clone(), false)).collect(),
                language_id: servers::language_id(language, &path),
            },
        );
        for (key, custom, spec) in keys {
            match self.servers.get(&key) {
                Some(ServerState::Running(server)) => {
                    let server = server.clone();
                    self.open_document(&path, &key, &server, cx);
                }
                Some(_) => {}
                None => self.start(key, spec, custom, language, path.clone(), cx),
            }
        }
    }

    /// The servers for `language` at `path`, the main one first (see
    /// `servers::specs`), each with a user-configured command if any. None
    /// when turned off in settings; a command there replaces them all.
    #[allow(clippy::type_complexity)]
    fn server_keys(&self, language: Language, path: &Path, cx: &App) -> Vec<(ServerKey, Option<Vec<String>>, &'static servers::ServerSpec)> {
        let custom = match Settings::get(cx).language_servers.get(language.name()) {
            Some(Value::Bool(false)) => return Vec::new(),
            Some(Value::Array(command)) => Some(command.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>()),
            _ => None,
        };
        let workspace_root = self.workspace_root.as_deref();
        let root = |spec: &servers::ServerSpec| {
            // A file outside the project (a dependency Go to Declaration
            // opened): a server of the same kind that's already running
            // knows it, the project's first; starting another would index
            // the dependency on its own.
            if !workspace_root.is_some_and(|r| path.starts_with(r)) {
                let running = self.servers.keys().filter(|k| k.id == spec.id);
                if let Some(key) = running.min_by_key(|k| Some(k.root.as_path()) != workspace_root) {
                    return key.root.clone();
                }
            }
            servers::root_for(spec, path, workspace_root)
        };
        #[cfg(test)]
        if let Some(specs) = &self.test_specs {
            return specs.iter().map(|spec| (ServerKey { id: spec.id.to_string(), root: root(spec) }, None, *spec)).collect();
        }
        // A command in settings works for languages without a server of their own too.
        if let Some(command) = custom {
            let spec = servers::spec(language).unwrap_or(&servers::CUSTOM);
            let key = ServerKey { id: format!("custom:{}", command.join(" ")), root: root(spec) };
            return vec![(key, Some(command), spec)];
        }
        servers::specs(language).into_iter().map(|spec| (ServerKey { id: spec.id.to_string(), root: root(spec) }, None, spec)).collect()
    }

    fn start(
        &mut self,
        key: ServerKey,
        spec: &'static servers::ServerSpec,
        custom: Option<Vec<String>>,
        language: Language,
        file: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.servers.insert(key.clone(), ServerState::Finding);
        self.next_generation += 1;
        let generation = self.next_generation;
        self.generations.insert(key.clone(), generation);
        cx.notify();
        let user = Settings::get(cx).lsp.clone();
        let task = cx.spawn(async move |this, cx| {
            // Finding the executable (and the login shell's PATH) touches the disk.
            let root = key.root.clone();
            let user_for_find = user.clone();
            let spawned = cx
                .background_executor()
                .spawn(async move {
                    // Python servers find the project's packages through `python` on PATH.
                    let venv = (language == Language::Python).then(|| crate::python::venv(file.parent()?)).flatten();
                    let command = match &custom {
                        Some(command) => servers::custom(command),
                        // `lsp.<id>.binary`, else where Kin finds it.
                        None => servers::user_binary(spec, &user_for_find).or_else(|| servers::find(spec, &file, venv.as_deref())),
                    };
                    // Kin's own Node.js runs the npm servers it installed.
                    let path = crate::python::path_with(venv.as_deref(), &install::path_with(install::node_bin().as_deref()));
                    let spawned = command.map(|c| (Client::spawn(&c.program, &c.args, &root, &path), c));
                    if let Some((Ok(_), c)) = &spawned {
                        let program = c.program.clone();
                        std::thread::spawn(move || install::check_for_update(&program));
                    }
                    spawned
                })
                .await;
            // Stopped or restarted meanwhile: this start is history.
            let current = |s: &Self| s.generations.get(&key) == Some(&generation);
            let (client, incoming, command) = match spawned {
                None => {
                    this.update(cx, |s, cx| {
                        if current(s) {
                            s.servers.insert(key.clone(), ServerState::Missing(spec));
                            cx.notify();
                        }
                    })
                    .ok();
                    return;
                }
                Some((Err(err), c)) => {
                    let message = format!("Couldn't start {}: {err}", c.program.display());
                    this.update(cx, |s, cx| {
                        if current(s) {
                            s.fail(&key, message, cx);
                        }
                    })
                    .ok();
                    return;
                }
                Some((Ok((client, incoming)), c)) => (client, incoming, c),
            };
            // Shown in the status bar: "pyright", not "pyright.cmd" on Windows.
            let program = &command.program;
            let windows_ext = program.extension().is_some_and(|e| ["exe", "cmd", "bat"].iter().any(|w| e.eq_ignore_ascii_case(w)));
            let name = if windows_ext { program.file_stem() } else { program.file_name() };
            let name = name.map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let config = servers::config(spec, &command.program, &user);
            let initialization_options = config.initialization_options.clone();
            let started = this.update(cx, |s, cx| {
                if !current(s) {
                    return false;
                }
                s.servers.insert(key.clone(), ServerState::Starting(client.clone(), spec));
                s.configs.insert(key.clone(), (spec, command.program.clone(), config));
                s.listen(key.clone(), generation, incoming, cx);
                true
            });
            if !started.unwrap_or(false) {
                // Dropping the client stops the process.
                return;
            }

            let init = client.request("initialize", initialize_params(&key.root, initialization_options));
            let result = match init.await {
                Ok(result) => result,
                // An exit is reported (and explained) by `Closed`.
                Err(err) if err.message == rpc::STOPPED => return,
                Err(err) => {
                    this.update(cx, |s, cx| {
                        if current(s) {
                            s.fail(&key, format!("{name} failed to start: {err}"), cx);
                        }
                    })
                    .ok();
                    return;
                }
            };
            client.notify("initialized", json!({}));
            // Servers that don't ask (`workspace/configuration`) take their settings this way.
            if let Ok(Some(settings)) = this.update(cx, |s, _| s.configs.get(&key).map(|(_, _, c)| c.settings.clone()))
                && !settings.is_null()
            {
                client.notify("workspace/didChangeConfiguration", json!({ "settings": settings }));
            }
            let capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
            let encoding = match capabilities.get("positionEncoding").and_then(Value::as_str) {
                Some("utf-8") => Encoding::Utf8,
                _ => Encoding::Utf16,
            };
            let sync = match capabilities.get("textDocumentSync") {
                Some(Value::Number(n)) => n.as_u64().unwrap_or(1),
                Some(Value::Object(o)) => o.get("change").and_then(Value::as_u64).unwrap_or(0),
                _ => 0,
            };
            let server = Arc::new(Server { id: key.id.clone(), name, client, encoding, capabilities, sync });
            this.update(cx, |s, cx| {
                if !current(s) {
                    return;
                }
                s.servers.insert(key.clone(), ServerState::Running(server.clone()));
                let waiting: Vec<PathBuf> =
                    s.documents.iter().filter(|(_, d)| d.has(&key) && !d.opened(&key)).map(|(p, _)| p.clone()).collect();
                for path in waiting {
                    s.open_document(&path, &key, &server, cx);
                }
                cx.notify();
            })
            .ok();
        });
        self._tasks.push(task);
    }

    fn fail(&mut self, key: &ServerKey, message: String, cx: &mut Context<Self>) {
        self.servers.insert(key.clone(), ServerState::Failed(message.clone()));
        self.progress.remove(key);
        cx.emit(LspEvent::Message(message));
        cx.notify();
    }

    /// Handles what the server sends on its own until it exits.
    fn listen(&mut self, key: ServerKey, generation: u64, mut incoming: futures::channel::mpsc::UnboundedReceiver<Incoming>, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            while let Some(message) = incoming.next().await {
                let handled = this.update(cx, |s, cx| {
                    // A replaced server's last words (its exit above all) are ignored.
                    if s.generations.get(&key) == Some(&generation) {
                        s.handle(&key, message, cx);
                    }
                });
                if handled.is_err() {
                    break;
                }
            }
        });
        self._tasks.push(task);
    }

    fn client(&self, key: &ServerKey) -> Option<Arc<Client>> {
        match self.servers.get(key)? {
            ServerState::Starting(client, _) => Some(client.clone()),
            ServerState::Running(server) => Some(server.client.clone()),
            _ => None,
        }
    }

    fn handle(&mut self, key: &ServerKey, message: Incoming, cx: &mut Context<Self>) {
        match message {
            Incoming::Notification { method, params } => match method.as_str() {
                "textDocument/publishDiagnostics" => {
                    let Some(path) = params.get("uri").and_then(Value::as_str).and_then(protocol::uri_to_path) else {
                        return;
                    };
                    self.diagnostics.insert((path.clone(), key.clone()), params);
                    self.apply_diagnostics(&path, cx);
                }
                "$/progress" => {
                    let token = match params.get("token") {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => v.to_string(),
                        None => return,
                    };
                    let value = params.get("value").unwrap_or(&Value::Null);
                    let entries = self.progress.entry(key.clone()).or_default();
                    match value.get("kind").and_then(Value::as_str) {
                        Some("end") => {
                            entries.remove(&token);
                        }
                        Some(kind) => {
                            let entry = entries.entry(token).or_default();
                            if kind == "begin" {
                                entry.title = value.get("title").and_then(Value::as_str).unwrap_or_default().to_string();
                            }
                            if let Some(message) = value.get("message").and_then(Value::as_str) {
                                entry.message = Some(message.to_string()).filter(|m| !m.is_empty());
                            }
                            if let Some(pct) = value.get("percentage").and_then(Value::as_u64) {
                                entry.percentage = Some(pct);
                            }
                        }
                        None => {}
                    }
                    cx.notify();
                }
                "window/showMessage" => {
                    // 1 = error; warnings and info are mostly chatter.
                    if params.get("type").and_then(Value::as_u64) == Some(1)
                        && let Some(text) = params.get("message").and_then(Value::as_str)
                    {
                        cx.emit(LspEvent::Message(text.to_string()));
                    }
                }
                _ => {}
            },
            Incoming::Request { id, method, params } => {
                let Some(client) = self.client(key) else { return };
                match method.as_str() {
                    // From `lsp.<id>.settings` (and Kin's defaults); null means "use yours".
                    "workspace/configuration" => {
                        let settings = self.configs.get(key).map_or(Value::Null, |(_, _, c)| c.settings.clone());
                        let items = params.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
                        let answers = items.iter().map(|item| servers::configuration(&settings, item.get("section").and_then(Value::as_str)));
                        client.respond(id, Value::Array(answers.collect()));
                    }
                    "workspace/workspaceFolders" => client.respond(id, workspace_folders(&key.root)),
                    "client/registerCapability" => {
                        for registration in params.get("registrations").and_then(Value::as_array).into_iter().flatten() {
                            if registration.get("method").and_then(Value::as_str) != Some("workspace/didChangeWatchedFiles") {
                                continue;
                            }
                            let id = registration.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                            let watchers = registration.pointer("/registerOptions/watchers").and_then(Value::as_array);
                            let watchers: Vec<FileWatcher> = watchers.into_iter().flatten().filter_map(FileWatcher::parse).collect();
                            let list = self.watchers.entry(key.clone()).or_default();
                            list.retain(|(existing, _)| *existing != id);
                            list.push((id, watchers));
                        }
                        client.respond(id, Value::Null);
                    }
                    "client/unregisterCapability" => {
                        let gone = params.get("unregisterations").and_then(Value::as_array);
                        for item in gone.into_iter().flatten() {
                            let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                            if let Some(list) = self.watchers.get_mut(key) {
                                list.retain(|(existing, _)| existing != id);
                            }
                        }
                        client.respond(id, Value::Null);
                    }
                    "workspace/applyEdit" => {
                        let encoding = match self.servers.get(key) {
                            Some(ServerState::Running(server)) => server.encoding,
                            _ => Encoding::Utf16,
                        };
                        let edits = protocol::workspace_edit(params.get("edit").unwrap_or(&Value::Null));
                        cx.emit(LspEvent::ApplyEdit { key: key.clone(), request_id: id, encoding, edits });
                    }
                    _ => client.respond(id, Value::Null),
                }
            }
            Incoming::Closed { stderr_tail } => {
                self.watchers.remove(key);
                // Stopped on purpose (Restricted Mode, closing): nothing to report.
                if !self.servers.contains_key(key) {
                    return;
                }
                // rustup's proxy runs, but exits when the component is missing.
                if let Some(ServerState::Starting(_, spec)) = self.servers.get(key)
                    && (stderr_tail.contains("Unknown binary") || stderr_tail.contains("is not installed"))
                {
                    let spec = *spec;
                    self.servers.insert(key.clone(), ServerState::Missing(spec));
                    cx.notify();
                    return;
                }
                let detail = stderr_tail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
                let message = if detail.is_empty() {
                    "The language server stopped".to_string()
                } else {
                    format!("The language server stopped: {detail}")
                };
                if !matches!(self.servers.get(key), Some(ServerState::Failed(_))) {
                    self.fail(key, message, cx);
                }
                for doc in self.documents.values_mut() {
                    doc.set_opened(key, false);
                }
            }
        }
    }

    /// Files changed on disk (created, changed or deleted): each server hears
    /// about the ones it watches.
    pub fn files_changed(&self, changes: &[(PathBuf, FileChange)]) {
        for (key, registrations) in &self.watchers {
            let events: Vec<Value> = changes
                .iter()
                .filter(|(path, change)| registrations.iter().flat_map(|(_, w)| w).any(|w| w.matches(path, *change)))
                .map(|(path, change)| json!({ "uri": path_to_uri(path), "type": *change as u8 }))
                .collect();
            if !events.is_empty()
                && let Some(client) = self.client(key)
            {
                client.notify("workspace/didChangeWatchedFiles", json!({ "changes": events }));
            }
        }
    }

    /// Answers a server request (after the workspace applied an edit).
    pub fn respond(&self, key: &ServerKey, request_id: Value, result: Value) {
        if let Some(client) = self.client(key) {
            client.respond(request_id, result);
        }
    }

    /// Gives the editor every running server's diagnostics for `path`.
    fn apply_diagnostics(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(doc) = self.documents.get(path) else { return };
        let sources: Vec<LspDiagnostics> = doc
            .servers
            .iter()
            .filter_map(|(key, _)| {
                let ServerState::Running(server) = self.servers.get(key)? else { return None };
                let params = self.diagnostics.get(&(path.to_path_buf(), key.clone()))?;
                Some(LspDiagnostics { id: server.id.clone(), server: server.name.clone(), encoding: server.encoding, params: params.clone() })
            })
            .collect();
        if sources.is_empty() {
            return;
        }
        let Some(editor) = doc.editor.upgrade() else { return };
        editor.update(cx, |e, cx| e.set_lsp_diagnostics(sources, cx));
    }

    fn open_document(&mut self, path: &Path, key: &ServerKey, server: &Arc<Server>, cx: &mut Context<Self>) {
        let Some(doc) = self.documents.get_mut(path) else { return };
        let Some(editor) = doc.editor.upgrade() else {
            self.documents.remove(path);
            return;
        };
        // The server gets the current text, so earlier edits don't matter
        // (unless another server still has to hear about them).
        let first = !doc.servers.iter().any(|(_, opened)| *opened);
        let (text, version) = editor.update(cx, |e, cx| {
            if first {
                e.doc.as_mut(cx).take_lsp_changes();
            }
            (e.doc.read(cx).rope().to_string(), e.doc.read(cx).version())
        });
        server.client.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": path_to_uri(path), "languageId": doc.language_id, "version": version as i64, "text": text
            } }),
        );
        doc.set_opened(key, true);
        self.apply_diagnostics(path, cx);
    }

    /// The running server for an open document.
    /// Every server that's up, for project-wide requests.
    pub fn running_servers(&self) -> Vec<Arc<Server>> {
        self.servers
            .values()
            .filter_map(|s| match s {
                ServerState::Running(server) => Some(server.clone()),
                _ => None,
            })
            .collect()
    }

    /// The running servers that have an open document, the main one first.
    pub fn servers_for(&self, path: &Path) -> Vec<Arc<Server>> {
        let Some(doc) = self.documents.get(path) else { return Vec::new() };
        doc.servers
            .iter()
            .filter(|(_, opened)| *opened)
            .filter_map(|(key, _)| match self.servers.get(key)? {
                ServerState::Running(server) => Some(server.clone()),
                _ => None,
            })
            .collect()
    }

    /// Sends an editor's edits since the last call to each of its servers.
    pub fn did_change(&self, path: &Path, version: u64, changes: LspChanges, rope: &Rope) {
        if changes.is_empty() {
            return;
        }
        for server in self.servers_for(path) {
            Self::send_changes(&server, path, version, &changes, rope);
        }
    }

    fn send_changes(server: &Server, path: &Path, version: u64, changes: &LspChanges, rope: &Rope) {
        if server.sync == 0 {
            return;
        }
        let content_changes: Vec<Value> = if changes.full || server.sync == 1 {
            vec![json!({ "text": rope.to_string() })]
        } else {
            let col = |(_, utf8, utf16): (u32, u32, u32)| if server.encoding == Encoding::Utf8 { utf8 } else { utf16 };
            changes
                .changes
                .iter()
                .map(|c| {
                    json!({
                        "range": {
                            "start": { "line": c.start.0, "character": col(c.start) },
                            "end": { "line": c.end.0, "character": col(c.end) },
                        },
                        "text": c.text,
                    })
                })
                .collect()
        };
        server.client.notify(
            "textDocument/didChange",
            json!({ "textDocument": { "uri": path_to_uri(path), "version": version as i64 }, "contentChanges": content_changes }),
        );
    }

    pub fn did_save(&self, path: &Path, rope: &Rope) {
        for server in self.servers_for(path) {
            Self::send_save(&server, path, rope);
        }
    }

    fn send_save(server: &Server, path: &Path, rope: &Rope) {
        let save = server.capabilities.get("textDocumentSync").and_then(|s| s.get("save"));
        if save.is_none_or(|s| s == &Value::Bool(false)) {
            return;
        }
        let include_text = save.and_then(|s| s.get("includeText")).and_then(Value::as_bool).unwrap_or(false);
        let mut params = json!({ "textDocument": { "uri": path_to_uri(path) } });
        if include_text {
            params["text"] = Value::String(rope.to_string());
        }
        server.client.notify("textDocument/didSave", params);
    }

    /// Stops tracking a closed tab's document.
    /// The editor the store tracks `path` through is closing while another
    /// editor of the same document stays: track it through that one.
    pub fn rebind(&mut self, path: &Path, editor: &Entity<Editor>) {
        if let Some(doc) = self.documents.get_mut(path) {
            doc.editor = editor.downgrade();
        }
    }

    pub fn did_close(&mut self, path: &Path) {
        let Some(doc) = self.documents.remove(path) else { return };
        for (key, _) in doc.servers.iter().filter(|(_, opened)| *opened) {
            if let Some(client) = self.client(key) {
                client.notify("textDocument/didClose", json!({ "textDocument": { "uri": path_to_uri(path) } }));
            }
        }
    }

    /// An editor's file got another path (rename, move, Save As): the server
    /// closes the old URI and opens the new one (a new name can also mean
    /// another language and server).
    pub fn moved(&mut self, old: &Path, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        if editor.read(cx).doc.read(cx).path.as_deref() != Some(old) {
            self.did_close(old);
        }
        self.register(editor, cx);
    }

    /// Installs the missing server of `path`'s document; when it's done,
    /// every window starts the servers it was missing.
    pub fn install_server(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(doc) = self.documents.get(path) else { return };
        let Some(spec) = doc.servers.iter().find_map(|(key, _)| match self.servers.get(key) {
            Some(ServerState::Missing(spec)) => Some(*spec),
            _ => None,
        }) else {
            return;
        };
        let Some(installer) = install::for_spec(spec) else { return };
        if install::installing(installer.command) {
            return;
        }
        let task = cx.background_executor().spawn(async move { install::install(installer) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |_, cx| match result {
                Ok(()) => {
                    cx.emit(LspEvent::Installed(installer.command));
                    install::installed_now(cx);
                }
                Err(err) => {
                    cx.emit(LspEvent::Message(format!("Couldn't install {}: {err}", installer.command)));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        // Shown as Installing from now on (`status`).
        cx.notify();
    }

    /// Starts again every server that was missing (one was installed).
    pub fn retry_missing(&mut self, cx: &mut Context<Self>) {
        let missing: Vec<ServerKey> = self.servers.iter().filter(|(_, s)| matches!(s, ServerState::Missing(_))).map(|(k, _)| k.clone()).collect();
        for key in &missing {
            self.servers.remove(key);
        }
        self.reregister(|d| missing.iter().any(|key| d.has(key)), cx);
        cx.notify();
    }

    /// Settings changed: running servers whose settings differ now are told
    /// (initialization options and programs apply when a server restarts).
    pub fn settings_changed(&mut self, cx: &App) {
        let user = &Settings::get(cx).lsp;
        for (key, (spec, program, config)) in &mut self.configs {
            let fresh = servers::config(spec, program, user);
            if fresh.settings == config.settings {
                continue;
            }
            config.settings = fresh.settings;
            if let Some(ServerState::Running(server)) = self.servers.get(key) {
                server.client.notify("workspace/didChangeConfiguration", json!({ "settings": config.settings }));
            }
        }
    }

    /// Registers again the documents `which` picks (their servers changed).
    fn reregister(&mut self, which: impl Fn(&OpenDocument) -> bool, cx: &mut Context<Self>) {
        let editors: Vec<Entity<Editor>> = self.documents.values().filter(|d| which(d)).filter_map(|d| d.editor.upgrade()).collect();
        self.documents.retain(|_, d| !which(d));
        for editor in editors {
            self.register(&editor, cx);
        }
    }

    /// Restarts a document's servers (after installing one, or if it's stuck).
    pub fn restart(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(keys) = self.documents.get(path).map(|d| d.servers.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()) else { return };
        for key in &keys {
            if let Some(client) = self.client(key) {
                std::thread::spawn(move || client.shutdown());
            }
            self.servers.remove(key);
            self.progress.remove(key);
            self.configs.remove(key);
            self.generations.remove(key);
        }
        self.reregister(|d| keys.iter().any(|key| d.has(key)), cx);
    }

    /// The main server's state for the status bar; when it's up, the names
    /// of all that are ("typescript-language-server, eslint").
    pub fn status(&self, path: &Path) -> Option<LspStatus> {
        let doc = self.documents.get(path)?;
        let (main, _) = doc.servers.first()?;
        let names = || {
            let names: Vec<String> = doc
                .servers
                .iter()
                .filter_map(|(key, _)| match self.servers.get(key)? {
                    ServerState::Running(server) => Some(server.name.clone()),
                    _ => None,
                })
                .collect();
            names.join(", ")
        };
        Some(match self.servers.get(main)? {
            ServerState::Finding | ServerState::Starting(..) => LspStatus::Starting,
            ServerState::Running(server) => match doc.servers.iter().find_map(|(key, _)| self.progress.get(key)?.values().next()) {
                Some(work) => LspStatus::Busy(format!("{}: {}", server.name, work.label())),
                None => LspStatus::Ready(names()),
            },
            ServerState::Missing(spec) => match install::for_spec(spec) {
                Some(installer) if install::installing(installer.command) => LspStatus::Installing(installer.command),
                installer => LspStatus::Missing { command: spec.install, installer },
            },
            ServerState::Failed(message) => LspStatus::Failed(message.clone()),
        })
    }

    /// Stops every server (window closing or app quitting).
    pub fn shutdown_all(&mut self) {
        let clients: Vec<Arc<Client>> = self.servers.keys().cloned().collect::<Vec<_>>().iter().filter_map(|k| self.client(k)).collect();
        self.servers.clear();
        self.configs.clear();
        self.generations.clear();
        self.documents.clear();
        if !clients.is_empty() {
            std::thread::spawn(move || clients.iter().for_each(|c| c.shutdown()));
        }
    }
}

fn workspace_folders(root: &Path) -> Value {
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    json!([{ "uri": path_to_uri(root), "name": name }])
}

/// What Kin tells servers it can do.
fn initialize_params(root: &Path, initialization_options: Value) -> Value {
    let mut params = json!({
        "processId": std::process::id(),
        "clientInfo": { "name": "Kin", "version": env!("CARGO_PKG_VERSION") },
        "rootUri": path_to_uri(root),
        "rootPath": root.to_string_lossy(),
        "workspaceFolders": workspace_folders(root),
        "capabilities": {
            "general": { "positionEncodings": ["utf-8", "utf-16"] },
            "workspace": {
                "applyEdit": true,
                "workspaceEdit": { "documentChanges": true },
                "configuration": true,
                "workspaceFolders": true,
                "didChangeWatchedFiles": { "dynamicRegistration": true, "relativePatternSupport": true },
            },
            "window": { "workDoneProgress": true },
            "textDocument": {
                "synchronization": { "didSave": true },
                "completion": {
                    "completionItem": {
                        "snippetSupport": true,
                        "labelDetailsSupport": true,
                        "resolveSupport": { "properties": ["additionalTextEdits", "detail", "documentation"] },
                    },
                    "contextSupport": true,
                },
                "hover": { "contentFormat": ["markdown", "plaintext"] },
                "signatureHelp": {
                    "signatureInformation": {
                        "parameterInformation": { "labelOffsetSupport": true },
                        "activeParameterSupport": true,
                    },
                },
                "definition": { "linkSupport": true },
                "references": {},
                "rename": { "prepareSupport": false },
                "codeAction": {
                    "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [
                        "", "quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite",
                        "source", "source.organizeImports",
                    ] } },
                    "resolveSupport": { "properties": ["edit"] },
                    "dataSupport": true,
                    "isPreferredSupport": true,
                },
                "publishDiagnostics": {},
                "inlayHint": {},
                "codeLens": {},
            },
            // Commands Kin runs itself, so servers send code lenses that use
            // them (rust-analyzer's Run, Debug and references lenses).
            "experimental": { "commands": { "commands": CLIENT_COMMANDS } },
        },
    });
    if !initialization_options.is_null() {
        params["initializationOptions"] = initialization_options;
    }
    params
}

/// Code lens commands the editor carries out (see `Editor::run_code_lens`).
pub const CLIENT_COMMANDS: &[&str] = &[
    "rust-analyzer.runSingle",
    "rust-analyzer.debugSingle",
    "rust-analyzer.showReferences",
    "rust-analyzer.gotoLocation",
    "editor.action.showReferences",
];

impl Default for LspStore {
    fn default() -> Self {
        Self::new()
    }
}

/// How a file changed on disk, as `workspace/didChangeWatchedFiles` numbers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileChange {
    Created = 1,
    Changed = 2,
    Deleted = 3,
}

/// One of a server's file watchers: a glob, relative to `base` when given
/// (else matched against the whole path), and which changes it wants.
struct FileWatcher {
    base: Option<PathBuf>,
    glob: globset::GlobMatcher,
    /// Bits: 1 created, 2 changed, 4 deleted.
    kind: u64,
}

impl FileWatcher {
    fn parse(watcher: &Value) -> Option<Self> {
        let pattern = watcher.get("globPattern")?;
        let (base, glob) = match pattern {
            Value::String(glob) => (None, glob.as_str()),
            // A RelativePattern: a base folder (a URI or a workspace folder) and a glob.
            _ => {
                let base = pattern.get("baseUri").and_then(|b| b.as_str().or_else(|| b.get("uri")?.as_str()))?;
                (Some(protocol::uri_to_path(base)?), pattern.get("pattern")?.as_str()?)
            }
        };
        let glob = globset::GlobBuilder::new(glob).literal_separator(true).build().ok()?.compile_matcher();
        Some(Self { base, glob, kind: watcher.get("kind").and_then(Value::as_u64).unwrap_or(7) })
    }

    fn matches(&self, path: &Path, change: FileChange) -> bool {
        let bit = match change {
            FileChange::Created => 1,
            FileChange::Changed => 2,
            FileChange::Deleted => 4,
        };
        if self.kind & bit == 0 {
            return false;
        }
        match &self.base {
            Some(base) => path.strip_prefix(base).is_ok_and(|rel| self.glob.is_match(rel)),
            None => self.glob.is_match(path),
        }
    }
}

/// A language server for tests: `python3 -c FAKE_SERVER <name> <trigger chars>`.
/// It publishes one diagnostic per opened file ("<name> problem", followed
/// by its initialization option `greeting` and the `mode` in its `fake`
/// settings section when it has them), completes with "<name>-item", offers "<name> fix
/// (<diagnostics sent>)", puts every declaration at line 0, column 3, and finds two usages.
#[cfg(test)]
pub(crate) const FAKE_SERVER: &str = r#"
import sys, json
name = sys.argv[1]
triggers = [t for t in sys.argv[2].split(",") if t] if len(sys.argv) > 2 else []
def read():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        line = line.strip()
        if not line:
            break
        k, v = line.split(b":", 1)
        headers[k.lower()] = v.strip()
    return json.loads(sys.stdin.buffer.read(int(headers[b"content-length"])))
def send(msg):
    body = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()
init = {}
last_uri = None
while True:
    m = read()
    method, id = m.get("method"), m.get("id")
    if method is None:
        continue
    if method == "initialized":
        # Watches TOML files, and tells (as a diagnostic) what it hears of them.
        watchers = [{"globPattern": "**/*.toml"}]
        send({"jsonrpc": "2.0", "id": "reg", "method": "client/registerCapability", "params": {"registrations": [{"id": "w", "method": "workspace/didChangeWatchedFiles", "registerOptions": {"watchers": watchers}}]}})
    elif method == "workspace/didChangeWatchedFiles" and last_uri:
        heard = ", ".join("%d %s" % (c["type"], c["uri"].rsplit("/", 1)[-1]) for c in m["params"]["changes"])
        problem = {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "severity": 3, "message": name + " watched " + heard}
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": last_uri, "diagnostics": [problem]}})
    elif method == "initialize":
        init = m["params"].get("initializationOptions") or {}
        caps = {"textDocumentSync": 1, "completionProvider": {"triggerCharacters": triggers}, "codeActionProvider": True, "codeLensProvider": {"resolveProvider": True}, "definitionProvider": True, "referencesProvider": True}
        send({"jsonrpc": "2.0", "id": id, "result": {"capabilities": caps}})
    elif method == "textDocument/didOpen":
        uri = m["params"]["textDocument"]["uri"]
        last_uri = uri
        # Asks for its whole section and reads `mode` in it, as basedpyright
        # asks for "basedpyright" and reads its "analysis".
        send({"jsonrpc": "2.0", "id": "cfg", "method": "workspace/configuration", "params": {"items": [{"section": "fake"}]}})
        answer = read()
        while answer.get("id") != "cfg":
            answer = read()
        section = answer["result"][0] or {}
        extra = [str(v) for v in (init.get("greeting"), section.get("mode")) if v is not None]
        message = " ".join([name + " problem"] + extra)
        problem = {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "severity": 1, "message": message}
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": [problem]}})
    elif method == "textDocument/codeLens":
        # Unresolved: the title comes with codeLens/resolve.
        lens_range = {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}
        send({"jsonrpc": "2.0", "id": id, "result": [{"range": lens_range, "data": 1}]})
    elif method == "codeLens/resolve":
        lens = m["params"]
        uri = last_uri or ""
        location = {"uri": uri, "range": lens["range"]}
        lens["command"] = {"title": name + " lens", "command": "editor.action.showReferences", "arguments": [uri, lens["range"]["start"], [location]]}
        send({"jsonrpc": "2.0", "id": id, "result": lens})
    elif method == "textDocument/definition":
        uri = m["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "id": id, "result": {"uri": uri, "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 4}}}})
    elif method == "textDocument/references":
        uri = m["params"]["textDocument"]["uri"]
        at = lambda line, ch: {"uri": uri, "range": {"start": {"line": line, "character": ch}, "end": {"line": line, "character": ch + 1}}}
        send({"jsonrpc": "2.0", "id": id, "result": [at(0, 3), at(1, 9)]})
    elif method == "textDocument/completion":
        send({"jsonrpc": "2.0", "id": id, "result": [{"label": name + "-item"}]})
    elif method == "textDocument/codeAction":
        sent = len(m["params"]["context"]["diagnostics"])
        send({"jsonrpc": "2.0", "id": id, "result": [{"title": "%s fix (%d)" % (name, sent)}]})
    elif method == "exit":
        sys.exit(0)
    elif id is not None:
        send({"jsonrpc": "2.0", "id": id, "result": None})
"#;

/// A spec that runs [`FAKE_SERVER`] (leaked; tests only).
#[cfg(test)]
pub(crate) fn fake_server(name: &str, triggers: &str) -> &'static servers::ServerSpec {
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    let args: &'static [&'static str] = Box::leak(vec!["-c", FAKE_SERVER, leak(name.to_string()), leak(triggers.to_string())].into_boxed_slice());
    let candidates: &'static [(&'static str, &'static [&'static str])] = Box::leak(vec![("python3", args)].into_boxed_slice());
    Box::leak(Box::new(servers::ServerSpec { id: leak(format!("fake-{name}")), candidates, ..servers::SPEC }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn files_outside_the_project_use_its_server(cx: &mut gpui::TestAppContext) {
        let spec = fake_server("outside", "");
        let mut store = LspStore::new();
        store.test_specs = Some(vec![spec]);
        store.set_root(Some(PathBuf::from("/work/project")));
        let root_of = |store: &LspStore, path: &str, cx: &mut gpui::TestAppContext| {
            cx.update(|cx| store.server_keys(Language::Rust, Path::new(path), cx))[0].0.root.clone()
        };
        // Nothing running: a dependency gets a root of its own.
        assert_eq!(root_of(&store, "/deps/serde/src/lib.rs", cx), PathBuf::from("/deps/serde/src"));
        // The project's server is running: the dependency goes to it.
        store.servers.insert(ServerKey { id: spec.id.to_string(), root: PathBuf::from("/work/project") }, ServerState::Finding);
        assert_eq!(root_of(&store, "/deps/serde/src/lib.rs", cx), PathBuf::from("/work/project"));
        assert_eq!(root_of(&store, "/work/project/src/main.rs", cx), PathBuf::from("/work/project"));
    }

    #[test]
    fn advertises_both_encodings() {
        let params = initialize_params(Path::new("/w"), json!({ "a": 1 }));
        assert_eq!(params["initializationOptions"], json!({ "a": 1 }));
        assert!(initialize_params(Path::new("/w"), Value::Null).get("initializationOptions").is_none());
        assert_eq!(params["capabilities"]["general"]["positionEncodings"], json!(["utf-8", "utf-16"]));
        assert_eq!(params["rootUri"], "file:///w");
    }
}
