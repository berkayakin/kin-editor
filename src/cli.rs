//! Opening Kin from outside: Finder's Open With and double-clicks (macOS
//! open-file events, as `file://` URLs) and the `kin` shell command, a small
//! script that hands its paths to the app with `open`, so they reach the
//! running Kin the same way.

use std::path::{Path, PathBuf};

/// Where Install Shell Command puts the script.
pub const COMMAND_PATH: &str = "/usr/local/bin/kin";

/// The local paths among URLs macOS asked Kin to open.
pub fn paths_from_urls(urls: &[String]) -> Vec<PathBuf> {
    urls.iter()
        .filter_map(|u| url::Url::parse(u).ok())
        .filter(|u| u.scheme() == "file")
        .filter_map(|u| u.to_file_path().ok())
        .collect()
}

/// The `kin` script for this copy of Kin: through `open` when it runs from
/// an app bundle (paths reach the running app), else the binary itself.
pub fn script(exe: &Path) -> String {
    let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', r"'\''"));
    let app = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app"));
    let run = match app {
        Some(app) => format!("exec open -a {} \"$@\"", quote(app)),
        None => format!("{} \"$@\" >/dev/null 2>&1 &", quote(exe)),
    };
    format!("#!/bin/sh\n# Opens files and folders in Kin: kin [path…]. Installed by Kin.\n{run}\n")
}

/// Writes the script to [`COMMAND_PATH`], asking for an administrator
/// password when that folder isn't writable (as Zed and VS Code do).
pub fn install() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let text = script(&exe);
    let target = Path::new(COMMAND_PATH);
    let direct = target.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(target, &text));
    if direct.is_ok() {
        return set_executable(target);
    }
    if !cfg!(target_os = "macos") {
        return Err(format!("couldn't write {COMMAND_PATH}: {}", direct.unwrap_err()));
    }
    let staged = std::env::temp_dir().join(format!("kin-command-{}", std::process::id()));
    std::fs::write(&staged, &text).map_err(|e| e.to_string())?;
    let shell = format!(
        "mkdir -p /usr/local/bin && cp '{}' {COMMAND_PATH} && chmod 755 {COMMAND_PATH}",
        staged.display()
    );
    let apple_script = format!("do shell script \"{shell}\" with administrator privileges");
    let output = crate::process::command("osascript").arg("-e").arg(apple_script).output().map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&staged);
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if message.contains("-128") { "Cancelled".into() } else { message })
    }
}

fn set_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_and_urls() {
        let bundled = script(Path::new("/Applications/Kin's.app/Contents/MacOS/kin"));
        assert!(bundled.contains(r#"exec open -a '/Applications/Kin'\''s.app' "$@""#), "{bundled}");
        let dev = script(Path::new("/src/kin/target/release/kin"));
        assert!(dev.contains(r#"'/src/kin/target/release/kin' "$@""#), "{dev}");
        let urls = vec!["file:///Users/a/My%20Project/".to_string(), "https://example.com".to_string()];
        assert_eq!(paths_from_urls(&urls), vec![PathBuf::from("/Users/a/My Project/")]);
    }
}
