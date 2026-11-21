//! Everything that talks to the operating system. `windows.rs` and `linux/` export the same
//! functions, so the shared code above them compiles only when both platforms implement
//! every one: a missing Linux (or Windows) function is a build error, not a missing button.
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("Flash supports Windows and Linux.");

/// System tools Flash opens with a button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    /// Device Manager on Windows; a hardware information app on Linux.
    DeviceManager,
    /// System Restore on Windows; Snapper or Timeshift on Linux.
    SystemRestore,
    /// Event Viewer on Windows; a journal viewer on Linux.
    EventViewer,
    /// Windows Memory Diagnostic; memtest86+ at the next boot on Linux.
    MemoryTest,
    /// Task Manager's startup tab; the desktop's autostart settings on Linux.
    StartupManager,
}

/// How this copy of Flash was installed, which decides how it updates itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// Windows Installer package.
    Msi,
    /// The portable or standalone Windows executable.
    Exe,
    /// A Linux AppImage at this path (replaced in place).
    AppImage(std::path::PathBuf),
    /// The Debian/Ubuntu package.
    Deb,
    /// The Fedora/openSUSE package.
    Rpm,
    /// The binary from the Linux tarball, replaced in place.
    Tarball(std::path::PathBuf),
    /// Installed by something else (a distribution, Flatpak, Nix): says how to update.
    Managed(&'static str),
}
