// Windows: embed a manifest so Windows shows the UAC admin prompt on launch (driver installs
// need admin). `-bins` keeps it off the test binary, so `cargo test` still runs unelevated.
// Linux: nothing to embed. Flash runs as the user and elevates single actions with polkit.
fn main() {
    println!("cargo:rerun-if-changed=assets/flash.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'");
    // A Windows build on a non-Windows host (only used to type-check the Windows target)
    // has no resource compiler; real releases are built on Windows by release.ps1.
    if !cfg!(windows) {
        println!("cargo:warning=not embedding the icon: cross-compiling from a non-Windows host");
        return;
    }
    // Explorer/taskbar icon for flash.exe (the window icon is set in main.rs).
    // FileDescription is what Task Manager and the UAC prompt show.
    winresource::WindowsResource::new()
        .set_icon("assets/flash.ico")
        .set("FileDescription", "Flash")
        .set("ProductName", "Flash")
        .compile()
        .expect("failed to embed icon");
}
