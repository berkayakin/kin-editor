use std::process::Command;

fn main() {
    commit();

    // Embed the app icon as resource 1, which GPUI loads for the window and taskbar.
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rerun-if-changed=assets/app-icon/windows/kuana.ico");
        winresource::WindowsResource::new()
            .set_icon("assets/app-icon/windows/kuana.ico")
            .compile()
            .expect("failed to embed Windows icon");
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The commit Kuana is built from, shown in About Kuana as `KUANA_COMMIT`. Nothing
/// is set outside a git checkout (e.g. a source tarball).
fn commit() {
    let Some(hash) = git(&["rev-parse", "--short", "HEAD"]) else { return };
    println!("cargo:rustc-env=KUANA_COMMIT={hash}");
    // Rerun when HEAD moves to another branch or its branch gets a commit
    // (a packed ref has no file, which makes Cargo rerun every build; cheap).
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]).and_then(|r| git(&["rev-parse", "--git-path", &r])) {
        println!("cargo:rerun-if-changed={branch}");
    }
}
