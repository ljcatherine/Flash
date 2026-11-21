//! The weekly background scan (a systemd user timer) and startup apps (XDG autostart
//! entries and your own enabled systemd user units), all in your own configuration: no root.
use super::{cmd, cmd_any, config_dir, has};
use crate::extras::StartupApp;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const UNIT: &str = "flash-weekly-scan";

fn unit_dir() -> Option<PathBuf> {
    config_dir().map(|c| c.join("systemd/user"))
}

pub fn schedule_enabled() -> bool {
    cmd_any("systemctl", &["--user", "is-enabled", &format!("{UNIT}.timer")]).is_some_and(|(o, _)| o.trim() == "enabled")
}

/// Quotes a path for a systemd ExecStart= line.
pub(crate) fn systemd_quote(path: &str) -> Option<String> {
    if path.is_empty() || !path.starts_with('/') || path.contains(['\n', '\r', '\0']) {
        return None;
    }
    let escaped = path.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$");
    Some(format!("\"{escaped}\""))
}

/// The path to start Flash by, one that survives updates: the AppImage itself, or the
/// profile link on Nix (store paths change with every update).
fn stable_exe() -> Result<PathBuf, String> {
    if let Some(app) = std::env::var_os("APPIMAGE").map(PathBuf::from).filter(|p| p.is_file()) {
        return Ok(app);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if exe.starts_with("/nix/store") {
        if let Some(path) = std::env::var_os("PATH") {
            if let Some(link) = std::env::split_paths(&path).filter(|d| !d.starts_with("/nix/store")).map(|d| d.join("flash")).find(|p| p.exists()) {
                return Ok(link);
            }
        }
    }
    Ok(exe)
}

/// Turns the weekly background scan (Sundays at noon, or at the next start if the PC was off)
/// on or off with a systemd user timer.
pub fn set_schedule(on: bool) -> Result<(), String> {
    if !has("systemctl") {
        return Err("systemd isn't running, so there's no timer to schedule the scan with.".into());
    }
    let dir = unit_dir().ok_or("no home folder")?;
    let (service, timer) = (dir.join(format!("{UNIT}.service")), dir.join(format!("{UNIT}.timer")));
    if on {
        let exe = stable_exe()?;
        let quoted = systemd_quote(&exe.to_string_lossy()).ok_or("Flash's path can't be used in a systemd unit")?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        std::fs::write(&service, format!("# Written by Flash. Turn the weekly scan off in Flash to remove it.\n[Unit]\nDescription=Flash weekly driver and health scan\n\n[Service]\nType=exec\nExecStart={quoted} --scheduled\n")).map_err(|e| e.to_string())?;
        std::fs::write(&timer, "# Written by Flash.\n[Unit]\nDescription=Run Flash's weekly scan\n\n[Timer]\nOnCalendar=Sun *-*-* 12:00:00\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n").map_err(|e| e.to_string())?;
        cmd("systemctl", &["--user", "daemon-reload"])?;
        cmd("systemctl", &["--user", "enable", "--now", &format!("{UNIT}.timer")]).map(|_| ())
    } else {
        cmd_any("systemctl", &["--user", "disable", "--now", &format!("{UNIT}.timer")]);
        std::fs::remove_file(&timer).ok();
        std::fs::remove_file(&service).ok();
        cmd("systemctl", &["--user", "daemon-reload"]).map(|_| ())
    }
}

// ---------------- Startup apps ----------------

/// The [Desktop Entry] keys of a .desktop file.
pub(crate) fn desktop_entry(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut inside = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            inside = l == "[Desktop Entry]";
        } else if inside {
            if let Some((k, v)) = l.split_once('=') {
                out.entry(k.trim().to_string()).or_insert_with(|| v.trim().to_string());
            }
        }
    }
    out
}

/// Whether an autostart entry runs in this desktop session.
pub(crate) fn runs_here(e: &HashMap<String, String>, desktops: &[String]) -> bool {
    let list = |k: &str| e.get(k).map(|v| v.split(';').filter(|s| !s.is_empty()).map(str::to_ascii_uppercase).collect::<Vec<_>>());
    if let Some(only) = list("OnlyShowIn") {
        if !only.iter().any(|d| desktops.contains(d)) {
            return false;
        }
    }
    if let Some(not) = list("NotShowIn") {
        if not.iter().any(|d| desktops.contains(d)) {
            return false;
        }
    }
    e.get("Type").is_none_or(|t| t == "Application")
}

pub(crate) fn entry_enabled(e: &HashMap<String, String>) -> bool {
    e.get("Hidden").is_none_or(|v| !v.eq_ignore_ascii_case("true")) && e.get("X-GNOME-Autostart-enabled").is_none_or(|v| !v.eq_ignore_ascii_case("false"))
}

/// `systemd-analyze --user blame`: unit -> milliseconds ("1.415s", "689ms", "1min 2.3s").
pub(crate) fn parse_blame(text: &str) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let mut f: Vec<&str> = line.split_whitespace().collect();
        let Some(unit) = f.pop() else { continue };
        let mut ms = 0.0;
        for t in f {
            let (num, unit_s) = t.split_at(t.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(t.len()));
            let n: f64 = num.parse().unwrap_or(0.0);
            ms += match unit_s {
                "ms" => n,
                "s" => n * 1000.0,
                "min" => n * 60_000.0,
                "h" => n * 3_600_000.0,
                "us" | "µs" => n / 1000.0,
                _ => 0.0,
            };
        }
        out.insert(unit.to_string(), ms as u64);
    }
    out
}

/// The desktop file an autostart unit came from: "app-org.kde.foo@autostart.service" -> "org.kde.foo".
pub(crate) fn autostart_unit_id(unit: &str) -> Option<String> {
    let id = unit.strip_prefix("app-")?.split_once("@autostart.service")?.0;
    Some(id.replace("\\x2d", "-"))
}

fn autostart_dirs() -> Vec<PathBuf> {
    let dirs = std::env::var("XDG_CONFIG_DIRS").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/etc/xdg".into());
    dirs.split(':').filter(|d| d.starts_with('/')).map(|d| Path::new(d).join("autostart")).collect()
}

fn desktops() -> Vec<String> {
    std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').map(str::to_ascii_uppercase).collect()
}

/// Programs that start when you sign in.
pub fn startup_apps() -> Vec<StartupApp> {
    let user = config_dir().map(|c| c.join("autostart"));
    let blame = cmd("systemd-analyze", &["--user", "blame"]).map(|t| parse_blame(&t)).unwrap_or_default();
    let delay = |id: &str| blame.iter().filter(|(u, _)| autostart_unit_id(u).as_deref() == Some(id) || u.as_str() == id).map(|(_, ms)| *ms).max();
    let desktops = desktops();
    // Your own entries override the system's with the same file name.
    let mut files: HashMap<String, PathBuf> = HashMap::new();
    for dir in autostart_dirs().into_iter().rev().chain(user.clone()) {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".desktop") {
                files.insert(name, e.path());
            }
        }
    }
    let mut out = Vec::new();
    for (file, path) in files {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let e = desktop_entry(&text);
        if !runs_here(&e, &desktops) {
            continue;
        }
        let id = file.trim_end_matches(".desktop").to_string();
        out.push(StartupApp {
            name: e.get("Name").cloned().unwrap_or_else(|| id.clone()),
            command: e.get("Exec").cloned().unwrap_or_default(),
            location: path.to_string_lossy().into_owned(),
            enabled: entry_enabled(&e),
            delay_ms: delay(&id),
        });
    }
    // systemd user services you set up yourself.
    if let Some(dir) = unit_dir() {
        let mut units: HashMap<String, bool> = HashMap::new();
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".service") && !name.starts_with(UNIT) {
                units.entry(name).or_insert(false);
            } else if name.ends_with(".wants") {
                for l in std::fs::read_dir(e.path()).into_iter().flatten().flatten() {
                    let u = l.file_name().to_string_lossy().to_string();
                    if u.ends_with(".service") && !u.starts_with(UNIT) {
                        units.insert(u, true);
                    }
                }
            }
        }
        for (unit, enabled) in units {
            out.push(StartupApp { name: unit.trim_end_matches(".service").to_string(), command: format!("systemd user service {unit}"), location: format!("systemd:{unit}"), enabled, delay_ms: blame.get(&unit).copied() });
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// Sets Hidden= in a desktop entry's text, adding it to [Desktop Entry] if it's missing.
pub(crate) fn with_hidden(text: &str, hidden: bool) -> String {
    let value = if hidden { "true" } else { "false" };
    let mut out = String::new();
    let mut inside = false;
    let mut done = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            if inside && !done {
                out += &format!("Hidden={value}\n");
                done = true;
            }
            inside = l == "[Desktop Entry]";
        } else if inside && l.split_once('=').is_some_and(|(k, _)| k.trim() == "Hidden" || k.trim() == "X-GNOME-Autostart-enabled") {
            if !done && l.starts_with("Hidden") {
                out += &format!("Hidden={value}\n");
                done = true;
            }
            continue;
        }
        out += line;
        out.push('\n');
    }
    if inside && !done {
        out += &format!("Hidden={value}\n");
    }
    out
}

fn valid_unit(u: &str) -> bool {
    u.ends_with(".service") && u.len() < 200 && !u.starts_with('-') && u.chars().all(|c| c.is_ascii_alphanumeric() || "@._-:\\".contains(c))
}

/// Turns a startup app on or off: Hidden= in your own copy of its autostart entry, or
/// `systemctl --user enable/disable` for a service.
pub fn set_startup(app: &StartupApp, enabled: bool) -> Result<(), String> {
    if let Some(unit) = app.location.strip_prefix("systemd:") {
        if !valid_unit(unit) {
            return Err("refused: unexpected unit name".into());
        }
        return cmd("systemctl", &["--user", if enabled { "enable" } else { "disable" }, unit]).map(|_| ());
    }
    let src = Path::new(&app.location);
    let file = src.file_name().and_then(|f| f.to_str()).filter(|f| f.ends_with(".desktop") && !f.contains('/')).ok_or("refused: unexpected file")?;
    let user = config_dir().ok_or("no home folder")?.join("autostart");
    let text = std::fs::read_to_string(src).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&user).map_err(|e| e.to_string())?;
    let target = user.join(file);
    // Never write through a link.
    if std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
        std::fs::remove_file(&target).map_err(|e| e.to_string())?;
    }
    std::fs::write(&target, with_hidden(&text, !enabled)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_exec_paths() {
        assert_eq!(systemd_quote("/home/u/Apps/Flash.AppImage").unwrap(), "\"/home/u/Apps/Flash.AppImage\"");
        assert_eq!(systemd_quote("/home/u/My \"Apps\"/50% $off/flash").unwrap(), "\"/home/u/My \\\"Apps\\\"/50%% $$off/flash\"");
        assert!(systemd_quote("relative/flash").is_none() && systemd_quote("/a\nExecStart=/bin/sh").is_none());
    }

    #[test]
    fn autostart_entries() {
        // ~/.config/autostart/claude.desktop on this PC, minus the path.
        let text = "[Desktop Entry]\nType=Application\nName=Claude\nExec=/opt/Claude/claude --startup\nX-GNOME-Autostart-enabled=true\n\n[Desktop Action new]\nName=New window\n";
        let e = desktop_entry(text);
        assert_eq!(e["Name"], "Claude");
        assert!(entry_enabled(&e) && runs_here(&e, &["KDE".into()]));
        let off = with_hidden(text, true);
        assert!(!entry_enabled(&desktop_entry(&off)));
        assert!(off.contains("[Desktop Action new]\nName=New window"));
        assert!(entry_enabled(&desktop_entry(&with_hidden(&off, false))));
        assert!(!runs_here(&desktop_entry("[Desktop Entry]\nOnlyShowIn=GNOME;\n"), &["KDE".into()]));
        assert!(!runs_here(&desktop_entry("[Desktop Entry]\nNotShowIn=KDE;\n"), &["KDE".into()]));
    }

    #[test]
    fn boot_blame() {
        // Captured from `systemd-analyze --user blame` on this PC.
        let b = parse_blame("5.016s nexus-nixos-workspace-tunnel.service\n3.321s xdg-desktop-portal.service\n 689ms plasma-powerdevil.service\n1min 2.500s slow.service\n 120ms app-org.kde.discover.notifier@autostart.service\n");
        assert_eq!(b["xdg-desktop-portal.service"], 3321);
        assert_eq!(b["plasma-powerdevil.service"], 689);
        assert_eq!(b["slow.service"], 62_500);
        assert_eq!(autostart_unit_id("app-org.kde.discover.notifier@autostart.service").as_deref(), Some("org.kde.discover.notifier"));
        assert_eq!(autostart_unit_id("app-solaar\\x2dgui@autostart.service").as_deref(), Some("solaar-gui"));
        assert!(valid_unit("syncthing.service") && !valid_unit("x;rm.service") && !valid_unit("-x.service"));
    }
}
