//! JSON-RPC over a language server's stdin/stdout. A reader thread parses
//! `Content-Length` frames: responses complete the matching request's
//! future, everything else goes to the `incoming` channel. A writer thread
//! sends, so a full pipe never blocks the UI.

use std::{
    collections::HashMap,
    future::Future,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    pin::Pin,
    process::{Child, ChildStdin, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
        mpsc,
    },
    task::Poll,
};

use futures::channel::{mpsc as async_mpsc, oneshot};
use serde_json::{Value, json};

/// A message the server sent on its own.
#[derive(Debug)]
pub enum Incoming {
    Notification { method: String, params: Value },
    /// Needs an answer through [`Client::respond`].
    Request { id: Value, method: String, params: Value },
    /// The server exited or its output became unreadable.
    Closed { stderr_tail: String },
}

#[derive(Debug, Clone)]
pub struct RpcError {
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The error for requests still waiting when the server exits.
pub const STOPPED: &str = "The language server stopped";

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

pub struct Client {
    child: Mutex<Child>,
    writer: mpsc::Sender<Vec<u8>>,
    next_id: AtomicI64,
    pending: Pending,
}

impl Client {
    /// Starts `command` in `cwd`. The returned receiver yields what the
    /// server sends besides responses.
    pub fn spawn(
        program: &Path,
        args: &[String],
        cwd: &Path,
        path_env: &std::ffi::OsStr,
    ) -> std::io::Result<(Arc<Client>, async_mpsc::UnboundedReceiver<Incoming>)> {
        let mut command = crate::process::command(program);
        command
            .args(args)
            .current_dir(cwd)
            // Node-based servers start with `#!/usr/bin/env node`.
            .env("PATH", path_env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| std::io::Error::other("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| std::io::Error::other("no stderr"))?;

        let (writer, to_write) = mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new().name("lsp-writer".into()).spawn(move || write_loop(stdin, to_write))?;

        // Keep the end of stderr to explain a crash.
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let tail = stderr_tail.clone();
        std::thread::Builder::new().name("lsp-stderr".into()).spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut tail = tail.lock().unwrap();
                tail.push_str(&line);
                tail.push('\n');
                if tail.len() > 4000 {
                    let cut = tail.len() - 2000;
                    let cut = tail.ceil_char_boundary(cut);
                    tail.drain(..cut);
                }
            }
        })?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (incoming_tx, incoming) = async_mpsc::unbounded();
        let reader_pending = pending.clone();
        std::thread::Builder::new()
            .name("lsp-reader".into())
            .spawn(move || read_loop(stdout, reader_pending, incoming_tx, stderr_tail))?;

        let client = Arc::new(Client { child: Mutex::new(child), writer, next_id: AtomicI64::new(1), pending });
        Ok((client, incoming))
    }

    /// Sends a request; the future resolves with the result. Dropping it
    /// before the answer comes tells the server to stop working on it.
    pub fn request(&self, method: &str, params: Value) -> Request {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        Request { id, rx, pending: self.pending.clone(), writer: self.writer.clone() }
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Answers a request from the server.
    pub fn respond(&self, id: Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn send(&self, message: Value) {
        send(&self.writer, message);
    }

    /// Asks the server to exit, then makes sure it does. Blocks for up to
    /// half a second, so call it off the main thread.
    pub fn shutdown(&self) {
        // Kept until the end, so it isn't cancelled.
        let _shutdown = self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        for _ in 0..20 {
            if self.child.lock().ok().and_then(|mut c| c.try_wait().ok().flatten()).is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

/// A request waiting for its answer (see [`Client::request`]).
pub struct Request {
    id: i64,
    rx: oneshot::Receiver<Result<Value, RpcError>>,
    pending: Pending,
    writer: mpsc::Sender<Vec<u8>>,
}

impl Future for Request {
    type Output = Result<Value, RpcError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.rx).poll(cx).map(|answer| answer.unwrap_or_else(|_| Err(RpcError { message: STOPPED.into() })))
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        // Still pending: no answer came, so the server may still be at it.
        if self.pending.lock().unwrap().remove(&self.id).is_some() {
            send(&self.writer, json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": self.id } }));
        }
    }
}

fn send(writer: &mpsc::Sender<Vec<u8>>, message: Value) {
    let body = message.to_string();
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(body.as_bytes());
    // Fails only once the writer thread is gone (server exited).
    writer.send(frame).ok();
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn write_loop(mut stdin: ChildStdin, frames: mpsc::Receiver<Vec<u8>>) {
    for frame in frames {
        if stdin.write_all(&frame).and_then(|_| stdin.flush()).is_err() {
            break;
        }
    }
}

fn read_loop(
    stdout: impl Read,
    pending: Pending,
    incoming: async_mpsc::UnboundedSender<Incoming>,
    stderr_tail: Arc<Mutex<String>>,
) {
    let mut reader = BufReader::new(stdout);
    while let Some(message) = read_message(&mut reader) {
        dispatch(message, &pending, &incoming);
    }
    // Whatever is still waiting will never get an answer.
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(Err(RpcError { message: STOPPED.into() }));
    }
    // Let stderr catch up so the tail explains the exit.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let stderr_tail = stderr_tail.lock().map(|t| t.clone()).unwrap_or_default();
    let _ = incoming.unbounded_send(Incoming::Closed { stderr_tail });
}

/// Reads one `Content-Length`-framed message; None at end of stream.
pub(crate) fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    loop {
        let mut length = None;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let header = line.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse::<usize>().ok();
            }
        }
        let Some(length) = length else { continue };
        let mut body = vec![0; length];
        reader.read_exact(&mut body).ok()?;
        if let Ok(value) = serde_json::from_slice(&body) {
            return Some(value);
        }
    }
}

fn dispatch(mut message: Value, pending: &Pending, incoming: &async_mpsc::UnboundedSender<Incoming>) {
    let method = message.get("method").and_then(Value::as_str).map(str::to_string);
    let id = message.get_mut("id").map(Value::take);
    match (method, id) {
        (Some(method), Some(id)) => {
            let params = message.get_mut("params").map(Value::take).unwrap_or(Value::Null);
            let _ = incoming.unbounded_send(Incoming::Request { id, method, params });
        }
        (Some(method), None) => {
            let params = message.get_mut("params").map(Value::take).unwrap_or(Value::Null);
            let _ = incoming.unbounded_send(Incoming::Notification { method, params });
        }
        (None, Some(id)) => {
            let Some(id) = id.as_i64() else { return };
            let Some(tx) = pending.lock().unwrap().remove(&id) else { return };
            let result = match message.get_mut("error") {
                Some(error) => Err(RpcError {
                    message: error.get("message").and_then(Value::as_str).unwrap_or("Request failed").to_string(),
                }),
                None => Ok(message.get_mut("result").map(Value::take).unwrap_or(Value::Null)),
            };
            let _ = tx.send(result);
        }
        (None, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_an_unanswered_request_cancels_it() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (writer, written) = mpsc::channel();
        let request = |id: i64| {
            let (tx, rx) = oneshot::channel();
            pending.lock().unwrap().insert(id, tx);
            Request { id, rx, pending: pending.clone(), writer: writer.clone() }
        };
        let (incoming, _) = async_mpsc::unbounded();

        // Answered, then dropped: nothing to cancel.
        let answered = request(1);
        dispatch(json!({ "jsonrpc": "2.0", "id": 1, "result": 42 }), &pending, &incoming);
        assert_eq!(futures::executor::block_on(answered).unwrap(), 42);
        assert!(written.try_recv().is_err());

        // Dropped while waiting: the server hears $/cancelRequest, and a
        // late answer goes nowhere.
        drop(request(2));
        let frame = String::from_utf8(written.try_recv().unwrap()).unwrap();
        assert!(frame.ends_with(r#"{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":2}}"#), "{frame}");
        assert!(pending.lock().unwrap().is_empty());
        dispatch(json!({ "jsonrpc": "2.0", "id": 2, "result": null }), &pending, &incoming);
    }

    #[test]
    fn reads_frames() {
        let a = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let b = r#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"message":"é"}}"#;
        let stream = format!(
            "Content-Length: {}\r\n\r\n{a}Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n{b}",
            a.len(),
            b.len()
        );
        let mut reader = BufReader::new(stream.as_bytes());
        assert_eq!(read_message(&mut reader).unwrap()["result"]["ok"], true);
        assert_eq!(read_message(&mut reader).unwrap()["params"]["message"], "é");
        assert!(read_message(&mut reader).is_none());
    }

    #[test]
    fn routes_responses_and_requests() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = oneshot::channel();
        pending.lock().unwrap().insert(7, tx);
        let (incoming_tx, mut incoming) = async_mpsc::unbounded();
        dispatch(json!({"id": 7, "error": {"code": -1, "message": "nope"}}), &pending, &incoming_tx);
        assert_eq!(rx.try_recv().unwrap().unwrap().unwrap_err().message, "nope");
        dispatch(json!({"id": "a", "method": "workspace/configuration", "params": {}}), &pending, &incoming_tx);
        assert!(matches!(incoming.try_recv(), Ok(Incoming::Request { method, .. }) if method == "workspace/configuration"));
    }
}
