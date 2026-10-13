//! Language servers (LSP): diagnostics, completion, hover, navigation,
//! rename and code actions from the server for each language.
//!
//! An `LspStore` per window starts one server per kind and project root the
//! first time a matching file opens, keeps open documents in sync and routes
//! what servers send back. Editors talk to their document's [`Server`]
//! directly for requests; see `Editor::lsp_request`.

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
    /// Running, waiting for `initialize` to finish; holds the install
    /// command in case it turns out not to be installed after all.
    Starting(Arc<Client>, &'static str),
    Running(Arc<Server>),
    /// Not installed; holds the install command.
    Missing(&'static str),
    Failed(String),
}

/// A running, initialized server.
pub struct Server {
    /// The executable's name, for the status bar and diagnostics.
    pub name: String,
    pub client: Arc<Client>,
    pub encoding: Encoding,
    capabilities: Value,
    /// `textDocumentSync` kind: 0 none, 1 full text, 2 incremental.
    sync: u64,
}

impl Server {
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
        match self.client.request(method, params).await {
            Ok(result) => result,
            Err(_) => Err(RpcError { message: rpc::STOPPED.into() }),
        }
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
    server: ServerKey,
    language_id: &'static str,
    /// `didOpen` was sent (the server may still be starting).
    opened: bool,
}

/// What the status bar shows for a file's server.
#[derive(Clone, PartialEq, Debug)]
pub enum LspStatus {
    Starting,
    /// Work in progress, e.g. "Indexing 40%".
    Busy(String),
    Ready(String),
    Missing(&'static str),
    Failed(String),
}

pub enum LspEvent {
    /// A server asked to change files (`workspace/applyEdit`); answer with
    /// [`LspStore::respond`].
    ApplyEdit { key: ServerKey, request_id: Value, encoding: Encoding, edits: Vec<(PathBuf, Vec<TextEdit>)> },
    /// An error message from a server.
    Message(String),
}

impl EventEmitter<LspEvent> for LspStore {}

pub struct LspStore {
    workspace_root: Option<PathBuf>,
    servers: HashMap<ServerKey, ServerState>,
    documents: HashMap<PathBuf, OpenDocument>,
    /// Latest diagnostics per file, open or not, so a file gets them when it
    /// opens later.
    diagnostics: HashMap<PathBuf, Value>,
    /// Work-done progress per server, by token.
    progress: HashMap<ServerKey, HashMap<String, Progress>>,
    _tasks: Vec<Task<()>>,
}

impl LspStore {
    pub fn new() -> Self {
        Self {
            workspace_root: None,
            servers: HashMap::new(),
            documents: HashMap::new(),
            diagnostics: HashMap::new(),
            progress: HashMap::new(),
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
            match e.doc.path.clone() {
                Some(path) => (path, e.doc.language),
                None => return,
            }
        };
        if self.documents.get(&path).is_some_and(|d| d.editor.upgrade().as_ref() == Some(editor)) {
            return;
        }
        let Some((key, custom)) = self.server_key(language, &path, cx) else { return };
        let store = cx.weak_entity();
        editor.update(cx, |e, _| e.set_lsp(store));
        self.documents.insert(
            path.clone(),
            OpenDocument {
                editor: editor.downgrade(),
                server: key.clone(),
                language_id: servers::language_id(language, &path),
                opened: false,
            },
        );
        match self.servers.get(&key) {
            Some(ServerState::Running(server)) => {
                let server = server.clone();
                self.open_document(&path, &server, cx);
            }
            Some(_) => {}
            None => self.start(key, language, custom, path, cx),
        }
    }

    /// Which server handles `language` for `path`, and a user-configured
    /// command if any. None when disabled or there is no server.
    fn server_key(&self, language: Language, path: &Path, cx: &App) -> Option<(ServerKey, Option<Vec<String>>)> {
        let spec = servers::spec(language)?;
        let root = servers::root_for(spec, path, self.workspace_root.as_deref());
        match Settings::get(cx).language_servers.get(language.name()) {
            Some(Value::Bool(false)) => None,
            Some(Value::Array(command)) => {
                let command: Vec<String> = command.iter().filter_map(Value::as_str).map(str::to_string).collect();
                Some((ServerKey { id: format!("custom:{}", command.join(" ")), root }, Some(command)))
            }
            _ => Some((ServerKey { id: spec.id.to_string(), root }, None)),
        }
    }

    fn start(&mut self, key: ServerKey, language: Language, custom: Option<Vec<String>>, file: PathBuf, cx: &mut Context<Self>) {
        let Some(spec) = servers::spec(language) else { return };
        self.servers.insert(key.clone(), ServerState::Finding);
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            // Finding the executable (and the login shell's PATH) touches the disk.
            let root = key.root.clone();
            let spawned = cx
                .background_executor()
                .spawn(async move {
                    let command = match &custom {
                        Some(command) => servers::custom(command),
                        None => servers::find(spec, &file),
                    };
                    command.map(|c| (Client::spawn(&c.program, &c.args, &root, servers::search_path()), c))
                })
                .await;
            let (client, incoming, command) = match spawned {
                None => {
                    this.update(cx, |s, cx| {
                        s.servers.insert(key.clone(), ServerState::Missing(spec.install));
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                Some((Err(err), c)) => {
                    let message = format!("Couldn't start {}: {err}", c.program.display());
                    this.update(cx, |s, cx| s.fail(&key, message, cx)).ok();
                    return;
                }
                Some((Ok((client, incoming)), c)) => (client, incoming, c),
            };
            let name = command.program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            this.update(cx, |s, cx| {
                s.servers.insert(key.clone(), ServerState::Starting(client.clone(), spec.install));
                s.listen(key.clone(), incoming, cx);
            })
            .ok();

            let init = client.request("initialize", initialize_params(&key.root));
            let result = match init.await {
                Ok(Ok(result)) => result,
                // An exit is reported (and explained) by `Closed`.
                Ok(Err(err)) if err.message == rpc::STOPPED => return,
                Ok(Err(err)) => {
                    this.update(cx, |s, cx| s.fail(&key, format!("{name} failed to start: {err}"), cx)).ok();
                    return;
                }
                // The reader reports the exit; `Closed` marks it failed.
                Err(_) => return,
            };
            client.notify("initialized", json!({}));
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
            let server = Arc::new(Server { name, client, encoding, capabilities, sync });
            this.update(cx, |s, cx| {
                s.servers.insert(key.clone(), ServerState::Running(server.clone()));
                let waiting: Vec<PathBuf> =
                    s.documents.iter().filter(|(_, d)| d.server == key && !d.opened).map(|(p, _)| p.clone()).collect();
                for path in waiting {
                    s.open_document(&path, &server, cx);
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
    fn listen(&mut self, key: ServerKey, mut incoming: futures::channel::mpsc::UnboundedReceiver<Incoming>, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            while let Some(message) = incoming.next().await {
                if this.update(cx, |s, cx| s.handle(&key, message, cx)).is_err() {
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
                    self.diagnostics.insert(path.clone(), params);
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
                    // Kin has no per-server settings; null means "use your defaults".
                    "workspace/configuration" => {
                        let count = params.get("items").and_then(Value::as_array).map_or(0, Vec::len);
                        client.respond(id, Value::Array(vec![Value::Null; count]));
                    }
                    "workspace/workspaceFolders" => client.respond(id, workspace_folders(&key.root)),
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
                // rustup's proxy runs, but exits when the component is missing.
                if let Some(ServerState::Starting(_, install)) = self.servers.get(key)
                    && (stderr_tail.contains("Unknown binary") || stderr_tail.contains("is not installed"))
                {
                    let install = *install;
                    self.servers.insert(key.clone(), ServerState::Missing(install));
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
                for doc in self.documents.values_mut().filter(|d| &d.server == key) {
                    doc.opened = false;
                }
            }
        }
    }

    /// Answers a server request (after the workspace applied an edit).
    pub fn respond(&self, key: &ServerKey, request_id: Value, result: Value) {
        if let Some(client) = self.client(key) {
            client.respond(request_id, result);
        }
    }

    fn apply_diagnostics(&mut self, path: &Path, cx: &mut Context<Self>) {
        let (Some(params), Some(doc)) = (self.diagnostics.get(path), self.documents.get(path)) else { return };
        let Some(ServerState::Running(server)) = self.servers.get(&doc.server) else { return };
        let (encoding, name) = (server.encoding, server.name.clone());
        let Some(editor) = doc.editor.upgrade() else { return };
        let params = params.clone();
        editor.update(cx, |e, cx| e.set_lsp_diagnostics(&params, encoding, &name, cx));
    }

    fn open_document(&mut self, path: &Path, server: &Arc<Server>, cx: &mut Context<Self>) {
        let Some(doc) = self.documents.get_mut(path) else { return };
        let Some(editor) = doc.editor.upgrade() else {
            self.documents.remove(path);
            return;
        };
        // The server gets the current text, so earlier edits don't matter.
        let (text, version) = editor.update(cx, |e, _| {
            e.doc.take_lsp_changes();
            (e.doc.rope().to_string(), e.doc.version())
        });
        server.client.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": path_to_uri(path), "languageId": doc.language_id, "version": version as i64, "text": text
            } }),
        );
        doc.opened = true;
        self.apply_diagnostics(path, cx);
    }

    /// The running server for an open document.
    pub fn server_for(&self, path: &Path) -> Option<Arc<Server>> {
        let doc = self.documents.get(path).filter(|d| d.opened)?;
        match self.servers.get(&doc.server)? {
            ServerState::Running(server) => Some(server.clone()),
            _ => None,
        }
    }

    /// Sends an editor's edits since the last call.
    pub fn did_change(&self, path: &Path, version: u64, changes: LspChanges, rope: &Rope) {
        let Some(server) = self.server_for(path) else { return };
        if server.sync == 0 || changes.is_empty() {
            return;
        }
        let content_changes: Vec<Value> = if changes.full || server.sync == 1 {
            vec![json!({ "text": rope.to_string() })]
        } else {
            let col = |(_, utf8, utf16): (u32, u32, u32)| if server.encoding == Encoding::Utf8 { utf8 } else { utf16 };
            changes
                .changes
                .into_iter()
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
        let Some(server) = self.server_for(path) else { return };
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
    pub fn did_close(&mut self, path: &Path) {
        let Some(doc) = self.documents.remove(path) else { return };
        if doc.opened
            && let Some(client) = self.client(&doc.server)
        {
            client.notify("textDocument/didClose", json!({ "textDocument": { "uri": path_to_uri(path) } }));
        }
    }

    /// Restarts the server for a document (after installing it, or if it's stuck).
    pub fn restart(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(key) = self.documents.get(path).map(|d| d.server.clone()) else { return };
        if let Some(client) = self.client(&key) {
            std::thread::spawn(move || client.shutdown());
        }
        self.servers.remove(&key);
        self.progress.remove(&key);
        let editors: Vec<Entity<Editor>> = self
            .documents
            .iter()
            .filter(|(_, d)| d.server == key)
            .filter_map(|(_, d)| d.editor.upgrade())
            .collect();
        self.documents.retain(|_, d| d.server != key);
        for editor in editors {
            self.register(&editor, cx);
        }
    }

    pub fn status(&self, path: &Path) -> Option<LspStatus> {
        let doc = self.documents.get(path)?;
        Some(match self.servers.get(&doc.server)? {
            ServerState::Finding | ServerState::Starting(..) => LspStatus::Starting,
            ServerState::Running(server) => match self.progress.get(&doc.server).and_then(|p| p.values().next()) {
                Some(work) => LspStatus::Busy(format!("{}: {}", server.name, work.label())),
                None => LspStatus::Ready(server.name.clone()),
            },
            ServerState::Missing(install) => LspStatus::Missing(install),
            ServerState::Failed(message) => LspStatus::Failed(message.clone()),
        })
    }

    /// Stops every server (window closing or app quitting).
    pub fn shutdown_all(&mut self) {
        let clients: Vec<Arc<Client>> = self.servers.keys().cloned().collect::<Vec<_>>().iter().filter_map(|k| self.client(k)).collect();
        self.servers.clear();
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
fn initialize_params(root: &Path) -> Value {
    json!({
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
            },
        },
    })
}

impl Default for LspStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_both_encodings() {
        let params = initialize_params(Path::new("/w"));
        assert_eq!(params["capabilities"]["general"]["positionEncodings"], json!(["utf-8", "utf-16"]));
        assert_eq!(params["rootUri"], "file:///w");
    }
}
