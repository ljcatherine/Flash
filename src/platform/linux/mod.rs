//! Linux: sysfs and the udev database for devices, the distribution's package manager,
//! fwupd/LVFS for firmware, the kernel log for hardware errors, udisks2 for drive health,
//! systemd for the weekly scan and startup apps. Flash runs as the user; anything that
//! needs root goes through `flash-helper` (see helper/) with polkit, one action at a time.
mod devices;
mod fwupd;
mod health;
mod monitor;
mod packages;
mod sources;
mod storage;
mod system;
mod update;

pub use devices::{is_builtin, problem_fix, problem_text, purpose, read_devices, rescan_hardware};
pub use health::{gpu_latest, health_report, whea_read};
pub use monitor::{gpu_sampler, sensors};
pub use sources::{can_undo, check_source, export_drivers, install, install_note, rollback, snapshot_status};
pub use storage::{breakdown_plan, clean, find_junk};
pub use system::{schedule_enabled, set_schedule, set_startup, startup_apps};
pub use update::{apply_update, install_kind, installed_exe, update_dir};

use super::Tool;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Runs a program; returns stdout and stderr combined.
pub fn run_hidden(program: &str, args: &[&str]) -> String {
    match Command::new(program).args(args).stdin(Stdio::null()).output() {
        Ok(o) => format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
        Err(e) => format!("cannot start {program}: {e}"),
    }
}

/// Runs a program: stdout when it succeeds, stderr (or the exit status) when it doesn't.
pub(crate) fn cmd(program: &str, args: &[&str]) -> Result<String, String> {
    let o = Command::new(program).args(args).stdin(Stdio::null()).env("LC_ALL", "C").output().map_err(|e| format!("cannot start {program}: {e}"))?;
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    if o.status.success() {
        Ok(out)
    } else {
        let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
        Err(if err.is_empty() { format!("{program} failed ({})", o.status) } else { err })
    }
}

/// Like `cmd`, but returns stdout whatever the exit status (for tools like `dnf check-update`
/// whose exit code means "updates available").
pub(crate) fn cmd_any(program: &str, args: &[&str]) -> Option<(String, i32)> {
    let o = Command::new(program).args(args).stdin(Stdio::null()).env("LC_ALL", "C").output().ok()?;
    Some((String::from_utf8_lossy(&o.stdout).into_owned(), o.status.code().unwrap_or(-1)))
}

/// Whether a program is on PATH.
pub(crate) fn has(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(program).is_file()))
}

pub(crate) fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// /etc/os-release as a map (ID, ID_LIKE, PRETTY_NAME...).
pub(crate) fn os_release() -> BTreeMap<String, String> {
    let text = std::fs::read_to_string("/etc/os-release").or_else(|_| std::fs::read_to_string("/usr/lib/os-release")).unwrap_or_default();
    parse_os_release(&text)
}

pub(crate) fn parse_os_release(text: &str) -> BTreeMap<String, String> {
    text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string())).collect()
}

pub(crate) fn is_nixos() -> bool {
    Path::new("/etc/NIXOS").exists()
}

pub(crate) fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_absolute())
}

fn xdg(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).or_else(|| home().map(|h| h.join(fallback)))
}

pub(crate) fn config_dir() -> Option<PathBuf> {
    xdg("XDG_CONFIG_HOME", ".config")
}

pub(crate) fn cache_dir() -> Option<PathBuf> {
    xdg("XDG_CACHE_HOME", ".cache")
}

/// Unix seconds now.
pub(crate) fn now() -> u64 {
    crate::extras::now()
}

// ---------------- The privileged helper ----------------

/// Where flash-helper is: next to a packaged install (`../libexec/flash/`), the system
/// locations, or beside the binary (tarball, AppImage).
pub(crate) fn helper_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let candidates = [
        dir.join("../libexec/flash/flash-helper"),
        PathBuf::from("/usr/libexec/flash/flash-helper"),
        PathBuf::from("/usr/lib/flash/flash-helper"),
        dir.join("flash-helper"),
    ];
    let found = candidates.into_iter().find(|p| p.is_file()).ok_or("flash-helper is missing from this installation, so Flash can't make system changes.")?;
    let found = std::fs::canonicalize(&found).unwrap_or(found);
    // Inside an AppImage the binary sits on the user's FUSE mount, which root can't read:
    // give pkexec a copy in the user's private runtime directory instead.
    if let Some(appdir) = std::env::var_os("APPDIR").map(PathBuf::from) {
        if found.starts_with(&appdir) {
            use std::os::unix::fs::PermissionsExt;
            let run = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).ok_or("XDG_RUNTIME_DIR isn't set")?.join("flash");
            std::fs::create_dir_all(&run).map_err(|e| e.to_string())?;
            std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).ok();
            let copy = run.join("flash-helper");
            std::fs::copy(&found, &copy).map_err(|e| e.to_string())?;
            std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o700)).ok();
            return Ok(copy);
        }
    }
    Ok(found)
}

/// Whether Flash's polkit policy is installed. Without it pkexec asks for an administrator
/// password for every action, so the optional read-only ones (SMART, DMI, crash logs) are skipped.
pub(crate) fn policy_installed() -> bool {
    let name = "polkit-1/actions/dev.liamjc.flash.policy";
    let exe_share = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("../share").join(name)));
    ["/usr/share", "/usr/local/share", "/run/current-system/sw/share", "/etc"].iter().map(|d| Path::new(d).join(name)).chain(exe_share).any(|p| p.is_file())
}

/// A read-only helper action that needs no password, only when that's guaranteed.
pub(crate) fn helper_read(verb: &str, args: &[&str]) -> Option<String> {
    policy_installed().then(|| helper("read", verb, args).ok()).flatten()
}

/// Runs one helper action as root through pkexec. `group` is `read` (no password for the
/// active user), `logs` or `manage` (administrator password, remembered for a few minutes).
pub(crate) fn helper(group: &str, verb: &str, args: &[&str]) -> Result<String, String> {
    if !has("pkexec") {
        return Err("pkexec (polkit) isn't installed, so Flash can't make system changes.".into());
    }
    let path = helper_path()?;
    let mut all = vec![group, verb];
    all.extend_from_slice(args);
    let o = Command::new("pkexec").arg(&path).args(&all).stdin(Stdio::null()).output().map_err(|e| format!("cannot start pkexec: {e}"))?;
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
    match o.status.code() {
        Some(0) => Ok(out),
        Some(126) => Err("The password prompt was closed, so nothing changed.".into()),
        Some(127) => Err(if err.is_empty() { "Not authorized: an administrator has to approve this.".into() } else { err }),
        _ => Err(if err.is_empty() { out } else { err }),
    }
}

// ---------------- Opening things ----------------

/// Opens a URL or file with its default app. `flash:software-updates` opens the
/// distribution's software updater.
pub fn open(target: &str) {
    if target == "flash:software-updates" {
        launch(&[("plasma-discover", &["--mode", "update"]), ("gnome-software", &["--mode", "updates"]), ("mintupdate", &[]), ("update-manager", &[]), ("pamac-manager", &["--updates"])]);
        return;
    }
    Command::new("xdg-open").arg(target).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
}

/// `file://` URI with everything but unreserved characters and `/` percent-encoded.
pub(crate) fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut s = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// Shows a file selected in the file manager (the FileManager1 D-Bus interface), or opens
/// its folder.
pub fn show_in_folder(path: &Path) {
    let uri = file_uri(path);
    let shown = Command::new("dbus-send")
        .args(["--session", "--print-reply", "--dest=org.freedesktop.FileManager1", "/org/freedesktop/FileManager1", "org.freedesktop.FileManager1.ShowItems", &format!("array:string:{uri}"), "string:"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !shown {
        if let Some(parent) = path.parent() {
            open(&parent.to_string_lossy());
        }
    }
}

/// The user's Desktop folder (from xdg-user-dirs).
pub fn desktop_dir() -> Option<PathBuf> {
    user_dir("DESKTOP").filter(|d| d.is_dir())
}

/// An XDG user directory (DESKTOP, DOCUMENTS, DOWNLOAD...) from ~/.config/user-dirs.dirs.
pub(crate) fn user_dir(name: &str) -> Option<PathBuf> {
    let home = home()?;
    let text = config_dir().and_then(|c| std::fs::read_to_string(c.join("user-dirs.dirs")).ok()).unwrap_or_default();
    user_dir_in(&text, name, &home)
}

fn user_dir_in(text: &str, name: &str, home: &Path) -> Option<PathBuf> {
    let key = format!("XDG_{name}_DIR=");
    let value = text.lines().find_map(|l| l.trim().strip_prefix(&key)).map(|v| v.trim().trim_matches('"').to_string());
    match value {
        Some(v) if v.starts_with("$HOME/") => Some(home.join(&v[6..])),
        Some(v) if v.starts_with('/') => Some(PathBuf::from(v)),
        _ => {
            let fallback = match name {
                "DESKTOP" => "Desktop",
                "DOCUMENTS" => "Documents",
                "DOWNLOAD" => "Downloads",
                "PICTURES" => "Pictures",
                "VIDEOS" => "Videos",
                "MUSIC" => "Music",
                _ => return None,
            };
            Some(home.join(fallback))
        }
    }
}

/// Starts the first of `apps` that's installed.
fn launch(apps: &[(&str, &[&str])]) -> bool {
    for (app, args) in apps {
        if has(app) && Command::new(app).args(*args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok() {
            return true;
        }
    }
    false
}

/// Opens the Linux counterpart of a Windows system tool. Ok carries a message to show.
pub fn open_tool(tool: Tool) -> Result<String, String> {
    match tool {
        Tool::DeviceManager => {
            if launch(&[("kinfocenter", &[]), ("hardinfo2", &[]), ("hardinfo", &[]), ("gnome-control-center", &["system"])]) {
                Ok(String::new())
            } else {
                Err("No hardware information app is installed. Try KInfoCenter (KDE) or HardInfo2.".into())
            }
        }
        Tool::SystemRestore => {
            if launch(&[("btrfs-assistant-launcher", &[]), ("btrfs-assistant", &[]), ("timeshift-launcher", &[]), ("snapper-gui", &[])]) {
                Ok(String::new())
            } else {
                Err("No snapshot app is installed. Install Timeshift, or Btrfs Assistant for Snapper.".into())
            }
        }
        Tool::EventViewer => {
            if launch(&[("ksystemlog", &[]), ("gnome-logs", &[]), ("qjournalctl", &[])]) {
                Ok(String::new())
            } else {
                Err("No log viewer is installed. Install KSystemLog (KDE) or Logs (GNOME), or run journalctl -k in a terminal.".into())
            }
        }
        Tool::MemoryTest => helper("manage", "memtest-next-boot", &[]),
        Tool::StartupManager => {
            if !launch(&[("systemsettings", &["kcm_autostart"]), ("gnome-tweaks", &[]), ("gnome-session-properties", &[]), ("xfce4-session-settings", &[])]) {
                if let Some(dir) = config_dir().map(|c| c.join("autostart")) {
                    std::fs::create_dir_all(&dir).ok();
                    open(&dir.to_string_lossy());
                }
            }
            Ok(String::new())
        }
    }
}

/// Asks for a folder with the desktop's own picker (kdialog on KDE, zenity elsewhere).
/// Titles are fixed strings, passed as argv.
pub fn pick_folder(title: &'static str) -> Option<String> {
    let start = home().map(|h| h.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
    let kdialog = ("kdialog", vec!["--title", title, "--getexistingdirectory", start.as_str()]);
    let zenity = ("zenity", vec!["--file-selection", "--directory", "--title", title]);
    let yad = ("yad", vec!["--file", "--directory", "--title", title]);
    let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.to_uppercase().contains("KDE"));
    let order = if kde { [kdialog, zenity, yad] } else { [zenity, kdialog, yad] };
    for (app, args) in order {
        if !has(app) {
            continue;
        }
        let out = Command::new(app).args(&args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        let path = String::from_utf8_lossy(&out.stdout).trim().trim_end_matches('|').to_string();
        return (out.status.success() && !path.is_empty() && Path::new(&path).is_dir()).then_some(path);
    }
    None
}

/// Where Flash keeps its state: $XDG_DATA_HOME/flash (~/.local/share/flash).
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").unwrap_or_else(std::env::temp_dir).join("flash")
}

/// Opens the benchmark file with O_DIRECT, past the page cache. Filesystems without
/// O_DIRECT (tmpfs) get a plain open, with the cache dropped for the file.
pub fn open_unbuffered(path: &Path, write: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut o = std::fs::OpenOptions::new();
    if write {
        o.write(true).create(true).truncate(true);
    } else {
        o.read(true);
    }
    match o.clone().custom_flags(libc::O_DIRECT | if write { libc::O_DSYNC } else { 0 }).open(path) {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            let f = o.open(path)?;
            use std::os::fd::AsRawFd;
            // SAFETY: a valid open descriptor; the advice only affects caching.
            unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
            Ok(f)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the read-only parts against this PC: `cargo test --lib live_linux -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_linux() {
        let (devices, err, _) = read_devices();
        println!("{} devices, error: {err:?}", devices.len());
        for d in &devices {
            println!("  [{}] {} | {} | {} {} | {} | problem {}", crate::purpose(&d.class), d.name, d.class, d.inf, d.version, d.provider, d.problem);
        }
        let shared = crate::Shared::default();
        for src in crate::Source::ALL.into_iter().take(3) {
            let ups = check_source(src, &shared, &devices, true, None);
            println!("{}: {} -> {ups:#?}", src.name(), shared.lock().unwrap().source_status[src as usize]);
        }
        println!("{}", shared.lock().unwrap().log);
        let r = health_report();
        println!("{r:#?}");
        println!("hardware errors (90 days): {}", whea_read(90).len());
        for j in find_junk() {
            println!("  junk {} [{}] {} bytes on={}", j.name, j.group, j.size, j.default_on);
        }
        for a in startup_apps() {
            println!("  startup {} enabled={} delay={:?} at {}", a.name, a.enabled, a.delay_ms, a.location);
        }
        println!("schedule on: {}  snapshot: {:?}  install kind: {:?}", schedule_enabled(), snapshot_status(), install_kind(&std::env::current_exe().unwrap()));
        let (temps, fans) = sensors(&mut sysinfo::Components::new_with_refreshed_list());
        println!("temps {temps:?}\nfans {fans:?}");
    }

    #[test]
    fn os_release_dirs_and_uris() {
        let os = parse_os_release("NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\n");
        assert_eq!(os["ID"], "ubuntu");
        assert_eq!(os["PRETTY_NAME"], "Ubuntu 24.04.1 LTS");
        assert_eq!(file_uri(Path::new("/home/a b/x,y.iso")), "file:///home/a%20b/x%2Cy.iso");
        let dirs = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Schreibtisch\"\nXDG_MUSIC_DIR=\"/data/music\"\n";
        let home = Path::new("/home/u");
        assert_eq!(user_dir_in(dirs, "DESKTOP", home), Some(PathBuf::from("/home/u/Schreibtisch")));
        assert_eq!(user_dir_in(dirs, "MUSIC", home), Some(PathBuf::from("/data/music")));
        assert_eq!(user_dir_in(dirs, "VIDEOS", home), Some(PathBuf::from("/home/u/Videos")));
    }
}
