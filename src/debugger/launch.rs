//! What a debug session runs: the adapter for a [`DebugSpec`] (lldb-dap for
//! Rust and other native code, Delve for Go, debugpy for Python, js-debug
//! for JavaScript; found on PATH, through Xcode or in Kin's config folder,
//! never downloaded), the cargo build that makes a Rust executable, the
//! `launch` or `attach` arguments each adapter expects, and the processes
//! Attach to Process lists.

use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

use serde_json::{Value, json};

use crate::{lsp::servers, run::DebugSpec};

/// How to start an adapter.
pub struct Adapter {
    /// `adapterID` for `initialize`.
    pub id: &'static str,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Listens on this port instead of talking over stdio (Delve).
    pub port: Option<u16>,
}

/// The adapter for `spec`, or what to install when it's missing.
pub fn adapter(spec: &DebugSpec, cwd: &Path) -> Result<Adapter, String> {
    match spec {
        DebugSpec::Cargo { .. } => lldb_dap(),
        DebugSpec::Go { .. } => delve(),
        DebugSpec::Python { .. } => debugpy(cwd),
        DebugSpec::Node { .. } => js_debug(),
        // What the process runs decides; anything else is native code.
        DebugSpec::Attach { program, .. } => {
            let name = program.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            if name == "node" {
                js_debug()
            } else if name.starts_with("python") {
                debugpy(cwd)
            } else if is_go_program(program) {
                delve()
            } else {
                lldb_dap()
            }
        }
    }
}

fn lldb_dap() -> Result<Adapter, String> {
    let program = servers::which("lldb-dap")
        .or_else(xcrun_lldb_dap)
        .or_else(|| servers::which("lldb-vscode"))
        .ok_or("Debugging Rust needs lldb-dap: install Xcode's command line tools (xcode-select --install) or LLVM.")?;
    Ok(Adapter { id: "lldb-dap", program, args: Vec::new(), port: None })
}

fn delve() -> Result<Adapter, String> {
    let program = servers::which("dlv").ok_or("Debugging Go needs Delve: go install github.com/go-delve/delve/cmd/dlv@latest")?;
    let port = crate::dap::free_port().map_err(|e| e.to_string())?;
    let args = vec!["dap".into(), format!("--listen=127.0.0.1:{port}")];
    Ok(Adapter { id: "go", program, args, port: Some(port) })
}

fn debugpy(cwd: &Path) -> Result<Adapter, String> {
    let python = python_for(cwd).ok_or("Debugging Python needs python3 on PATH.")?;
    let has_debugpy = Command::new(&python)
        .args(["-c", "import debugpy"])
        .env("PATH", servers::search_path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has_debugpy {
        return Err(format!("Debugging Python needs debugpy: {} -m pip install debugpy", python.display()));
    }
    Ok(Adapter { id: "python", program: python, args: vec!["-m".into(), "debugpy.adapter".into()], port: None })
}

/// Microsoft's js-debug, as its standalone DAP server: `js-debug-adapter` on
/// PATH (as Mason installs it), else a release unpacked in the config folder.
fn js_debug() -> Result<Adapter, String> {
    let port = crate::dap::free_port().map_err(|e| e.to_string())?;
    let listen = vec![port.to_string(), "127.0.0.1".to_string()];
    if let Some(program) = servers::which("js-debug-adapter") {
        return Ok(Adapter { id: "pwa-node", program, args: listen, port: Some(port) });
    }
    let server = crate::theme::config_dir().map(|d| d.join("debug-adapters/js-debug/src/dapDebugServer.js")).filter(|p| p.is_file());
    let Some(server) = server else {
        return Err("Debugging JavaScript needs js-debug: unpack js-debug-dap-*.tar.gz from github.com/microsoft/vscode-js-debug/releases \
                    into ~/.config/kin/debug-adapters (or put js-debug-adapter on PATH)."
            .into());
    };
    let node = servers::which("node").ok_or("Debugging JavaScript needs Node.js on PATH.")?;
    let mut args = vec![server.to_string_lossy().into_owned()];
    args.extend(listen);
    Ok(Adapter { id: "pwa-node", program: node, args, port: Some(port) })
}

/// Go executables carry build info that `go version` reads: "<path>: go1.22.1".
/// (For others it says why not, and still exits with 0.)
fn is_go_program(program: &Path) -> bool {
    let Some(go) = servers::which("go") else { return false };
    let Ok(output) = Command::new(go).arg("version").arg(program).stderr(Stdio::null()).output() else { return false };
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim().rsplit(": ").next().is_some_and(|version| version.starts_with("go") || version.starts_with("devel"))
}

/// Xcode ships lldb-dap without putting it on PATH.
fn xcrun_lldb_dap() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = Command::new("xcrun").args(["-f", "lldb-dap"]).stderr(Stdio::null()).output().ok()?;
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    (output.status.success() && path.is_file()).then_some(path)
}

/// The project's virtualenv Python, else python3 on PATH.
fn python_for(cwd: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        for venv in [".venv", "venv"] {
            let python = dir.join(venv).join("bin/python");
            if python.is_file() {
                return Some(python);
            }
        }
    }
    servers::which("python3")
}

/// `initialize` arguments (what Kin supports as a client).
pub fn initialize_arguments(adapter_id: &str) -> Value {
    json!({
        "clientID": "kin",
        "clientName": "Kin",
        "adapterID": adapter_id,
        "pathFormat": "path",
        "linesStartAt1": true,
        "columnsStartAt1": true,
        "supportsVariableType": true,
        "supportsRunInTerminalRequest": false,
        // js-debug debugs each Node process in a child session.
        "supportsStartDebuggingRequest": adapter_id == "pwa-node",
        "locale": "en",
    })
}

/// The request that starts debugging: `attach` to a running process, else `launch`.
pub fn request(spec: &DebugSpec) -> &'static str {
    if matches!(spec, DebugSpec::Attach { .. }) { "attach" } else { "launch" }
}

/// LLDB setup: Step Into skips the standard library (it ships with line info,
/// and lldb only avoids `std::` by default; legacy symbols name trait impls
/// `_$LT$alloc..vec..Vec…`), and Rust's own formatters, so String, Vec and
/// Option read naturally.
fn lldb_commands(cwd: &Path) -> Vec<String> {
    let mut commands = vec![r"settings set target.process.thread.step-avoid-regexp ^(<|_\$LT\$)?(std|core|alloc)(::|\.\.)".to_string()];
    commands.extend(rust_formatters(cwd).unwrap_or_default());
    commands
}

/// Arguments for [`request`]. `program` is the executable a cargo build
/// made; `adapter_id` says which adapter attaches.
pub fn launch_arguments(spec: &DebugSpec, cwd: &Path, program: Option<&Path>, adapter_id: &str) -> Value {
    match spec {
        DebugSpec::Cargo { args, .. } => json!({
            "program": program.map(|p| p.to_string_lossy().into_owned()),
            "args": args,
            "cwd": cwd,
            "stopOnEntry": false,
            "initCommands": lldb_commands(cwd),
        }),
        DebugSpec::Go { mode, program, args } => json!({
            "mode": mode,
            "program": program,
            "args": args,
            "cwd": cwd,
            // Program output as DAP output events, not Delve's own stdout.
            "outputMode": "remote",
            // The build goes to the temp folder, not the project (Delve removes it after).
            "output": std::env::temp_dir().join(format!("kin-debug-{}-{}", std::process::id(), unique())),
        }),
        DebugSpec::Python { program, module, args } => {
            let mut launch = json!({
                "args": args,
                "cwd": cwd,
                "console": "internalConsole",
                "justMyCode": true,
                "redirectOutput": true,
            });
            if let Some(program) = program {
                launch["program"] = json!(program);
            }
            if let Some(module) = module {
                launch["module"] = json!(module);
            }
            launch
        }
        // js-debug reads `request` from the configuration, not the command.
        DebugSpec::Node { runtime, args } => json!({
            "type": "pwa-node",
            "request": "launch",
            "runtimeExecutable": runtime,
            "runtimeArgs": args,
            "cwd": cwd,
            // The program's stdout and stderr as output events.
            "console": "internalConsole",
            "outputCapture": "std",
            "skipFiles": ["<node_internals>/**"],
            // Source maps for the project's code (TypeScript), not its dependencies'.
            "resolveSourceMapLocations": ["**", "!**/node_modules/**"],
        }),
        DebugSpec::Attach { pid, program } => match adapter_id {
            "go" => json!({ "mode": "local", "processId": pid }),
            "python" => json!({ "processId": pid, "justMyCode": true }),
            // The inspector's port comes from `open_node_inspector`.
            "pwa-node" => json!({ "type": "pwa-node", "request": "attach", "skipFiles": ["<node_internals>/**"] }),
            // The executable too, or lldb finds no debug info for it.
            _ => json!({ "pid": pid, "program": program, "initCommands": lldb_commands(cwd) }),
        },
    }
}

/// Node listens for a debugger after a SIGUSR1 (js-debug's own server
/// doesn't send it, unlike VS Code): sends it and returns the inspector's
/// port, from `--inspect…` flags or Node's default.
pub fn open_node_inspector(pid: u32) -> u16 {
    let pid = pid.to_string();
    let command = Command::new("ps").args(["-o", "args=", "-p", &pid]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let _ = Command::new("kill").args(["-USR1", &pid]).status();
    inspector_port(&command.unwrap_or_default()).unwrap_or(9229)
}

/// The port of `--inspect[-brk|-wait|-port][=[host:]port]`.
fn inspector_port(command: &str) -> Option<u16> {
    command.split_whitespace().find_map(|arg| {
        let rest = ["--inspect-brk", "--inspect-wait", "--inspect-port", "--inspect"].iter().find_map(|flag| arg.strip_prefix(flag))?;
        match rest.strip_prefix('=') {
            Some(value) => value.rsplit(':').next()?.parse().ok(),
            None => rest.is_empty().then_some(9229),
        }
    })
}

/// Different on every call (a Go build's name).
fn unique() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

/// A running process of the user's, for Attach to Process.
pub struct Process {
    pub pid: u32,
    /// The executable's file name.
    pub name: String,
    /// The command line.
    pub command: String,
    pub program: PathBuf,
}

/// The user's processes, newest first (from `ps`), without Kin itself.
pub fn processes() -> Vec<Process> {
    let column = |name: &str| -> Vec<(u32, String)> {
        let Ok(output) = Command::new("ps").args(["x", "-o", "pid=", "-o"]).arg(format!("{name}=")).output() else { return Vec::new() };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let (pid, rest) = line.trim_start().split_once(' ')?;
                Some((pid.parse().ok()?, rest.trim().to_string()))
            })
            .collect()
    };
    let commands: std::collections::HashMap<u32, String> = column("args").into_iter().collect();
    let mut processes: Vec<Process> = column("comm")
        .into_iter()
        .filter(|(pid, _)| *pid != std::process::id())
        .map(|(pid, comm)| {
            // Linux's `comm` is a short name; the real path is in /proc.
            let program = std::fs::read_link(format!("/proc/{pid}/exe")).unwrap_or_else(|_| PathBuf::from(&comm));
            let name = program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(comm);
            let command = commands.get(&pid).cloned().unwrap_or_default();
            Process { pid, name, command, program }
        })
        .collect();
    processes.sort_by(|a, b| b.pid.cmp(&a.pid));
    processes
}

/// LLDB commands that load the toolchain's Rust formatters.
fn rust_formatters(cwd: &Path) -> Option<Vec<String>> {
    let output = Command::new("rustc")
        .args(["--print", "sysroot"])
        .current_dir(cwd)
        .env("PATH", servers::search_path())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let etc = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()).join("lib/rustlib/etc");
    let (lookup, commands) = (etc.join("lldb_lookup.py"), etc.join("lldb_commands"));
    (lookup.is_file() && commands.is_file()).then(|| {
        vec![
            format!("command script import \"{}\"", lookup.display()),
            format!("command source -s 0 \"{}\"", commands.display()),
        ]
    })
}

/// A running cargo build, which Stop kills.
pub type Build = Arc<Mutex<Option<std::process::Child>>>;

/// Builds with cargo (`build …` or `test --no-run …`) and returns the
/// executable it made. Cargo's progress and compiler messages go to
/// `on_line` as they come. Blocks: run it off the main thread.
pub fn cargo_build(cwd: &Path, args: &[String], build: &Build, mut on_line: impl FnMut(String) + Send + 'static) -> Result<PathBuf, String> {
    let mut command = Command::new("cargo");
    command
        .args(args)
        .arg("--message-format=json-render-diagnostics")
        .current_dir(cwd)
        .env("PATH", servers::search_path())
        .env("CARGO_TERM_COLOR", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("couldn't run cargo: {e}"))?;
    let stdout = child.stdout.take().ok_or("no output from cargo")?;
    let stderr = child.stderr.take().ok_or("no output from cargo")?;
    *build.lock().unwrap() = Some(child);
    // Progress and rendered diagnostics come on stderr.
    let progress = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            on_line(line);
        }
    });
    let artifacts: Vec<Value> = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["executable"].is_string())
        .collect();
    let _ = progress.join();
    let status = build.lock().unwrap().take().map(|mut child| child.wait());
    match status {
        Some(Ok(status)) if status.success() => {}
        Some(Ok(_)) => return Err("The build failed".into()),
        Some(Err(err)) => return Err(err.to_string()),
        None => return Err("The build was stopped".into()),
    }
    pick_executable(&artifacts, args).ok_or_else(|| "cargo didn't build an executable for this".into())
}

/// The executable among cargo's artifacts: the test binary for a test
/// build, else the binary asked for by name (or the last one).
fn pick_executable(artifacts: &[Value], args: &[String]) -> Option<PathBuf> {
    let test = args.first().is_some_and(|a| a == "test");
    let wanted = args.iter().position(|a| a == "--bin" || a == "--example").and_then(|i| args.get(i + 1));
    let candidates: Vec<&Value> = artifacts.iter().filter(|a| a["profile"]["test"].as_bool() == Some(test)).collect();
    let named = wanted.and_then(|name| candidates.iter().rev().find(|a| a["target"]["name"].as_str() == Some(name.as_str())));
    named.or(candidates.last()).and_then(|a| a["executable"].as_str()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_executable_cargo_made() {
        let artifacts = vec![
            json!({"target": {"name": "kin", "kind": ["bin"]}, "profile": {"test": false}, "executable": "/t/debug/kin"}),
            json!({"target": {"name": "helper", "kind": ["bin"]}, "profile": {"test": false}, "executable": "/t/debug/helper"}),
            json!({"target": {"name": "kin", "kind": ["bin"]}, "profile": {"test": true}, "executable": "/t/debug/deps/kin-1a2b"}),
        ];
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(pick_executable(&artifacts, &args(&["build", "--bin", "kin"])), Some("/t/debug/kin".into()));
        assert_eq!(pick_executable(&artifacts, &args(&["build"])), Some("/t/debug/helper".into()));
        assert_eq!(pick_executable(&artifacts, &args(&["test", "--no-run", "--bin", "kin"])), Some("/t/debug/deps/kin-1a2b".into()));
    }

    #[test]
    fn launch_arguments_per_adapter() {
        let go = DebugSpec::Go { mode: "test", program: "/p/pkg".into(), args: vec!["-test.run".into(), "^TestA$".into()] };
        let launch = launch_arguments(&go, Path::new("/p/pkg"), None, "go");
        assert_eq!((launch["mode"].as_str(), launch["args"][1].as_str()), (Some("test"), Some("^TestA$")));
        let py = DebugSpec::Python { program: None, module: Some("pytest".into()), args: vec!["t.py::test_a".into()] };
        let launch = launch_arguments(&py, Path::new("/p"), None, "python");
        assert_eq!((launch["module"].as_str(), launch.get("program")), (Some("pytest"), None));
        let node = DebugSpec::Node { runtime: "npm".into(), args: vec!["run".into(), "dev".into()] };
        let launch = launch_arguments(&node, Path::new("/p"), None, "pwa-node");
        assert_eq!((launch["type"].as_str(), launch["runtimeArgs"][1].as_str()), (Some("pwa-node"), Some("dev")));
        let attach = DebugSpec::Attach { pid: 42, program: "/usr/local/bin/node".into() };
        assert_eq!(request(&attach), "attach");
        assert_eq!(launch_arguments(&attach, Path::new("/p"), None, "pwa-node")["request"], "attach");
        assert_eq!(inspector_port("node --inspect=127.0.0.1:9333 server.js"), Some(9333));
        assert_eq!(inspector_port("node --inspect-brk server.js"), Some(9229));
        assert_eq!(inspector_port("node server.js --inspector"), None);
        assert_eq!(launch_arguments(&attach, Path::new("/p"), None, "go")["processId"], 42);
        assert_eq!(launch_arguments(&attach, Path::new("/p"), None, "lldb-dap")["pid"], 42);
    }

    #[test]
    fn lists_processes() {
        let all = processes();
        assert!(all.iter().all(|p| p.pid != std::process::id()));
        assert!(all.windows(2).all(|w| w[0].pid > w[1].pid));
        assert!(all.iter().any(|p| !p.name.is_empty() && !p.command.is_empty()), "no processes from ps");
    }
}
