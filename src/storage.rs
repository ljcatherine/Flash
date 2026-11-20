//! Storage: drive usage, what's taking space on the system drive, and a cleaner for
//! temporary files, caches, old driver versions and preinstalled apps.
use crate::{ps, rows, run_hidden};
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

/// Calls `f` for every file under `path`. Links and junctions are never followed.
fn walk(path: &Path, f: &mut impl FnMut(&Path, u64)) {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return };
    if meta.file_type().is_symlink() {
        return;
    }
    if meta.is_file() {
        f(path, meta.len());
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else { return };
    for e in entries.flatten() {
        walk(&e.path(), f);
    }
}

pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    walk(path, &mut |_, len| total += len);
    total
}

fn env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from).filter(|p| p.is_absolute())
}

#[derive(Clone, Debug, Default)]
pub struct Breakdown {
    pub drive: String,
    pub total: u64,
    pub used: u64,
    pub parts: Vec<(&'static str, u64)>,
    pub largest: Vec<(PathBuf, u64)>,
}

const USER_DIRS: [&str; 6] = ["Desktop", "Documents", "Downloads", "Pictures", "Videos", "Music"];

/// Measures the main folders on the system drive and finds the biggest files in your folders.
pub fn breakdown() -> Breakdown {
    let sys = env("SystemDrive").map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| "C:".into());
    let (total, used) = drives().iter().find(|d| d.mount.to_lowercase().starts_with(&sys.to_lowercase())).map_or((0, 0), |d| (d.total, d.total - d.free));
    let home = env("USERPROFILE");
    let user_dirs: Vec<PathBuf> = home.iter().flat_map(|h| USER_DIRS.iter().map(move |d| h.join(d))).collect();
    let parts: Vec<(&'static str, Vec<PathBuf>)> = vec![
        ("Windows", env("SystemRoot").into_iter().collect()),
        ("Programs", ["ProgramFiles", "ProgramFiles(x86)"].iter().filter_map(|v| env(v)).collect()),
        ("Program data", env("ProgramData").into_iter().collect()),
        ("Your files", user_dirs.clone()),
        ("App data & caches", home.iter().map(|h| h.join("AppData")).collect()),
    ];
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
    // Page file, hibernation file, restore points, other users...
    parts.push(("System & other", used.saturating_sub(known)));
    Breakdown { drive: sys, total, used, parts, largest }
}

// ---------- Cleaner ----------

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Paths(Vec<PathBuf>),
    RecycleBin,
    UpdateCache(PathBuf),
    DeliveryOptimization(PathBuf),
    OldDriver(String), // oemNN.inf
    App(String),       // package full name
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

/// Refuses anything shallower than e.g. `C:\Windows\Temp`, so a bad environment
/// variable can never point the cleaner at a drive root or the Windows folder.
pub fn safe_target(p: &Path) -> bool {
    p.is_absolute() && p.components().count() >= 4
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

const SCAN: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
$inuse = @{}
Get-CimInstance Win32_PnPSignedDriver | ForEach-Object { if ($_.InfName) { $inuse[$_.InfName.ToLower()] = 1 } }
Get-WindowsDriver -Online | ForEach-Object {
  @('DRV', $_.Driver, $_.OriginalFileName, $_.ProviderName, $_.ClassName, $_.Version, $_.Date.ToString('yyyy-MM-dd'), [int]$inuse.ContainsKey($_.Driver.ToLower())) -join "`t"
}
$rb = [int64]0
(New-Object -ComObject Shell.Application).NameSpace(10).Items() | ForEach-Object { $rb += [int64]$_.ExtendedProperty('Size') }
"RB`t$rb"
$known = 'Microsoft.BingNews','Microsoft.BingWeather','Microsoft.BingSearch','Microsoft.GetHelp','Microsoft.Getstarted','Microsoft.MicrosoftSolitaireCollection','Microsoft.WindowsFeedbackHub','Clipchamp.Clipchamp','Microsoft.MicrosoftOfficeHub','Microsoft.People','Microsoft.SkypeApp','Microsoft.MixedReality.Portal','Microsoft.Microsoft3DViewer','Microsoft.549981C3F5F10','Microsoft.WindowsMaps','Microsoft.ZuneVideo','Microsoft.PowerAutomateDesktop'
Get-AppxPackage | Where-Object { $known -contains $_.Name -or $_.Name -like 'king.com.*' -or $_.Name -like '*Disney*' -or $_.Name -like '*TikTok*' -or $_.Name -like 'Facebook.*' } | ForEach-Object {
  "APP`t$($_.Name)`t$($_.PackageFullName)`t$($_.InstallLocation)"
}
"#;

fn app_name(package: &str) -> String {
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

fn is_oem_inf(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("oem") && l.ends_with(".inf") && l[3..l.len() - 4].chars().all(|c| c.is_ascii_digit()) && l.len() > 7
}

fn is_package_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-~".contains(c))
}

/// Finds and measures everything the cleaner can remove. Takes 10-30 seconds.
pub fn find_junk() -> Vec<Junk> {
    let local = env("LOCALAPPDATA");
    let sysroot = env("SystemRoot");
    let pdata = env("ProgramData");
    let mut items: Vec<(&str, &str, &str, bool, Vec<PathBuf>)> = Vec::new();
    let l = |s: &str| local.iter().map(|p| p.join(s)).collect::<Vec<_>>();
    let w = |s: &str| sysroot.iter().map(|p| p.join(s)).collect::<Vec<_>>();
    items.push(("temp-user", "Your temporary files", "Files apps left in your Temp folder. Anything still in use is skipped.", true, vec![std::env::temp_dir()]));
    items.push(("temp-win", "Windows temporary files", "Leftovers from installers and Windows itself.", true, w("Temp")));
    items.push(("thumbs", "Thumbnail cache", "Picture previews Explorer rebuilds when needed.", true, l("Microsoft\\Windows\\Explorer")));
    let mut dumps = w("Minidump");
    dumps.extend(w("MEMORY.DMP"));
    dumps.extend(l("CrashDumps"));
    dumps.extend(pdata.iter().flat_map(|p| [p.join("Microsoft\\Windows\\WER\\ReportArchive"), p.join("Microsoft\\Windows\\WER\\ReportQueue")]));
    items.push(("dumps", "Crash dumps and error reports", "Saved after crashes, for troubleshooting. Safe to remove once things are stable.", true, dumps));
    let mut browser = Vec::new();
    for base in ["Google\\Chrome\\User Data", "Microsoft\\Edge\\User Data", "BraveSoftware\\Brave-Browser\\User Data"] {
        for root in l(base) {
            for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name == "Default" || name.starts_with("Profile ") {
                    browser.extend(["Cache", "Code Cache", "GPUCache"].iter().map(|c| e.path().join(c)));
                }
            }
        }
    }
    items.push(("browser", "Browser caches", "Cached web pages for Chrome, Edge and Brave. You stay signed in; pages load a little slower at first.", true, browser));
    let mut shaders = l("D3DSCache");
    shaders.extend(l("NVIDIA\\DXCache"));
    shaders.extend(l("NVIDIA\\GLCache"));
    shaders.extend(l("AMD\\DxCache"));
    items.push(("shaders", "Graphics shader caches", "Games rebuild these, so the first launch after cleaning can stutter briefly.", false, shaders));

    let mut out: Vec<Junk> = items
        .into_iter()
        .map(|(id, name, about, on, paths)| {
            let paths: Vec<PathBuf> = paths.into_iter().filter(|p| safe_target(p) && p.exists()).collect();
            Junk { id: id.into(), group: "Temporary files & caches", name: name.into(), about: about.into(), size: paths.iter().map(|p| dir_size(p)).sum(), default_on: on, kind: Kind::Paths(paths) }
        })
        .collect();

    if let Some(root) = &sysroot {
        let dl = root.join("SoftwareDistribution\\Download");
        out.push(Junk { id: "wu".into(), group: "Temporary files & caches", name: "Windows Update downloads".into(), about: "Installer files for updates that are already installed.".into(), size: dir_size(&dl), default_on: true, kind: Kind::UpdateCache(dl) });
        let dobj = root.join("ServiceProfiles\\NetworkService\\AppData\\Local\\Microsoft\\Windows\\DeliveryOptimization\\Cache");
        out.push(Junk { id: "do".into(), group: "Temporary files & caches", name: "Delivery Optimization cache".into(), about: "Update pieces Windows shares with other PCs.".into(), size: dir_size(&dobj), default_on: true, kind: Kind::DeliveryOptimization(dobj) });
    }

    let (text, _) = ps(SCAN);
    let r = rows(&text, 8);
    let rb: u64 = r.iter().find(|x| x[0] == "RB").and_then(|x| x[1].parse().ok()).unwrap_or(0);
    out.push(Junk { id: "recycle".into(), group: "Temporary files & caches", name: "Recycle Bin".into(), about: "Permanently deletes what's in the Recycle Bin.".into(), size: rb, default_on: false, kind: Kind::RecycleBin });

    let drv: Vec<Vec<String>> = r.iter().filter(|x| x[0] == "DRV").cloned().collect();
    for (oem, folder, about) in old_drivers(&drv) {
        if is_oem_inf(&oem) {
            out.push(Junk { id: format!("drv-{oem}"), group: "Old driver versions", name: oem.clone(), about, size: dir_size(&folder), default_on: false, kind: Kind::OldDriver(oem) });
        }
    }
    for a in r.iter().filter(|x| x[0] == "APP" && is_package_name(&x[2])) {
        out.push(Junk {
            id: format!("app-{}", a[1]),
            group: "Apps you may not need",
            name: app_name(&a[1]),
            about: "Came with Windows. You can reinstall it from the Microsoft Store.".into(),
            size: dir_size(Path::new(&a[3])),
            default_on: false,
            kind: Kind::App(a[2].clone()),
        });
    }
    out
}

/// Cleans one item. Returns bytes freed.
pub fn clean(j: &Junk) -> Result<u64, String> {
    match &j.kind {
        Kind::Paths(paths) => Ok(paths.iter().map(|p| delete_contents(p)).sum()),
        Kind::RecycleBin => {
            let (_, err) = ps("Clear-RecycleBin -Force -ErrorAction Stop");
            if err.trim().is_empty() { Ok(j.size) } else { Err(err.trim().to_string()) }
        }
        Kind::UpdateCache(p) => {
            // The update service holds these files open; pause it while cleaning.
            run_hidden("net", &["stop", "wuauserv"]);
            let freed = delete_contents(p);
            run_hidden("net", &["start", "wuauserv"]);
            Ok(freed)
        }
        Kind::DeliveryOptimization(p) => {
            let before = dir_size(p);
            ps("Delete-DeliveryOptimizationCache -Force");
            Ok(before.saturating_sub(dir_size(p)))
        }
        Kind::OldDriver(oem) if is_oem_inf(oem) => {
            // Without /force, pnputil refuses a package any device still uses.
            let out = run_hidden("pnputil", &["/delete-driver", oem]);
            if out.contains("deleted successfully") { Ok(j.size) } else { Err(out.trim().lines().last().unwrap_or("pnputil failed").to_string()) }
        }
        Kind::App(full) if is_package_name(full) => {
            let (_, err) = ps(&format!("Remove-AppxPackage -Package '{full}' -ErrorAction Stop"));
            if err.trim().is_empty() { Ok(j.size) } else { Err(err.trim().lines().next().unwrap_or("").to_string()) }
        }
        _ => Err("refused: unexpected name".into()),
    }
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
        assert!(!safe_target(Path::new("C:\\")));
        assert!(!safe_target(Path::new("C:\\Windows")));
        assert!(safe_target(Path::new("C:\\Windows\\Temp")));
        assert_eq!(delete_contents(Path::new("C:\\Windows")), 0);
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
