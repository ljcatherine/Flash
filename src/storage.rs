//! Storage: drive usage, what's taking space on the system drive, and a cleaner for
//! temporary files, caches, old driver versions (old kernels on Linux) and preinstalled apps.
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Drive {
    pub name: String,
    pub mount: String,
    pub fs: String,
    pub kind: String, // SSD / HDD / Unknown
    pub total: u64,
    pub free: u64,
    pub removable: bool,
}

pub fn drives() -> Vec<Drive> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut out: Vec<Drive> = disks
        .list()
        .iter()
        .map(|d| Drive {
            name: d.name().to_string_lossy().into_owned(),
            mount: d.mount_point().to_string_lossy().into_owned(),
            fs: d.file_system().to_string_lossy().into_owned(),
            kind: format!("{:?}", d.kind()).replace("Unknown(-1)", "Unknown"),
            total: d.total_space(),
            free: d.available_space(),
            removable: d.is_removable(),
        })
        .filter(|d| d.total > 0)
        .collect();
    out.sort_by(|a, b| a.mount.cmp(&b.mount));
    out
}

pub fn human(bytes: u64) -> String {
    let b = bytes as f64;
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{:.0} KB", b / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1} MB", b / 1e6),
        1_000_000_000..1_000_000_000_000 => format!("{:.1} GB", b / 1e9),
        _ => format!("{:.2} TB", b / 1e12),
    }
}

/// Calls `f` for every file under `path`. Links and junctions are never followed, and on
/// Linux the walk stays on the filesystem it started on (like `du -x`).
fn walk(path: &Path, f: &mut impl FnMut(&Path, u64)) {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return };
    walk_in(path, &meta, device_of(&meta), f);
}

#[cfg(unix)]
fn device_of(meta: &std::fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::dev(meta)
}

#[cfg(not(unix))]
fn device_of(_: &std::fs::Metadata) -> u64 {
    0
}

fn walk_in(path: &Path, meta: &std::fs::Metadata, dev: u64, f: &mut impl FnMut(&Path, u64)) {
    if meta.file_type().is_symlink() || device_of(meta) != dev {
        return;
    }
    if meta.is_file() {
        f(path, meta.len());
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if let Ok(m) = std::fs::symlink_metadata(&p) {
            walk_in(&p, &m, dev, f);
        }
    }
}

pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    walk(path, &mut |_, len| total += len);
    total
}

#[derive(Clone, Debug, Default)]
pub struct Breakdown {
    pub drive: String,
    pub total: u64,
    pub used: u64,
    pub parts: Vec<(&'static str, u64)>,
    pub largest: Vec<(PathBuf, u64)>,
}

/// Measures the main folders on the system drive and finds the biggest files in your folders.
pub fn breakdown() -> Breakdown {
    let (sys, parts, user_dirs) = crate::platform::breakdown_plan();
    let (total, used) = system_drive(&drives(), &sys).map_or((0, 0), |d| (d.total, d.total - d.free));
    let mut largest: Vec<(PathBuf, u64)> = Vec::new();
    let sizes: Vec<(&'static str, u64)> = std::thread::scope(|s| {
        let hs: Vec<_> = parts.iter().map(|(name, paths)| s.spawn(move || (*name, paths.iter().map(|p| dir_size(p)).sum::<u64>()))).collect();
        // Biggest files in your own folders, gathered while the totals run.
        for dir in &user_dirs {
            walk(dir, &mut |p, len| {
                if largest.len() < 20 || len > largest.last().map_or(0, |l| l.1) {
                    largest.push((p.to_path_buf(), len));
                    largest.sort_by(|a, b| b.1.cmp(&a.1));
                    largest.truncate(20);
                }
            });
        }
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let known: u64 = sizes.iter().map(|s| s.1).sum();
    let mut parts = sizes;
    // Page file, hibernation file, restore points, other users, root-only folders...
    parts.push(("System & other", used.saturating_sub(known)));
    Breakdown { drive: sys, total, used, parts, largest }
}

// ---------- Cleaner ----------

/// The drive the breakdown measures: the one mounted at `mount` (`C:` or `/`).
pub fn system_drive<'a>(drives: &'a [Drive], mount: &str) -> Option<&'a Drive> {
    if cfg!(target_os = "linux") {
        drives.iter().find(|d| d.mount == mount)
    } else {
        drives.iter().find(|d| d.mount.to_lowercase().starts_with(&mount.to_lowercase()))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Paths(Vec<PathBuf>),
    RecycleBin,
    UpdateCache(PathBuf),
    DeliveryOptimization(PathBuf),
    OldDriver(String), // oemNN.inf
    App(String),       // package full name
    /// Linux: your own files in /tmp and /var/tmp older than a day.
    UserTemp(Vec<PathBuf>),
    /// Linux: a system cleanup the polkit helper runs ("pkg-cache", "journal", ...).
    Helper(&'static str),
    /// Linux: unused Flatpak runtimes in your own installation.
    FlatpakUser,
    /// Linux: an old kernel (its release string), never the running or newest one.
    OldKernel(String),
}

impl Kind {
    /// Items whose size can't be measured beforehand, but can still be picked.
    pub fn sizeless(&self) -> bool {
        matches!(self, Kind::App(_) | Kind::FlatpakUser | Kind::Helper("flatpak-unused" | "snap-disabled"))
    }
}

#[derive(Clone, Debug)]
pub struct Junk {
    pub id: String,
    pub group: &'static str,
    pub name: String,
    pub about: String,
    pub size: u64,
    pub default_on: bool,
    pub kind: Kind,
}

/// Refuses anything shallower than e.g. `C:\Windows\Temp` or `/home/you/.cache`, so a bad
/// environment variable can never point the cleaner at a drive root, the Windows folder,
/// `/`, `/usr` or your home folder itself.
pub fn safe_target(p: &Path) -> bool {
    p.is_absolute() && p.components().count() >= 4 && !p.components().any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Deletes everything inside `path` (or the file itself). Files in use are skipped.
/// Returns the bytes actually removed.
pub fn delete_contents(path: &Path) -> u64 {
    if !safe_target(path) {
        return 0;
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if meta.is_file() {
        return if std::fs::remove_file(path).is_ok() { meta.len() } else { 0 };
    }
    let Ok(entries) = std::fs::read_dir(path) else { return 0 };
    let mut freed = 0;
    for e in entries.flatten() {
        let p = e.path();
        let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
        if m.file_type().is_symlink() {
            // Remove the link itself, never what it points to.
            std::fs::remove_file(&p).or_else(|_| std::fs::remove_dir(&p)).ok();
        } else if m.is_dir() {
            let size = dir_size(&p);
            if std::fs::remove_dir_all(&p).is_ok() {
                freed += size;
            } else {
                freed += delete_contents(&p); // partly in use: take what we can
                std::fs::remove_dir(&p).ok();
            }
        } else if std::fs::remove_file(&p).is_ok() {
            freed += m.len();
        }
    }
    freed
}

pub fn app_name(package: &str) -> String {
    match package {
        "Microsoft.BingNews" => "News",
        "Microsoft.BingWeather" => "Weather",
        "Microsoft.BingSearch" => "Bing Search",
        "Microsoft.GetHelp" => "Get Help",
        "Microsoft.Getstarted" => "Tips",
        "Microsoft.MicrosoftSolitaireCollection" => "Solitaire Collection",
        "Microsoft.WindowsFeedbackHub" => "Feedback Hub",
        "Clipchamp.Clipchamp" => "Clipchamp",
        "Microsoft.MicrosoftOfficeHub" => "Microsoft 365 app hub",
        "Microsoft.People" => "People",
        "Microsoft.SkypeApp" => "Skype",
        "Microsoft.MixedReality.Portal" => "Mixed Reality Portal",
        "Microsoft.Microsoft3DViewer" => "3D Viewer",
        "Microsoft.549981C3F5F10" => "Cortana",
        "Microsoft.WindowsMaps" => "Maps",
        "Microsoft.ZuneVideo" => "Films & TV",
        "Microsoft.PowerAutomateDesktop" => "Power Automate",
        other => return other.split('.').next_back().unwrap_or(other).to_string(),
    }
    .to_string()
}

/// Third-party driver packages that a newer version of the same driver has replaced and no device uses.
pub fn old_drivers(rows: &[Vec<String>]) -> Vec<(String, PathBuf, String)> {
    use std::collections::HashMap;
    let mut groups: HashMap<(String, String), Vec<&Vec<String>>> = HashMap::new();
    for r in rows {
        let file = Path::new(&r[2]).file_name().map(|f| f.to_string_lossy().to_lowercase()).unwrap_or_default();
        groups.entry((file, r[3].to_lowercase())).or_default().push(r);
    }
    let mut out = Vec::new();
    for (_, mut g) in groups {
        if g.len() < 2 {
            continue;
        }
        g.sort_by(|a, b| crate::inf::cmp_driver(&a[6], &a[5], &b[6], &b[5]));
        let newest = g.pop().map(|r| r[5].clone()).unwrap_or_default();
        for r in g.into_iter().filter(|r| r[7] == "0") {
            let folder = Path::new(&r[2]).parent().map(Path::to_path_buf).unwrap_or_default();
            out.push((r[1].clone(), folder, format!("{} {} driver {} ({}), replaced by {newest}", r[3], r[4], r[5], r[6])));
        }
    }
    out.sort();
    out
}

pub fn is_oem_inf(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("oem") && l.ends_with(".inf") && l[3..l.len() - 4].chars().all(|c| c.is_ascii_digit()) && l.len() > 7
}

pub fn is_package_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-~".contains(c))
}

/// Finds and measures everything the cleaner can remove. Takes 10-30 seconds.
pub fn find_junk() -> Vec<Junk> {
    crate::platform::find_junk()
}

/// Cleans one item. Returns bytes freed.
pub fn clean(j: &Junk) -> Result<u64, String> {
    crate::platform::clean(j)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletes_inside_and_guards() {
        let root = std::env::temp_dir().join(format!("flash-clean-test-{}", std::process::id())).join("junk");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.tmp"), vec![0u8; 1000]).unwrap();
        std::fs::write(root.join("sub").join("b.tmp"), vec![0u8; 500]).unwrap();
        assert_eq!(dir_size(&root), 1500);
        assert_eq!(delete_contents(&root), 1500);
        assert!(root.exists() && dir_size(&root) == 0); // the folder itself stays
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
        #[cfg(windows)]
        {
            assert!(!safe_target(Path::new("C:\\")));
            assert!(!safe_target(Path::new("C:\\Windows")));
            assert!(safe_target(Path::new("C:\\Windows\\Temp")));
            assert_eq!(delete_contents(Path::new("C:\\Windows")), 0);
        }
        #[cfg(unix)]
        {
            for shallow in ["/", "/tmp", "/usr", "/var/tmp", "/home/you", "relative/a/b/c", "/home/you/../../etc"] {
                assert!(!safe_target(Path::new(shallow)), "{shallow}");
            }
            assert!(safe_target(Path::new("/home/you/.cache")));
            assert!(safe_target(Path::new("/home/you/.cache/thumbnails")));
            assert_eq!(delete_contents(Path::new("/usr")), 0);
        }
    }

    #[test]
    fn names_and_old_drivers() {
        assert!(is_oem_inf("oem12.inf") && !is_oem_inf("oem.inf") && !is_oem_inf("oem1.inf & calc"));
        assert!(is_package_name("Microsoft.BingNews_4.1.0.0_x64__8wekyb3d8bbwe") && !is_package_name("x'; rm"));
        assert_eq!(human(1_500_000_000), "1.5 GB");
        let row = |oem: &str, ver: &str, date: &str, used: &str| -> Vec<String> {
            ["DRV", oem, &format!("C:\\Windows\\System32\\DriverStore\\FileRepository\\nv.inf_amd64_{oem}\\nv.inf"), "NVIDIA", "Display", ver, date, used].iter().map(|s| s.to_string()).collect()
        };
        let old = old_drivers(&[row("oem1.inf", "31.0.0.1", "2024-01-01", "0"), row("oem2.inf", "32.0.0.1", "2025-01-01", "1"), row("oem3.inf", "30.0.0.1", "2023-01-01", "1")]);
        assert_eq!(old.len(), 1); // oem3 is older but still in use, so it stays
        assert_eq!(old[0].0, "oem1.inf");
        assert!(old[0].2.contains("replaced by 32.0.0.1"));
    }
}
