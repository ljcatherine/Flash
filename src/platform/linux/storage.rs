//! Storage on Linux: what's on the root filesystem, and the cleaner: app caches in
//! ~/.cache, your old files in /tmp, the Trash, crash dumps, the journal, package caches,
//! unused Flatpak runtimes, old Snap revisions and old kernels (never the running or newest).
//! Same rules as Windows: measure first, confirm, never follow links.
use super::{cache_dir, cmd, cmd_any, has, helper, home, read, user_dir};
use crate::storage::{Junk, Kind, delete_contents, dir_size, safe_target};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const USER_DIRS: [&str; 6] = ["DESKTOP", "DOCUMENTS", "DOWNLOAD", "PICTURES", "VIDEOS", "MUSIC"];

fn dev_of(p: &Path) -> Option<u64> {
    std::fs::symlink_metadata(p).ok().map(|m| m.dev())
}

/// What to measure on the root filesystem: ("/", parts, your folders). Folders on another
/// filesystem (a separate /home, say) are left out so the shares add up.
pub fn breakdown_plan() -> (String, Vec<(&'static str, Vec<PathBuf>)>, Vec<PathBuf>) {
    let root = dev_of(Path::new("/"));
    let on_root = |p: &PathBuf| p.exists() && dev_of(p) == root;
    let home = home();
    let user_dirs: Vec<PathBuf> = USER_DIRS.iter().filter_map(|d| user_dir(d)).filter(|p| p.is_dir()).collect();
    let h = |s: &str| home.iter().map(|h| h.join(s)).collect::<Vec<_>>();
    let mut app_data = h(".cache");
    app_data.extend(h(".config"));
    app_data.extend(h(".local/share"));
    app_data.extend(h(".var/app"));
    let parts: Vec<(&'static str, Vec<PathBuf>)> = vec![
        ("System & programs", ["/usr", "/opt"].map(PathBuf::from).to_vec()),
        ("Flatpak & Snap", ["/var/lib/flatpak", "/var/lib/snapd/snaps"].map(PathBuf::from).to_vec()),
        ("Logs & system caches", ["/var/log", "/var/cache", "/var/tmp"].map(PathBuf::from).to_vec()),
        ("Your files", user_dirs.clone()),
        ("App data & caches", app_data),
    ]
    .into_iter()
    .map(|(n, paths)| (n, paths.into_iter().filter(|p| on_root(p)).collect()))
    .collect();
    (String::from("/"), parts, user_dirs)
}

// ---------------- Finding junk ----------------

/// Known caches under ~/.cache with a friendly name, a description and whether to tick them.
const KNOWN: [(&str, &str, &str, bool); 8] = [
    ("thumbnails", "Thumbnail cache", "Picture previews the file manager rebuilds when needed.", true),
    ("mozilla", "Firefox cache", "Cached web pages. You stay signed in; pages load a little slower at first.", true),
    ("google-chrome", "Chrome cache", "Cached web pages. You stay signed in; pages load a little slower at first.", true),
    ("chromium", "Chromium cache", "Cached web pages. You stay signed in; pages load a little slower at first.", true),
    ("BraveSoftware", "Brave cache", "Cached web pages. You stay signed in; pages load a little slower at first.", true),
    ("microsoft-edge", "Edge cache", "Cached web pages. You stay signed in; pages load a little slower at first.", true),
    ("pip", "Python package cache", "Downloads pip keeps to install packages faster.", true),
    ("yarn", "Yarn package cache", "Downloads Yarn keeps to install packages faster.", true),
];
const SHADERS: [&str; 5] = ["mesa_shader_cache", "mesa_shader_cache_db", "nvidia", "radv_builtin_shaders", "fontconfig"];

fn is_mine_and_old(m: &std::fs::Metadata, uid: u32, before: i64) -> bool {
    m.uid() == uid && m.mtime() < before && (m.is_file() || m.is_dir())
}

/// Your own files and folders directly in /tmp and /var/tmp, untouched for a day.
fn user_temp(uid: u32) -> (Vec<PathBuf>, u64) {
    let before = super::now() as i64 - 86400;
    let mut found = Vec::new();
    let mut size = 0;
    for dir in ["/tmp", "/var/tmp"] {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
            if is_mine_and_old(&m, uid, before) && !e.file_name().to_string_lossy().starts_with(".X") {
                size += if m.is_dir() { dir_size(&p) } else { m.len() };
                found.push(p);
            }
        }
    }
    (found, size)
}

/// `journalctl --disk-usage`: "Archived and active journals take up 1.2G in the file system."
pub(crate) fn parse_disk_usage(s: &str) -> u64 {
    let Some(size) = s.split("take up ").nth(1).and_then(|r| r.split_whitespace().next()) else { return 0 };
    let (num, unit) = size.split_at(size.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(size.len()));
    let n: f64 = num.parse().unwrap_or(0.0);
    let mult = match unit.chars().next() {
        Some('K') => 1024.0,
        Some('M') => 1024.0 * 1024.0,
        Some('G') => 1024.0 * 1024.0 * 1024.0,
        Some('T') => 1024.0f64.powi(4),
        _ => 1.0,
    };
    (n * mult) as u64
}

fn journal_size() -> u64 {
    cmd("journalctl", &["--disk-usage"]).map(|s| parse_disk_usage(&s)).unwrap_or(0)
}

/// Folders a system cleanup frees space in, to measure before and after.
fn helper_dirs(what: &str) -> Vec<PathBuf> {
    let p = |s: &str| PathBuf::from(s);
    match what {
        "pkg-cache" => vec![p("/var/cache/apt/archives"), p("/var/cache/dnf"), p("/var/cache/libdnf5"), p("/var/cache/pacman/pkg"), p("/var/cache/zypp/packages")],
        "coredumps" => vec![p("/var/lib/systemd/coredump"), p("/var/crash")],
        "flatpak-unused" => vec![p("/var/lib/flatpak")],
        "snap-disabled" => vec![p("/var/lib/snapd/snaps")],
        _ => vec![],
    }
}

fn measure(paths: &[PathBuf]) -> u64 {
    paths.iter().filter(|p| p.exists()).map(|p| dir_size(p)).sum()
}

/// Revisions of snaps that a newer one replaced ("disabled" in `snap list --all`).
pub(crate) fn disabled_snaps(list: &str) -> Vec<(String, String)> {
    list.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 6 && f.last().is_some_and(|n| n.split(',').any(|x| x == "disabled"))).then(|| (f[0].to_string(), f[2].to_string()))
        })
        .collect()
}

/// Installed kernels other than the running and the newest one: (release, bytes).
fn old_kernels() -> Vec<(String, u64)> {
    let running = read("/proc/sys/kernel/osrelease");
    let mut kernels: Vec<String> = std::fs::read_dir("/lib/modules").into_iter().flatten().flatten().filter(|e| e.path().join("modules.dep").exists() || e.path().join("kernel").is_dir()).filter_map(|e| e.file_name().into_string().ok()).collect();
    kernels.sort_by(|a, b| super::packages::cmp_versions(a, b));
    let newest = kernels.last().cloned().unwrap_or_default();
    kernels
        .into_iter()
        .filter(|k| *k != running && *k != newest && super::packages::is_version(k))
        .map(|k| {
            let mut size = dir_size(&Path::new("/lib/modules").join(&k));
            for f in [format!("vmlinuz-{k}"), format!("initrd.img-{k}"), format!("initramfs-{k}.img"), format!("System.map-{k}"), format!("config-{k}")] {
                size += std::fs::metadata(Path::new("/boot").join(f)).map_or(0, |m| m.len());
            }
            (k, size)
        })
        .collect()
}

fn junk(id: &str, name: &str, about: &str, size: u64, on: bool, kind: Kind) -> Junk {
    Junk { id: id.into(), group: "Temporary files & caches", name: name.into(), about: about.into(), size, default_on: on, kind }
}

/// Finds and measures everything the cleaner can remove.
pub fn find_junk() -> Vec<Junk> {
    let uid = unsafe { libc::getuid() };
    let mut out = Vec::new();
    let (temp, temp_size) = user_temp(uid);
    out.push(junk("temp-user", "Your temporary files", "Files and folders of yours in /tmp and /var/tmp that haven't changed for a day. Anything in use is skipped.", temp_size, true, Kind::UserTemp(temp)));

    // ~/.cache, app by app.
    if let Some(cache) = cache_dir().filter(|c| safe_target(&c.join("x"))) {
        let mut shaders = Vec::new();
        let mut others: Vec<Junk> = Vec::new();
        for e in std::fs::read_dir(&cache).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let p = e.path();
            if !safe_target(&p) || std::fs::symlink_metadata(&p).map_or(true, |m| m.file_type().is_symlink()) || name == "flash" {
                continue;
            }
            if SHADERS.contains(&name.as_str()) {
                shaders.push(p);
                continue;
            }
            let size = dir_size(&p);
            match KNOWN.iter().find(|k| k.0 == name) {
                Some((_, title, about, on)) => out.push(junk(&format!("cache-{name}"), title, about, size, *on, Kind::Paths(vec![p]))),
                None if size >= 10_000_000 => others.push(junk(&format!("cache-{name}"), &format!("{name} cache"), &format!("Files {name} keeps in ~/.cache and recreates when needed."), size, false, Kind::Paths(vec![p]))),
                None => {}
            }
        }
        if let Some(steam) = home().map(|h| h.join(".local/share/Steam/steamapps/shadercache")).filter(|p| p.is_dir()) {
            shaders.push(steam);
        }
        let size = measure(&shaders);
        out.push(junk("shaders", "Graphics shader caches", "Games and apps rebuild these, so the first launch after cleaning can stutter briefly.", size, false, Kind::Paths(shaders)));
        others.sort_by(|a, b| b.size.cmp(&a.size));
        out.extend(others.into_iter().take(15));
    }

    out.push(junk("dumps", "Crash dumps and error reports", "Saved after programs crashed, for troubleshooting. Safe to remove once things are stable.", measure(&helper_dirs("coredumps")), true, Kind::Helper("coredumps")));
    if has("journalctl") {
        out.push(junk("journal", "System logs older than two weeks", "The journal's older entries. Flash's hardware error history uses these, so keep them while you troubleshoot.", journal_size(), false, Kind::Helper("journal")));
    }
    if super::packages::detect().is_ok() {
        out.push(junk("pkg-cache", "Package cache", "Downloaded package files your package manager already installed.", measure(&helper_dirs("pkg-cache")), true, Kind::Helper("pkg-cache")));
    }
    if let Some(trash) = home().map(|h| h.join(".local/share/Trash")) {
        let size = measure(&[trash.join("files"), trash.join("info")]);
        out.push(junk("recycle", "Trash", "Permanently deletes what's in the Trash.", size, false, Kind::RecycleBin));
    }
    if has("flatpak") {
        out.push(junk("flatpak", "Unused Flatpak runtimes", "Runtimes no installed Flatpak app uses any more.", 0, false, Kind::FlatpakUser));
    }
    if has("snap") {
        let list = cmd("snap", &["list", "--all"]).unwrap_or_default();
        let size: u64 = disabled_snaps(&list).iter().map(|(n, r)| std::fs::metadata(format!("/var/lib/snapd/snaps/{n}_{r}.snap")).map_or(0, |m| m.len())).sum();
        out.push(junk("snap", "Old Snap versions", "Earlier revisions Snap keeps after updating an app.", size, size > 0, Kind::Helper("snap-disabled")));
    }
    let newest = {
        let mut k: Vec<String> = std::fs::read_dir("/lib/modules").into_iter().flatten().flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
        k.sort_by(|a, b| super::packages::cmp_versions(a, b));
        k.pop().unwrap_or_default()
    };
    if matches!(super::packages::detect(), Ok(super::packages::Pm::Apt | super::packages::Pm::Dnf)) {
        for (k, size) in old_kernels() {
            out.push(Junk { id: format!("kernel-{k}"), group: "Old driver versions", name: format!("Linux kernel {k}"), about: format!("Replaced by {newest}. Removing it means you can't start the PC with this kernel any more."), size, default_on: false, kind: Kind::OldKernel(k) });
        }
    }
    out
}

// ---------------- Cleaning ----------------

/// Removes a file or folder of yours, never following links, and only what you own.
fn remove_owned(p: &Path, uid: u32) -> u64 {
    let Ok(m) = std::fs::symlink_metadata(p) else { return 0 };
    if m.uid() != uid {
        return 0;
    }
    if m.file_type().is_symlink() || m.is_file() {
        return if std::fs::remove_file(p).is_ok() { m.len() } else { 0 };
    }
    if !m.is_dir() {
        return 0;
    }
    let mut freed = 0;
    for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
        freed += remove_owned(&e.path(), uid);
    }
    std::fs::remove_dir(p).ok();
    freed
}

/// Cleans one item. Returns bytes freed.
pub fn clean(j: &Junk) -> Result<u64, String> {
    let uid = unsafe { libc::getuid() };
    match &j.kind {
        Kind::Paths(paths) => Ok(paths.iter().map(|p| delete_contents(p)).sum()),
        Kind::UserTemp(paths) => {
            let before = super::now() as i64 - 86400;
            // Only direct entries of /tmp and /var/tmp, checked again now.
            Ok(paths
                .iter()
                .filter(|p| p.parent().is_some_and(|d| d == Path::new("/tmp") || d == Path::new("/var/tmp")))
                .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| is_mine_and_old(&m, uid, before)))
                .map(|p| remove_owned(p, uid))
                .sum())
        }
        Kind::RecycleBin => {
            let trash = home().ok_or("no home folder")?.join(".local/share/Trash");
            Ok(["files", "info", "expunged"].iter().map(|d| delete_contents(&trash.join(d))).sum())
        }
        Kind::Helper("journal") => {
            let before = journal_size();
            helper("manage", "clean", &["journal"])?;
            Ok(before.saturating_sub(journal_size()))
        }
        Kind::Helper(what) => {
            let dirs = helper_dirs(what);
            let before = measure(&dirs);
            helper("manage", "clean", &[what])?;
            Ok(before.saturating_sub(measure(&dirs)))
        }
        Kind::FlatpakUser => {
            let dirs: Vec<PathBuf> = home().map(|h| h.join(".local/share/flatpak")).into_iter().chain([PathBuf::from("/var/lib/flatpak")]).collect();
            let before = measure(&dirs);
            cmd_any("flatpak", &["uninstall", "--user", "--unused", "-y", "--noninteractive"]);
            // The system installation needs an administrator.
            let system = helper("manage", "clean", &["flatpak-unused"]);
            let freed = before.saturating_sub(measure(&dirs));
            match system {
                Err(e) if freed == 0 => Err(e),
                _ => Ok(freed),
            }
        }
        Kind::OldKernel(version) if super::packages::is_version(version) => helper("manage", "remove-kernel", &[version]).map(|_| j.size),
        _ => Err("refused: not something Flash cleans on Linux".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers() {
        assert_eq!(parse_disk_usage("Archived and active journals take up 1.5G in the file system."), 1_610_612_736);
        assert_eq!(parse_disk_usage("Archived and active journals take up 512.0M in the file system."), 536_870_912);
        assert_eq!(parse_disk_usage("nonsense"), 0);
        let snaps = "Name      Version   Rev    Tracking       Publisher   Notes\ncore22    20240111  1122   latest/stable  canonical✓  base,disabled\ncore22    20240408  1380   latest/stable  canonical✓  base\n";
        assert_eq!(disabled_snaps(snaps), [("core22".to_string(), "1122".to_string())]);
    }

    #[test]
    fn removes_only_own_files_without_following_links() {
        let uid = unsafe { libc::getuid() };
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join(format!("flash-owned-{}", std::process::id()));
        let outside = root.with_extension("outside");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), b"keep").unwrap();
        std::fs::write(root.join("sub/a.tmp"), vec![0u8; 100]).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        // The file inside and the link itself go; what the link points to stays.
        assert!(remove_owned(&root, uid) >= 100);
        assert!(!root.exists());
        assert!(outside.join("keep.txt").exists());
        std::fs::remove_dir_all(&outside).unwrap();
    }
}
