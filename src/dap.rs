//! The Debug Adapter Protocol, as a client: messages to and from a debug
//! adapter (lldb-dap, Delve, debugpy) over its stdin/stdout or a TCP socket.
//! The framing is LSP's (`Content-Length` headers); requests carry a `seq`
//! that responses answer with `request_seq`. A reader thread completes the
//! matching request's future and sends everything else to a channel; a
//! writer thread sends, so the UI never blocks on a full pipe.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use futures::channel::{mpsc as async_mpsc, oneshot};
use serde_json::{Value, json};

use crate::lsp::rpc::read_message;

/// What the adapter sends besides responses.
#[derive(Debug)]
pub enum Incoming {
    Event { event: String, body: Value },
    /// A "reverse request" (`startDebugging`, `runInTerminal`…); answer
    /// with [`DapClient::respond`].
    Request { seq: i64, command: String, arguments: Value },
    /// A line the adapter process printed (TCP adapters: their own stdout and stderr).
    Output(String),
    /// The adapter exited or its connection closed.
    Closed { tail: String },
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;

pub struct DapClient {
    writer: mpsc::Sender<Vec<u8>>,
    seq: AtomicI64,
    pending: Pending,
    /// The adapter's process (none for a child session's connection).
    child: Mutex<Option<Child>>,
}

impl DapClient {
    /// An adapter that talks over its stdin/stdout (lldb-dap, debugpy).
    pub fn spawn_stdio(command: &mut Command) -> std::io::Result<(Arc<DapClient>, async_mpsc::UnboundedReceiver<Incoming>)> {
        let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| std::io::Error::other("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| std::io::Error::other("no stderr"))?;
        let (incoming_tx, incoming) = async_mpsc::unbounded();
        let tail = keep_tail(stderr, None);
        let client = Self::start(Some(child), stdin, stdout, tail, incoming_tx)?;
        Ok((client, incoming))
    }

    /// An adapter that listens on a port (Delve): starts it, then connects,
    /// retrying while it comes up. Its own output goes to the console.
    pub fn spawn_tcp(command: &mut Command, addr: SocketAddr) -> std::io::Result<(Arc<DapClient>, async_mpsc::UnboundedReceiver<Incoming>)> {
        let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let (incoming_tx, incoming) = async_mpsc::unbounded();
        let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| std::io::Error::other("no stderr"))?;
        keep_tail(stdout, Some(incoming_tx.clone()));
        let tail = keep_tail(stderr, Some(incoming_tx.clone()));
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline && child.try_wait().ok().flatten().is_none() => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(err) => {
                    let _ = child.kill();
                    let detail = tail.lock().map(|t| t.trim().to_string()).unwrap_or_default();
                    return Err(std::io::Error::other(if detail.is_empty() { err.to_string() } else { detail }));
                }
            }
        };
        let reader = stream.try_clone()?;
        let client = Self::start(Some(child), stream, reader, tail, incoming_tx)?;
        Ok((client, incoming))
    }

    /// Another connection to an adapter that's listening already: js-debug
    /// runs each Node process as a child session on one of these.
    pub fn connect_tcp(addr: SocketAddr) -> std::io::Result<(Arc<DapClient>, async_mpsc::UnboundedReceiver<Incoming>)> {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        let reader = stream.try_clone()?;
        let (incoming_tx, incoming) = async_mpsc::unbounded();
        let client = Self::start(None, stream, reader, Arc::default(), incoming_tx)?;
        Ok((client, incoming))
    }

    fn start(
        child: Option<Child>,
        writer: impl Write + Send + 'static,
        reader: impl Read + Send + 'static,
        tail: Arc<Mutex<String>>,
        incoming: async_mpsc::UnboundedSender<Incoming>,
    ) -> std::io::Result<Arc<DapClient>> {
        let (write_tx, to_write) = mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new().name("dap-writer".into()).spawn(move || {
            let mut writer = writer;
            for frame in to_write {
                if writer.write_all(&frame).and_then(|_| writer.flush()).is_err() {
                    break;
                }
            }
        })?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        let reader_tx = incoming;
        std::thread::Builder::new().name("dap-reader".into()).spawn(move || {
            let mut reader = BufReader::new(reader);
            while let Some(message) = read_message(&mut reader) {
                dispatch(message, &reader_pending, &reader_tx);
            }
            for (_, tx) in reader_pending.lock().unwrap().drain() {
                let _ = tx.send(Err("The debugger stopped".into()));
            }
            std::thread::sleep(Duration::from_millis(50));
            let tail = tail.lock().map(|t| t.clone()).unwrap_or_default();
            let _ = reader_tx.unbounded_send(Incoming::Closed { tail });
        })?;
        Ok(Arc::new(DapClient { writer: write_tx, seq: AtomicI64::new(1), pending, child: Mutex::new(child) }))
    }

    /// Sends a request; resolves with the response's body, or its error message.
    pub fn request(&self, command: &str, arguments: Value) -> oneshot::Receiver<Result<Value, String>> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(seq, tx);
        self.send(json!({ "seq": seq, "type": "request", "command": command, "arguments": arguments }));
        rx
    }

    /// Answers a reverse request.
    pub fn respond(&self, request_seq: i64, command: &str, success: bool, body: Value) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.send(json!({
            "seq": seq, "type": "response", "request_seq": request_seq,
            "command": command, "success": success, "body": body,
        }));
    }

    fn send(&self, message: Value) {
        let body = message.to_string();
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(body.as_bytes());
        self.writer.send(frame).ok();
    }
}

impl Drop for DapClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().ok().and_then(|mut c| c.take()) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Reads a stream line by line, keeping its last few KB (to explain a
/// crash) and optionally passing each line on as [`Incoming::Output`].
fn keep_tail(stream: impl Read + Send + 'static, forward: Option<async_mpsc::UnboundedSender<Incoming>>) -> Arc<Mutex<String>> {
    let tail = Arc::new(Mutex::new(String::new()));
    let keep = tail.clone();
    std::thread::Builder::new()
        .name("dap-stderr".into())
        .spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if let Some(tx) = &forward {
                    let _ = tx.unbounded_send(Incoming::Output(line.clone()));
                }
                let mut tail = keep.lock().unwrap();
                tail.push_str(&line);
                tail.push('\n');
                if tail.len() > 4000 {
                    let cut = tail.ceil_char_boundary(tail.len() - 2000);
                    tail.drain(..cut);
                }
            }
        })
        .ok();
    tail
}

fn dispatch(mut message: Value, pending: &Pending, incoming: &async_mpsc::UnboundedSender<Incoming>) {
    let kind = message.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
    let body = message.get_mut("body").map(Value::take).unwrap_or(Value::Null);
    match kind.as_str() {
        "response" => {
            let Some(seq) = message.get("request_seq").and_then(Value::as_i64) else { return };
            let Some(tx) = pending.lock().unwrap().remove(&seq) else { return };
            let success = message.get("success").and_then(Value::as_bool).unwrap_or(false);
            let result = if success {
                Ok(body)
            } else {
                // `body.error.format` is the detailed text; `message` a short one.
                let detail = body.get("error").and_then(|e| e.get("format")).and_then(Value::as_str);
                let short = message.get("message").and_then(Value::as_str);
                Err(detail.or(short).unwrap_or("The request failed").to_string())
            };
            let _ = tx.send(result);
        }
        "event" => {
            let event = message.get("event").and_then(Value::as_str).unwrap_or_default().to_string();
            let _ = incoming.unbounded_send(Incoming::Event { event, body });
        }
        "request" => {
            let seq = message.get("seq").and_then(Value::as_i64).unwrap_or(0);
            let command = message.get("command").and_then(Value::as_str).unwrap_or_default().to_string();
            let arguments = message.get_mut("arguments").map(Value::take).unwrap_or(Value::Null);
            let _ = incoming.unbounded_send(Incoming::Request { seq, command, arguments });
        }
        _ => {}
    }
}

/// A free local port for an adapter that listens (the OS picks it).
pub fn free_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// Where an adapter's working directory and PATH come from.
pub fn command(program: &Path, args: &[String], cwd: &Path) -> Command {
    let mut command = crate::process::command(program);
    command.args(args).current_dir(cwd).env("PATH", crate::lsp::servers::search_path());
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_responses_events_and_reverse_requests() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (ok_tx, mut ok_rx) = oneshot::channel();
        let (err_tx, mut err_rx) = oneshot::channel();
        pending.lock().unwrap().insert(3, ok_tx);
        pending.lock().unwrap().insert(4, err_tx);
        let (tx, mut incoming) = async_mpsc::unbounded();
        dispatch(json!({"type": "response", "request_seq": 3, "success": true, "body": {"threads": []}}), &pending, &tx);
        dispatch(
            json!({"type": "response", "request_seq": 4, "success": false, "message": "short",
                   "body": {"error": {"format": "Could not launch: no such file"}}}),
            &pending,
            &tx,
        );
        assert_eq!(ok_rx.try_recv().unwrap().unwrap().unwrap(), json!({"threads": []}));
        assert_eq!(err_rx.try_recv().unwrap().unwrap().unwrap_err(), "Could not launch: no such file");
        dispatch(json!({"type": "event", "event": "stopped", "body": {"threadId": 1}}), &pending, &tx);
        assert!(matches!(incoming.try_recv(), Ok(Incoming::Event { event, body }) if event == "stopped" && body["threadId"] == 1));
        dispatch(json!({"type": "request", "seq": 9, "command": "startDebugging", "arguments": {"request": "launch"}}), &pending, &tx);
        assert!(matches!(incoming.try_recv(), Ok(Incoming::Request { seq: 9, command, arguments })
            if command == "startDebugging" && arguments["request"] == "launch"));
    }
}
