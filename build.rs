fn main() {
    // Embed the app icon as resource 1, which GPUI loads for the window and taskbar.
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rerun-if-changed=assets/app-icon/windows/kin.ico");
        winresource::WindowsResource::new()
            .set_icon("assets/app-icon/windows/kin.ico")
            .compile()
            .expect("failed to embed Windows icon");
    }
}
