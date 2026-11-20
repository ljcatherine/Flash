// Embed a manifest so Windows shows the UAC admin prompt on launch (driver installs need admin).
// `-bins` keeps it off the test binary, so `cargo test` still runs unelevated.
fn main() {
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'");
    // Explorer/taskbar icon for flash.exe (the window icon is set in main.rs).
    winresource::WindowsResource::new().set_icon("assets/flash.ico").compile().expect("failed to embed icon");
}
