//! Debugging a run target: a session with a debug adapter (see `crate::dap`
//! and `launch`) and the Debug panel that shows it: the stepping toolbar,
//! the paused thread's frames, variables and watches, and a console that
//! also evaluates expressions. The workspace starts sessions, owns the
//! breakpoints and shows the line the program stopped on.

pub mod launch;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use futures::StreamExt as _;
use gpui::{
    AnyElement, App, AsyncApp, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    IntoElement, Render, ScrollHandle, SharedString, Task, WeakEntity, Window, div, font, prelude::*, px,
};
use serde_json::{Value, json};

use crate::{
    assets::Icon,
    dap::{DapClient, Incoming},
    editor::{Editor, Newline},
    run::{DebugSpec, RunTarget},
    theme::Theme,
    ui::{icon_el, input_box, tooltip_view},
};

pub enum DebugEvent {
    /// Paused here, or another frame was picked: show this line (1-based).
    Stopped { path: PathBuf, line: u32 },
    /// Running again: no current line.
    Resumed,
    Ended,
    /// The panel's close button.
    Hide,
}

#[derive(Clone, PartialEq)]
enum Status {
    Building,
    Starting,
    Running,
    /// Why it paused, as shown ("Paused at a breakpoint").
    Paused(String),
    Ended(String),
}

struct Frame {
    id: i64,
    name: String,
    path: Option<PathBuf>,
    line: u32,
}

#[derive(Clone)]
struct Var {
    name: String,
    value: String,
    ty: Option<String>,
    /// Children to ask for (0: none).
    reference: i64,
}

#[derive(Clone, Copy, PartialEq)]
enum Line {
    Output,
    Error,
    /// Kin's own notes (build progress, "Process exited").
    Info,
    /// An evaluated expression, as typed.
    Input,
}

/// Keep the console bounded.
const MAX_CONSOLE: usize = 5000;

pub struct DebugSession {
    pub target: RunTarget,
    status: Status,
    client: Option<Arc<DapClient>>,
    /// Set once the adapter said `initialized` and got its breakpoints.
    configured: bool,
    /// Stop was pressed: the end is "Stopped", not "Finished".
    stopping: bool,
    capabilities: Value,
    /// Rows (0-based) per file, as the workspace last sent them.
    breakpoints: HashMap<PathBuf, Vec<usize>>,
    thread: Option<i64>,
    frames: Vec<Frame>,
    frame: usize,
    scopes: Vec<Var>,
    children: HashMap<i64, Vec<Var>>,
    expanded: HashSet<i64>,
    watches: Vec<(String, Option<Result<String, String>>)>,
    console: Vec<(Line, String)>,
    /// The last console line has no line break yet (output comes in pieces).
    console_open: bool,
    console_scroll: ScrollHandle,
    input: Entity<Editor>,
    watch_input: Entity<Editor>,
    build: launch::Build,
    focus_handle: FocusHandle,
    _tasks: Vec<Task<()>>,
}

impl EventEmitter<DebugEvent> for DebugSession {}

impl DebugSession {
    /// Starts debugging `target` (which has a [`DebugSpec`]) with these breakpoints.
    pub fn new(target: RunTarget, breakpoints: HashMap<PathBuf, Vec<usize>>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| Editor::single_line("Evaluate an expression", cx));
        let watch_input = cx.new(|cx| Editor::single_line("Add a watch", cx));
        let build = launch::Build::default();
        let status = if matches!(target.debug, Some(DebugSpec::Cargo { .. })) { Status::Building } else { Status::Starting };
        let task = cx.spawn(async move |this, cx| {
            if let Err(message) = Self::run(this.clone(), cx).await {
                this.update(cx, |s, cx| s.end(message, cx)).ok();
            }
        });
        Self {
            target,
            status,
            client: None,
            configured: false,
            stopping: false,
            capabilities: Value::Null,
            breakpoints,
            thread: None,
            frames: Vec::new(),
            frame: 0,
            scopes: Vec::new(),
            children: HashMap::new(),
            expanded: HashSet::new(),
            watches: Vec::new(),
            console: Vec::new(),
            console_open: false,
            console_scroll: ScrollHandle::new(),
            input,
            watch_input,
            build,
            focus_handle: cx.focus_handle(),
            _tasks: vec![task],
        }
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.status, Status::Ended(_))
    }

    pub fn is_paused(&self) -> bool {
        matches!(self.status, Status::Paused(_))
    }

    /// Build if needed, start the adapter, then handle what it sends.
    async fn run(this: WeakEntity<Self>, cx: &mut AsyncApp) -> Result<(), String> {
        let (target, build) = this.update(cx, |s, _| (s.target.clone(), s.build.clone())).map_err(|_| String::new())?;
        let spec = target.debug.clone().ok_or("This can't be debugged")?;
        let cwd = target.cwd.clone();

        // Rust: build first, showing cargo's progress.
        let mut program = None;
        if let DebugSpec::Cargo { build: args, .. } = &spec {
            this.update(cx, |s, cx| s.note(format!("$ cargo {}", args.join(" ")), cx)).ok();
            let (line_tx, mut lines) = futures::channel::mpsc::unbounded::<String>();
            let (cwd2, args2, build2) = (cwd.clone(), args.clone(), build.clone());
            let built = cx.background_executor().spawn(async move {
                launch::cargo_build(&cwd2, &args2, &build2, move |line| {
                    let _ = line_tx.unbounded_send(line);
                })
            });
            while let Some(line) = lines.next().await {
                this.update(cx, |s, cx| s.push_line(Line::Info, &line, cx)).ok();
            }
            program = Some(built.await?);
            this.update(cx, |s, cx| {
                s.status = Status::Starting;
                cx.notify();
            })
            .ok();
        }

        let (cwd2, spec2) = (cwd.clone(), spec.clone());
        let adapter = cx.background_executor().spawn(async move { launch::adapter(&spec2, &cwd2) }).await?;
        let launch_args = {
            let (cwd2, spec2, program2) = (cwd.clone(), spec.clone(), program.clone());
            cx.background_executor().spawn(async move { launch::launch_arguments(&spec2, &cwd2, program2.as_deref()) }).await
        };
        let id = adapter.id;
        let started = cx
            .background_executor()
            .spawn(async move {
                let mut command = crate::dap::command(&adapter.program, &adapter.args, &cwd);
                match adapter.port {
                    Some(port) => DapClient::spawn_tcp(&mut command, ([127, 0, 0, 1], port).into()),
                    None => DapClient::spawn_stdio(&mut command),
                }
            })
            .await;
        let (client, mut incoming) = started.map_err(|e| format!("Couldn't start the debugger: {e}"))?;
        this.update(cx, |s, cx| {
            s.client = Some(client.clone());
            cx.notify();
        })
        .map_err(|_| String::new())?;

        let capabilities = client.request("initialize", launch::initialize_arguments(id)).await.map_err(|_| "The debugger stopped".to_string())??;
        this.update(cx, |s, _| s.capabilities = capabilities).ok();

        // The launch answer can come after configurationDone, so don't wait for it here.
        let launched = client.request("launch", launch_args);
        let reporter = this.clone();
        cx.spawn(async move |cx| {
            if let Ok(Err(message)) = launched.await {
                reporter.update(cx, |s, cx| s.end(format!("Couldn't start: {message}"), cx)).ok();
            }
        })
        .detach();

        // The session owns the client from here: dropping it (Stop, the end)
        // must end the adapter, so this loop mustn't keep it alive.
        drop(client);
        while let Some(message) = incoming.next().await {
            let alive = this.update(cx, |s, cx| s.handle(message, cx)).is_ok();
            if !alive {
                break;
            }
        }
        Ok(())
    }

    fn handle(&mut self, message: Incoming, cx: &mut Context<Self>) {
        match message {
            Incoming::Event { event, body } => match event.as_str() {
                "initialized" => self.configure(cx),
                "stopped" => {
                    self.status = Status::Paused(pause_reason(&body));
                    self.thread = body["threadId"].as_i64().or(self.thread);
                    self.load_stack(cx);
                }
                "continued" => self.resumed(cx),
                "output" => {
                    let category = body["category"].as_str().unwrap_or("console");
                    if category != "telemetry"
                        && let Some(text) = body["output"].as_str()
                    {
                        let kind = match category {
                            "stderr" => Line::Error,
                            "stdout" => Line::Output,
                            _ => Line::Info,
                        };
                        if category == "console" {
                            let text = without_chatter(text);
                            if !text.is_empty() {
                                self.push_text(kind, &text, cx);
                            }
                        } else {
                            self.push_text(kind, text, cx);
                        }
                    }
                }
                "exited" => {
                    if let Some(code) = body["exitCode"].as_i64() {
                        self.note(format!("Process finished with exit code {code}"), cx);
                    }
                }
                "terminated" => self.end(if self.stopping { "Stopped" } else { "Finished" }.into(), cx),
                _ => {}
            },
            // Kin runs programs itself (no runInTerminal and the like).
            Incoming::Request { seq, command, .. } => {
                if let Some(client) = &self.client {
                    client.respond(seq, &command, false, json!({ "error": { "format": "Not supported by Kin" } }));
                }
            }
            Incoming::Output(line) => self.push_line(Line::Info, &line, cx),
            Incoming::Closed { tail } => {
                if self.is_active() {
                    let tail = tail.trim();
                    if !tail.is_empty() {
                        self.push_text(Line::Error, tail, cx);
                    }
                    self.end("The debugger exited".into(), cx);
                }
            }
        }
    }

    /// Breakpoints, then configurationDone: the program starts running.
    fn configure(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        for (path, rows) in &self.breakpoints {
            let _ = client.request("setBreakpoints", breakpoints_arguments(path, rows));
        }
        if self.capabilities["exceptionBreakpointFilters"].is_array() {
            let _ = client.request("setExceptionBreakpoints", json!({ "filters": [] }));
        }
        let _ = client.request("configurationDone", json!({}));
        self.configured = true;
        self.status = Status::Running;
        cx.notify();
    }

    /// A file's breakpoints changed in the editor.
    pub fn set_breakpoints(&mut self, path: PathBuf, rows: Vec<usize>) {
        if let Some(client) = self.client.as_ref().filter(|_| self.configured) {
            let _ = client.request("setBreakpoints", breakpoints_arguments(&path, &rows));
        }
        self.breakpoints.insert(path, rows);
    }

    fn load_stack(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(thread)) = (self.client.clone(), self.thread) else {
            cx.notify();
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let Ok(Ok(body)) = client.request("stackTrace", json!({ "threadId": thread, "startFrame": 0, "levels": 60 })).await else {
                return;
            };
            let frames: Vec<Frame> = body["stackFrames"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| Frame {
                    id: f["id"].as_i64().unwrap_or(0),
                    name: frame_name(f["name"].as_str().unwrap_or("?")),
                    path: f["source"]["path"].as_str().map(PathBuf::from),
                    line: f["line"].as_u64().unwrap_or(0) as u32,
                })
                .collect();
            this.update(cx, |s, cx| {
                // The first frame in code that exists here, not inside a library.
                let first = frames.iter().position(|f| f.path.as_ref().is_some_and(|p| p.is_file())).unwrap_or(0);
                s.frames = frames;
                s.select_frame(first, cx);
            })
            .ok();
        });
        task.detach();
    }

    fn select_frame(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.frame = ix;
        self.scopes.clear();
        self.children.clear();
        self.expanded.clear();
        let Some(frame) = self.frames.get(ix) else {
            cx.notify();
            return;
        };
        if let Some(path) = frame.path.clone().filter(|p| p.is_file()) {
            cx.emit(DebugEvent::Stopped { path, line: frame.line });
        }
        let Some(client) = self.client.clone() else { return };
        let frame_id = frame.id;
        let task = cx.spawn(async move |this, cx| {
            let Ok(Ok(body)) = client.request("scopes", json!({ "frameId": frame_id })).await else { return };
            let scopes: Vec<Var> = body["scopes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|s| Var {
                    name: s["name"].as_str().unwrap_or("Scope").to_string(),
                    value: String::new(),
                    ty: None,
                    reference: s["variablesReference"].as_i64().unwrap_or(0),
                })
                .collect();
            // Locals (the first cheap scope) open by default.
            let first = body["scopes"].as_array().into_iter().flatten().zip(&scopes).find(|(raw, _)| !raw["expensive"].as_bool().unwrap_or(false));
            let open = first.map(|(_, s)| s.reference).filter(|r| *r > 0);
            this.update(cx, |s, cx| {
                s.scopes = scopes;
                if let Some(reference) = open {
                    s.toggle(reference, cx);
                }
                s.evaluate_watches(cx);
                cx.notify();
            })
            .ok();
        });
        task.detach();
    }

    /// Expands or collapses a variable (or scope), fetching its children once.
    fn toggle(&mut self, reference: i64, cx: &mut Context<Self>) {
        if !self.expanded.insert(reference) {
            self.expanded.remove(&reference);
            cx.notify();
            return;
        }
        if self.children.contains_key(&reference) {
            cx.notify();
            return;
        }
        let Some(client) = self.client.clone() else { return };
        let task = cx.spawn(async move |this, cx| {
            let Ok(Ok(body)) = client.request("variables", json!({ "variablesReference": reference })).await else { return };
            let vars: Vec<Var> = body["variables"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|v| Var {
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    value: v["value"].as_str().unwrap_or("").to_string(),
                    ty: v["type"].as_str().filter(|t| !t.is_empty()).map(str::to_string),
                    reference: v["variablesReference"].as_i64().unwrap_or(0),
                })
                .collect();
            this.update(cx, |s, cx| {
                s.children.insert(reference, vars);
                cx.notify();
            })
            .ok();
        });
        task.detach();
    }

    fn frame_id(&self) -> Option<i64> {
        self.frames.get(self.frame).map(|f| f.id)
    }

    fn evaluate_watches(&mut self, cx: &mut Context<Self>) {
        for ix in 0..self.watches.len() {
            self.evaluate_watch(ix, cx);
        }
    }

    fn evaluate_watch(&mut self, ix: usize, cx: &mut Context<Self>) {
        let (Some(client), Some(frame)) = (self.client.clone(), self.frame_id()) else { return };
        let expression = self.watches[ix].0.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = client.request("evaluate", json!({ "expression": expression, "frameId": frame, "context": "watch" })).await;
            let value = match result {
                Ok(Ok(body)) => Ok(body["result"].as_str().unwrap_or("").to_string()),
                Ok(Err(message)) => Err(message),
                Err(_) => return,
            };
            this.update(cx, |s, cx| {
                if let Some(watch) = s.watches.get_mut(ix).filter(|w| w.0 == expression) {
                    watch.1 = Some(value);
                    cx.notify();
                }
            })
            .ok();
        });
        task.detach();
    }

    /// Console input: evaluates in the selected frame (or globally while running).
    fn evaluate(&mut self, expression: String, cx: &mut Context<Self>) {
        self.push_line(Line::Input, &format!("> {expression}"), cx);
        let Some(client) = self.client.clone() else {
            self.push_line(Line::Error, "Not debugging", cx);
            return;
        };
        let mut args = json!({ "expression": expression, "context": "repl" });
        if let Some(frame) = self.frame_id().filter(|_| self.is_paused()) {
            args["frameId"] = json!(frame);
        }
        let task = cx.spawn(async move |this, cx| {
            let result = client.request("evaluate", args).await;
            this.update(cx, |s, cx| match result {
                Ok(Ok(body)) => s.push_lines(Line::Output, body["result"].as_str().unwrap_or(""), cx),
                Ok(Err(message)) => s.push_lines(Line::Error, &message, cx),
                Err(_) => {}
            })
            .ok();
        });
        task.detach();
    }

    // ---- Controls ----------------------------------------------------------------------

    fn resumed(&mut self, cx: &mut Context<Self>) {
        if self.is_active() {
            self.status = Status::Running;
        }
        self.frames.clear();
        self.scopes.clear();
        self.children.clear();
        self.expanded.clear();
        cx.emit(DebugEvent::Resumed);
        cx.notify();
    }

    /// `continue`, `next`, `stepIn` or `stepOut` on the paused thread.
    fn step(&mut self, command: &'static str, cx: &mut Context<Self>) {
        let (Some(client), Some(thread)) = (self.client.clone(), self.thread) else { return };
        if !self.is_paused() {
            return;
        }
        let _ = client.request(command, json!({ "threadId": thread }));
        self.resumed(cx);
    }

    pub fn resume(&mut self, cx: &mut Context<Self>) {
        self.step("continue", cx);
    }

    pub fn step_over(&mut self, cx: &mut Context<Self>) {
        self.step("next", cx);
    }

    pub fn step_into(&mut self, cx: &mut Context<Self>) {
        self.step("stepIn", cx);
    }

    pub fn step_out(&mut self, cx: &mut Context<Self>) {
        self.step("stepOut", cx);
    }

    pub fn pause(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone().filter(|_| self.status == Status::Running) else { return };
        let known = self.thread;
        let task = cx.spawn(async move |_, _| {
            let thread = match known {
                Some(thread) => thread,
                None => match client.request("threads", json!({})).await {
                    Ok(Ok(body)) => body["threads"][0]["id"].as_i64().unwrap_or(1),
                    _ => return,
                },
            };
            let _ = client.request("pause", json!({ "threadId": thread }));
        });
        task.detach();
    }

    /// Ends the program and the adapter (Stop).
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if !self.is_active() {
            return;
        }
        // A build in progress is just killed.
        if let Some(mut child) = self.build.lock().unwrap().take() {
            let _ = child.kill();
        }
        let Some(client) = self.client.clone() else {
            self.end("Stopped".into(), cx);
            return;
        };
        self.stopping = true;
        let disconnect = client.request("disconnect", json!({ "terminateDebuggee": true, "restart": false }));
        let task = cx.spawn(async move |this, cx| {
            // Give it a moment to clean up, then drop it either way.
            let timeout = cx.background_executor().timer(std::time::Duration::from_millis(1500));
            futures::future::select(disconnect, Box::pin(timeout)).await;
            this.update(cx, |s, cx| s.end("Stopped".into(), cx)).ok();
        });
        task.detach();
    }

    fn end(&mut self, message: String, cx: &mut Context<Self>) {
        if !self.is_active() {
            return;
        }
        self.status = Status::Ended(message);
        // Dropping the client ends the adapter process.
        self.client = None;
        self.frames.clear();
        self.scopes.clear();
        self.children.clear();
        cx.emit(DebugEvent::Ended);
        cx.notify();
    }

    // ---- Console -----------------------------------------------------------------------

    fn note(&mut self, text: String, cx: &mut Context<Self>) {
        self.push_line(Line::Info, &text, cx);
    }

    fn push_line(&mut self, kind: Line, text: &str, cx: &mut Context<Self>) {
        self.console.push((kind, text.trim_end_matches(['\r', '\n']).to_string()));
        self.console_open = false;
        self.trim_console(cx);
    }

    /// Output that may come in pieces: lines continue until a line break.
    fn push_text(&mut self, kind: Line, text: &str, cx: &mut Context<Self>) {
        let mut pieces = text.split('\n').peekable();
        while let Some(piece) = pieces.next() {
            let last = pieces.peek().is_none();
            if last && piece.is_empty() {
                // The text ended with a line break.
                self.console_open = false;
                break;
            }
            let piece = piece.trim_end_matches('\r');
            match self.console.last_mut() {
                Some((k, line)) if self.console_open && *k == kind => line.push_str(piece),
                _ => self.console.push((kind, piece.to_string())),
            }
            self.console_open = last;
        }
        self.trim_console(cx);
    }

    /// A complete answer (an evaluation): its own lines, whatever the
    /// program printed without a line break before it.
    fn push_lines(&mut self, kind: Line, text: &str, cx: &mut Context<Self>) {
        for line in text.trim_end().lines() {
            self.console.push((kind, line.trim_end_matches('\r').to_string()));
        }
        self.console_open = false;
        self.trim_console(cx);
    }

    fn trim_console(&mut self, cx: &mut Context<Self>) {
        if self.console.len() > MAX_CONSOLE {
            let extra = self.console.len() - MAX_CONSOLE;
            self.console.drain(..extra);
        }
        self.console_scroll.scroll_to_bottom();
        cx.notify();
    }

    fn on_enter(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.focus_handle(cx).is_focused(window) {
            let expression = self.input.read(cx).text().trim().to_string();
            if !expression.is_empty() {
                self.input.update(cx, |e, cx| e.set_text("", cx));
                self.evaluate(expression, cx);
            }
        } else if self.watch_input.focus_handle(cx).is_focused(window) {
            let expression = self.watch_input.read(cx).text().trim().to_string();
            if !expression.is_empty() {
                self.watch_input.update(cx, |e, cx| e.set_text("", cx));
                self.watches.push((expression, None));
                let ix = self.watches.len() - 1;
                if self.is_paused() {
                    self.evaluate_watch(ix, cx);
                }
                cx.notify();
            }
        }
    }

    fn status_label(&self) -> String {
        match &self.status {
            Status::Building => "Building…".into(),
            Status::Starting => "Starting…".into(),
            Status::Running => "Running".into(),
            Status::Paused(reason) => reason.clone(),
            Status::Ended(message) => message.clone(),
        }
    }
}

/// The status for a `stopped` event. Adapters' own texts ("breakpoint 2.1")
/// are only worth showing for exceptions.
fn pause_reason(body: &Value) -> String {
    let text = body["text"].as_str().or(body["description"].as_str()).filter(|t| !t.is_empty());
    match body["reason"].as_str().unwrap_or("pause") {
        "breakpoint" | "function breakpoint" | "data breakpoint" | "instruction breakpoint" => "Paused at a breakpoint".into(),
        "entry" => "Paused on entry".into(),
        "exception" => match text {
            Some(text) => format!("Exception: {text}"),
            None => "Paused on an exception".into(),
        },
        _ => "Paused".into(),
    }
}

/// An adapter's console output without its own bookkeeping: lldb echoes
/// the init commands and reports the exit that Kin already notes.
fn without_chatter(text: &str) -> String {
    let chatter = |line: &str| {
        let line = line.trim_end();
        line.starts_with("(lldb) ")
            || line.starts_with("Running ") && line.ends_with("Commands:")
            || line.starts_with("Executing commands in ")
            || line.starts_with("Process ") && line.contains(" exited with status = ")
    };
    text.split_inclusive('\n').filter(|line| !chatter(line)).collect()
}

/// A frame's function name, readable: Rust's legacy symbols come with a
/// hash and `$LT$`-style escapes.
fn frame_name(name: &str) -> String {
    let hashed = name.rsplit_once("::h").is_some_and(|(_, h)| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()));
    if !hashed {
        return name.to_string();
    }
    let name = &name[..name.len() - 19];
    let decoded = name
        .split("::")
        .map(|segment| {
            let segment = segment.strip_prefix("_$").map_or(segment.to_string(), |rest| format!("${rest}"));
            let mut out = segment.replace("..", "::");
            for (code, text) in [
                ("$LT$", "<"),
                ("$GT$", ">"),
                ("$C$", ","),
                ("$RF$", "&"),
                ("$BP$", "*"),
                ("$SP$", "@"),
                ("$LP$", "("),
                ("$RP$", ")"),
                ("$u20$", " "),
                ("$u27$", "'"),
                ("$u5b$", "["),
                ("$u5d$", "]"),
                ("$u7b$", "{"),
                ("$u7d$", "}"),
                ("$u7e$", "~"),
            ] {
                out = out.replace(code, text);
            }
            out
        })
        .collect::<Vec<_>>();
    decoded.join("::")
}

fn breakpoints_arguments(path: &std::path::Path, rows: &[usize]) -> Value {
    json!({
        "source": { "path": path, "name": path.file_name().map(|n| n.to_string_lossy().into_owned()) },
        "breakpoints": rows.iter().map(|r| json!({ "line": r + 1 })).collect::<Vec<_>>(),
        "sourceModified": false,
    })
}

impl Focusable for DebugSession {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DebugSession {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let paused = self.is_paused();
        let running = self.status == Status::Running;
        let active = self.is_active();

        let tool = |id: &'static str, icon: Icon, tip: &'static str, enabled: bool, color: Hsla, cx: &mut Context<Self>, f: fn(&mut Self, &mut Context<Self>)| {
            div()
                .id(id)
                .size(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .when(enabled, |el| {
                    el.cursor_pointer().hover(|s| s.bg(theme.hover)).on_click(cx.listener(move |s, _: &ClickEvent, _, cx| f(s, cx)))
                })
                .tooltip(move |_, cx| tooltip_view(tip, cx))
                .child(icon_el(icon, px(14.), if enabled { color } else { Hsla::from(theme.text_dim).alpha(0.45) }))
        };
        let muted: Hsla = theme.text_muted.into();
        let status_color: Hsla = match &self.status {
            Status::Paused(_) => theme.warning.into(),
            Status::Running => theme.success.into(),
            Status::Ended(_) => theme.text_dim.into(),
            _ => theme.text_muted.into(),
        };
        let header = div()
            .flex()
            .items_center()
            .gap_1()
            .h(px(32.))
            .px_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border_subtle)
            .child(div().px_1().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text_muted).child("DEBUG"))
            .child(div().max_w(px(240.)).truncate().text_color(theme.text).child(self.target.name.clone()))
            .child(div().ml_1().max_w(px(320.)).truncate().text_size(px(12.)).text_color(status_color).child(self.status_label()))
            .child(div().w(px(8.)))
            .child(tool("debug-resume", Icon::Play, "Resume", paused, theme.success.into(), cx, Self::resume))
            .child(tool("debug-pause", Icon::Pause, "Pause", running, muted, cx, Self::pause))
            .child(tool("debug-step-over", Icon::StepOver, "Step Over", paused, muted, cx, Self::step_over))
            .child(tool("debug-step-into", Icon::StepInto, "Step Into", paused, muted, cx, Self::step_into))
            .child(tool("debug-step-out", Icon::StepOut, "Step Out", paused, muted, cx, Self::step_out))
            .child(tool("debug-stop", Icon::Square, "Stop", active, theme.error.into(), cx, Self::stop))
            .child(div().flex_1())
            .child(tool("debug-hide", Icon::X, "Hide", true, muted, cx, |_, cx| cx.emit(DebugEvent::Hide)));

        let column_title = |title: &'static str| {
            div()
                .flex_shrink_0()
                .h(px(24.))
                .px_2()
                .flex()
                .items_center()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_dim)
                .child(title)
        };
        let mono = font(theme.mono_font.clone());
        let code_size = px(theme.editor_font_size - 1.5);

        // Frames of the paused thread.
        let frames: Vec<AnyElement> = self
            .frames
            .iter()
            .enumerate()
            .map(|(ix, f)| {
                let selected = ix == self.frame;
                let place = match &f.path {
                    Some(p) => format!("{}:{}", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), f.line),
                    None => String::new(),
                };
                let ours = f.path.as_ref().is_some_and(|p| p.is_file());
                div()
                    .id(("debug-frame", ix))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(22.))
                    .px_2()
                    .mx_1()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .when(selected, |el| el.bg(theme.accent_soft))
                    .when(!selected, |el| el.hover(|s| s.bg(theme.hover)))
                    .on_click(cx.listener(move |s, _: &ClickEvent, _, cx| s.select_frame(ix, cx)))
                    .child(div().flex_1().min_w_0().truncate().text_color(if ours { theme.text } else { theme.text_dim }).child(f.name.clone()))
                    .child(div().flex_shrink_0().text_size(px(11.)).text_color(theme.text_dim).child(place))
                    .into_any_element()
            })
            .collect();
        let frames_empty = match &self.status {
            Status::Paused(_) => "Loading…",
            Status::Ended(_) => "Not running",
            _ => "Frames show when the program pauses",
        };

        // Watches, then the scopes with their (expanded) variables.
        let mut rows: Vec<AnyElement> = Vec::new();
        for (ix, (expression, value)) in self.watches.iter().enumerate() {
            let (text, color): (String, Hsla) = match value {
                Some(Ok(v)) => (v.clone(), theme.text.into()),
                Some(Err(e)) => (e.clone(), theme.error.into()),
                None => (String::new(), theme.text_dim.into()),
            };
            rows.push(
                div()
                    .id(("debug-watch", ix))
                    .group("debug-watch")
                    .flex()
                    .items_center()
                    .gap_1()
                    .h(px(22.))
                    .px_2()
                    .font(mono.clone())
                    .text_size(code_size)
                    .child(icon_el(Icon::Search, px(11.), theme.text_dim.into()))
                    .child(div().flex_shrink_0().text_color(theme.syntax.property).child(expression.clone()))
                    .child(div().text_color(theme.text_dim).child("="))
                    .child(div().flex_1().min_w_0().truncate().text_color(color).child(text))
                    .child(
                        div()
                            .id(("debug-unwatch", ix))
                            .invisible()
                            .group_hover("debug-watch", |s| s.visible())
                            .cursor_pointer()
                            .on_click(cx.listener(move |s, _: &ClickEvent, _, cx| {
                                if ix < s.watches.len() {
                                    s.watches.remove(ix);
                                    cx.notify();
                                }
                            }))
                            .child(icon_el(Icon::X, px(11.), theme.text_muted.into())),
                    )
                    .into_any_element(),
            );
        }
        let mut stack: Vec<(Var, usize, String)> = self.scopes.iter().rev().map(|v| (v.clone(), 0, v.reference.to_string())).collect();
        while let Some((var, depth, key)) = stack.pop() {
            let open = self.expanded.contains(&var.reference);
            let reference = var.reference;
            rows.push(
                div()
                    .id(SharedString::from(format!("debug-var-{key}")))
                    .flex()
                    .items_center()
                    .gap_1()
                    .h(px(22.))
                    .pl(px(6. + depth as f32 * 14.))
                    .pr_2()
                    .font(mono.clone())
                    .text_size(code_size)
                    .when(reference > 0, |el| {
                        el.cursor_pointer().hover(|s| s.bg(theme.hover)).on_click(cx.listener(move |s, _: &ClickEvent, _, cx| s.toggle(reference, cx)))
                    })
                    .child(div().w(px(12.)).flex_shrink_0().when(reference > 0, |el| {
                        el.child(icon_el(if open { Icon::ChevronDown } else { Icon::ChevronRight }, px(11.), theme.text_dim.into()))
                    }))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(if depth == 0 { theme.text_muted } else { theme.syntax.property })
                            .when(depth == 0, |el| el.font_weight(FontWeight::SEMIBOLD))
                            .child(var.name.clone()),
                    )
                    .when(!var.value.is_empty(), |el| {
                        el.child(div().text_color(theme.text_dim).child("="))
                            .child(div().flex_1().min_w_0().truncate().text_color(theme.text).child(var.value.clone()))
                    })
                    .when_some(var.ty.clone(), |el, ty| el.child(div().flex_shrink_0().text_size(px(11.)).text_color(theme.text_dim).child(ty)))
                    .into_any_element(),
            );
            if open && let Some(children) = self.children.get(&reference) {
                for (ix, child) in children.iter().enumerate().rev() {
                    stack.push((child.clone(), depth + 1, format!("{key}-{ix}")));
                }
            }
        }

        // Console: program output, notes and evaluations.
        let console: Vec<AnyElement> = self
            .console
            .iter()
            .map(|(kind, text)| {
                let color: Hsla = match kind {
                    Line::Output => theme.text.into(),
                    Line::Error => theme.error.into(),
                    Line::Info => theme.text_dim.into(),
                    Line::Input => theme.accent.into(),
                };
                div().min_h(px(18.)).whitespace_nowrap().text_color(color).child(text.clone()).into_any_element()
            })
            .collect();

        div()
            .key_context("DebugPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_enter))
            .flex()
            .flex_col()
            .size_full()
            .text_size(px(12.5))
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w(px(260.))
                            .flex_shrink_0()
                            .border_r_1()
                            .border_color(theme.border_subtle)
                            .child(column_title("FRAMES"))
                            .child(
                                div()
                                    .id("debug-frames")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .when(frames.is_empty(), |el| el.child(div().px_3().py_1().text_color(theme.text_dim).child(frames_empty)))
                                    .children(frames),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .border_r_1()
                            .border_color(theme.border_subtle)
                            .child(column_title("VARIABLES"))
                            .child(div().id("debug-vars").flex_1().min_h_0().overflow_y_scroll().children(rows))
                            .child(div().p_1p5().border_t_1().border_color(theme.border_subtle).child(input_box(&self.watch_input, Some(Icon::Plus), window, cx))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(column_title("CONSOLE"))
                            .child(
                                div()
                                    .id("debug-console")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.console_scroll)
                                    .px_2()
                                    .font(mono.clone())
                                    .text_size(code_size)
                                    .children(console),
                            )
                            .child(div().p_1p5().border_t_1().border_color(theme.border_subtle).child(input_box(&self.input, Some(Icon::ChevronRight), window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dap;

    #[test]
    fn readable_frames_and_quiet_console() {
        assert_eq!(frame_name("dbgdemo::add::h818874da906e1f2a"), "dbgdemo::add");
        assert_eq!(
            frame_name("std::rt::lang_start::_$u7b$$u7b$closure$u7d$$u7d$::h0123456789abcdef"),
            "std::rt::lang_start::{{closure}}"
        );
        assert_eq!(
            frame_name("_$LT$alloc..vec..Vec$LT$T$C$A$GT$$u20$as$u20$core..ops..drop..Drop$GT$::drop::hfedcba9876543210"),
            "<alloc::vec::Vec<T,A> as core::ops::drop::Drop>::drop"
        );
        assert_eq!(frame_name("main.main"), "main.main");
        let text = "Running initCommands:\n(lldb) command source -s 0 \"x\"\nhello\nProcess 41434 exited with status = 0 (0x00000000) \n";
        assert_eq!(without_chatter(text), "hello\n");
        assert_eq!(pause_reason(&json!({"reason": "breakpoint", "description": "breakpoint 1.1"})), "Paused at a breakpoint");
        assert_eq!(pause_reason(&json!({"reason": "exception", "text": "ZeroDivisionError"})), "Exception: ZeroDivisionError");
    }

    /// The whole protocol against a real adapter: build, launch, stop at a
    /// breakpoint, read the stack and locals, continue to the end.
    #[test]
    #[ignore = "needs cargo and lldb-dap; run with --ignored"]
    fn debugs_a_rust_program_with_lldb_dap() {
        let dir = std::env::temp_dir().join(format!("kin-dap-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"dapdemo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        let main = "fn main() {\n    let name = String::from(\"Kin\");\n    let n = name.len();\n    println!(\"{name} {n}\");\n}\n";
        std::fs::write(dir.join("src/main.rs"), main).unwrap();
        let dir = dir.canonicalize().unwrap();

        let (done_tx, done) = std::sync::mpsc::channel();
        let work = dir.clone();
        std::thread::spawn(move || {
            let spec = DebugSpec::Cargo { build: vec!["build".into()], args: Vec::new() };
            let exe = launch::cargo_build(&work, &["build".into()], &launch::Build::default(), |_| {}).unwrap();
            let adapter = launch::adapter(&spec, &work).unwrap();
            let mut command = dap::command(&adapter.program, &adapter.args, &work);
            let (client, mut incoming) = DapClient::spawn_stdio(&mut command).unwrap();
            futures::executor::block_on(async {
                client.request("initialize", launch::initialize_arguments(adapter.id)).await.unwrap().unwrap();
                let _launched = client.request("launch", launch::launch_arguments(&spec, &work, Some(&exe)));
                let mut event = async |name: &str| loop {
                    match incoming.next().await {
                        Some(Incoming::Event { event, body }) if event == name => return body,
                        Some(_) => {}
                        None => panic!("the adapter closed before `{name}`"),
                    }
                };
                event("initialized").await;
                let set = client.request("setBreakpoints", breakpoints_arguments(&work.join("src/main.rs"), &[2])).await.unwrap().unwrap();
                assert_eq!(set["breakpoints"][0]["verified"], true, "{set}");
                client.request("configurationDone", json!({})).await.unwrap().unwrap();

                let stopped = event("stopped").await;
                let thread = stopped["threadId"].as_i64().unwrap();
                let stack = client.request("stackTrace", json!({ "threadId": thread })).await.unwrap().unwrap();
                let top = &stack["stackFrames"][0];
                assert_eq!((top["line"].as_u64(), top["source"]["path"].as_str().map(PathBuf::from)), (Some(3), Some(work.join("src/main.rs"))));
                let scopes = client.request("scopes", json!({ "frameId": top["id"] })).await.unwrap().unwrap();
                let locals = scopes["scopes"][0]["variablesReference"].clone();
                let vars = client.request("variables", json!({ "variablesReference": locals })).await.unwrap().unwrap();
                let name = vars["variables"].as_array().unwrap().iter().find(|v| v["name"] == "name").cloned().unwrap();
                // Rust's formatters show the string itself.
                assert!(name["value"].as_str().unwrap().contains("Kin"), "{name}");

                client.request("continue", json!({ "threadId": thread })).await.unwrap().unwrap();
                event("terminated").await;
            });
            done_tx.send(()).ok();
        });
        done.recv_timeout(std::time::Duration::from_secs(120)).expect("the debug session didn't finish in time");
        std::fs::remove_dir_all(&dir).ok();
    }
}
