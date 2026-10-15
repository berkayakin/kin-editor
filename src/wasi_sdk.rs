//! wasi-sdk: the clang that compiles extensions' grammars to WebAssembly,
//! which tree-sitter runs in wasmtime instead of loading as native code.
//! Downloaded once, when a grammar first needs it and the user agrees, into
//! `<config>/toolchains/` (pinned release, SHA-256-checked, trimmed to what
//! compiling a grammar uses). `WASI_SDK_PATH` names one already installed,
//! as for tree-sitter's CLI.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

const VERSION: &str = "34.0";
const TAG: &str = "wasi-sdk-34";

/// This computer's release archive: its platform name, SHA-256 and size in MB.
fn asset() -> Option<(&'static str, &'static str, u32)> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("arm64-macos", "9c59398106b417f8f14913380fdf0097a8cc0ff4af9eb3ce0065a859e88d49e9", 180),
        ("macos", "x86_64") => ("x86_64-macos", "87d27fa8adc68dee59bfbf2e22a6d34ef717c34d6bf1d8af2a56fc929d9ce0eb", 182),
        ("linux", "x86_64") => ("x86_64-linux", "b761e3a0721dbae9c09a0059e5fdb2bf917d1b4a8a7b430fb3b5aafb0984b2c4", 192),
        ("linux", "aarch64") => ("arm64-linux", "f7e243dff54d60bcc576e94d6166b69f410f2500ae4a9ceef34315be10e77971", 192),
        ("windows", "x86_64") => ("x86_64-windows", "cccb5c323a9b34f0349a9b09e8804a0a7632c68c3310f4b5f437ed57d7e71d8f", 619),
        ("windows", "aarch64") => ("arm64-windows", "45e1c71f3e965621e7b98ebe1d37b0e4b1f77f3e8072113ffb4534e67b1a4b7c", 620),
        _ => return None,
    })
}

/// The download's size in MB, for asking first; None where there's no build.
pub fn download_mb() -> Option<u32> {
    asset().map(|(_, _, mb)| mb)
}

fn home() -> Option<PathBuf> {
    crate::theme::config_dir().map(|d| d.join("toolchains").join(format!("wasi-sdk-{VERSION}")))
}

fn clang_in(dir: &Path) -> Option<PathBuf> {
    let clang = dir.join("bin").join(format!("clang{}", std::env::consts::EXE_SUFFIX));
    clang.is_file().then_some(clang)
}

/// The compiler, if there is one.
pub fn clang() -> Option<PathBuf> {
    std::env::var_os("WASI_SDK_PATH").and_then(|dir| clang_in(Path::new(&dir))).or_else(|| clang_in(&home()?))
}

static INSTALLING: AtomicBool = AtomicBool::new(false);

pub fn installing() -> bool {
    INSTALLING.load(Ordering::Relaxed)
}

/// Downloads and unpacks it. Blocking (a few minutes on a slow connection).
pub fn install() -> Result<(), String> {
    if INSTALLING.swap(true, Ordering::SeqCst) {
        return Err("wasi-sdk is already being downloaded".into());
    }
    let result = home().ok_or_else(|| "Kin has no config folder".to_string()).and_then(|home| install_into(&home));
    INSTALLING.store(false, Ordering::SeqCst);
    result
}

fn install_into(home: &Path) -> Result<(), String> {
    let (platform, sha256, _) = asset().ok_or("wasi-sdk has no build for this computer")?;
    let parent = home.parent().ok_or("no folder for toolchains")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let name = format!("wasi-sdk-{VERSION}-{platform}.tar.gz");
    let temp = parent.join(format!("{name}.download"));
    crate::updater::download(&format!("https://github.com/WebAssembly/wasi-sdk/releases/download/{TAG}/{name}"), &temp)?;
    if crate::updater::sha256_hex(&temp).as_deref() != Some(sha256) {
        std::fs::remove_file(&temp).ok();
        return Err(format!("{name} didn't match its checksum"));
    }
    // Unpacked beside its place, then moved there: a half-unpacked one is never used.
    let unpacking = parent.join(".unpacking");
    std::fs::remove_dir_all(&unpacking).ok();
    std::fs::create_dir_all(&unpacking).map_err(|e| e.to_string())?;
    let unpacked = crate::lsp::install::unzip(&temp, &unpacking);
    std::fs::remove_file(&temp).ok();
    let result = unpacked.and_then(|()| {
        let top = unpacking.join(format!("wasi-sdk-{VERSION}-{platform}"));
        clang_in(&top).ok_or_else(|| format!("{name} has no clang"))?;
        trim(&top);
        std::fs::remove_dir_all(home).ok();
        std::fs::rename(&top, home).map_err(|e| e.to_string())
    });
    std::fs::remove_dir_all(&unpacking).ok();
    result
}

/// Removes what compiling a grammar for `wasm32-wasip1` without a C
/// library doesn't use: other targets' headers, every prebuilt library, the
/// debugger and libclang (about 600 MB → 170 MB).
fn trim(dir: &Path) {
    let sysroot = dir.join("share/wasi-sysroot");
    let mut gone = vec![
        sysroot.join("lib"),
        sysroot.join("experimental-coop-threads"),
        sysroot.join("include/c++"),
        sysroot.join("include/wasm32-wasip1-threads"),
        sysroot.join("include/wasm32-wasip2"),
        sysroot.join("include/wasm32-wasip3"),
        dir.join(format!("bin/clang-tidy{}", std::env::consts::EXE_SUFFIX)),
    ];
    for folder in [dir.join("lib"), dir.join("bin")] {
        let entries = std::fs::read_dir(&folder).into_iter().flatten().flatten();
        // The debugger, and libclang (the C API for tools; clang doesn't use it).
        let unused = |name: &str| name.starts_with("liblldb") || name.starts_with("lldb") || name.starts_with("libclang.");
        gone.extend(entries.map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| unused(&n.to_string_lossy()))));
    }
    for path in gone {
        if path.is_dir() {
            std::fs::remove_dir_all(&path).ok();
        } else {
            std::fs::remove_file(&path).ok();
        }
    }
}

/// Compiles a grammar's `parser.c` (and C scanner) to `output`: a module
/// exporting `tree_sitter_<name>`, whose C library calls tree-sitter's
/// runtime provides.
pub fn compile(clang: &Path, src: &Path, name: &str, output: &Path) -> Result<(), String> {
    let mut command = crate::process::command(clang);
    command
        .args(["-fPIC", "-shared", "-Os", "-w"])
        .arg(format!("-Wl,--export=tree_sitter_{}", name.replace('-', "_")))
        .args(["-Wl,--allow-undefined", "-Wl,--no-entry", "-nostdlib", "-fno-exceptions", "-fvisibility=hidden"])
        .arg("-I")
        .arg(src)
        .arg(src.join("parser.c"));
    if src.join("scanner.c").is_file() {
        command.arg(src.join("scanner.c"));
    }
    let out = command
        .arg("-o")
        .arg(output)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("couldn't run {}: {e}", clang.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(stderr.lines().find(|l| l.contains("error")).unwrap_or("the compiler failed").trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs wasi-sdk (`WASI_SDK_PATH`) and a bash grammar's sources
    /// (`GRAMMAR_SRC`): compiles them and loads the result as WebAssembly.
    #[test]
    #[ignore = "needs wasi-sdk (WASI_SDK_PATH) and a grammar's sources (GRAMMAR_SRC); run with --ignored"]
    fn compiles_a_grammar_that_loads() {
        let clang = clang().expect("set WASI_SDK_PATH");
        let src = std::env::var("GRAMMAR_SRC").map(PathBuf::from).expect("set GRAMMAR_SRC to a grammar's src folder");
        let out = std::env::temp_dir().join(format!("kin-wasm-{}.wasm", std::process::id()));
        compile(&clang, &src, "bash", &out).unwrap();
        let language = crate::extensions::load_wasm(&out, "bash").unwrap();
        let mut parser = tree_sitter::Parser::new();
        crate::extensions::prepare_parser(&mut parser, &language);
        parser.set_language(&language).unwrap();
        let tree = parser.parse("if true; then echo hi; fi\n", None).unwrap();
        assert!(!tree.root_node().has_error(), "{}", tree.root_node().to_sexp());
        std::fs::remove_file(&out).ok();
    }
}
