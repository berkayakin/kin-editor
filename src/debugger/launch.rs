//! What a debug session runs: the adapter for a [`DebugSpec`] (lldb-dap for
//! Rust, Delve for Go, debugpy for Python; found on PATH or through Xcode,
//! never downloaded), the cargo build that makes a Rust executable, and the
//! `launch` arguments each adapter expects.

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
        DebugSpec::Cargo { .. } => {
            let program = servers::which("lldb-dap")
                .or_else(xcrun_lldb_dap)
                .or_else(|| servers::which("lldb-vscode"))
                .ok_or("Debugging Rust needs lldb-dap: install Xcode's command line tools (xcode-select --install) or LLVM.")?;
            Ok(Adapter { id: "lldb-dap", program, args: Vec::new(), port: None })
        }
        DebugSpec::Go { .. } => {
            let program = servers::which("dlv")
                .ok_or("Debugging Go needs Delve: go install github.com/go-delve/delve/cmd/dlv@latest")?;
            let port = crate::dap::free_port().map_err(|e| e.to_string())?;
            let args = vec!["dap".into(), format!("--listen=127.0.0.1:{port}")];
            Ok(Adapter { id: "go", program, args, port: Some(port) })
        }
        DebugSpec::Python { .. } => {
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
    }
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
        "locale": "en",
    })
}

/// `launch` arguments. `program` is the executable a cargo build made.
pub fn launch_arguments(spec: &DebugSpec, cwd: &Path, program: Option<&Path>) -> Value {
    match spec {
        DebugSpec::Cargo { args, .. } => {
            let mut launch = json!({
                "program": program.map(|p| p.to_string_lossy().into_owned()),
                "args": args,
                "cwd": cwd,
                "stopOnEntry": false,
            });
            // Step Into skips the standard library (it ships with line info, and
            // lldb only avoids `std::` by default). Legacy symbols name trait
            // impls `_$LT$alloc..vec..Vec…`.
            let mut commands =
                vec![r"settings set target.process.thread.step-avoid-regexp ^(<|_\$LT\$)?(std|core|alloc)(::|\.\.)".to_string()];
            // Rust's own LLDB formatters, so String, Vec and Option read naturally.
            commands.extend(rust_formatters(cwd).unwrap_or_default());
            launch["initCommands"] = json!(commands);
            launch
        }
        DebugSpec::Go { mode, program, args } => json!({
            "mode": mode,
            "program": program,
            "args": args,
            "cwd": cwd,
            // Program output as DAP output events, not Delve's own stdout.
            "outputMode": "remote",
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
    }
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
        let launch = launch_arguments(&go, Path::new("/p/pkg"), None);
        assert_eq!((launch["mode"].as_str(), launch["args"][1].as_str()), (Some("test"), Some("^TestA$")));
        let py = DebugSpec::Python { program: None, module: Some("pytest".into()), args: vec!["t.py::test_a".into()] };
        let launch = launch_arguments(&py, Path::new("/p"), None);
        assert_eq!((launch["module"].as_str(), launch.get("program")), (Some("pytest"), None));
    }
}
