//! The four update sources on Linux and installing from them:
//! 1. Distribution packages: kernel, linux-firmware, microcode, Mesa, NVIDIA, DKMS.
//! 2. Firmware from the LVFS through fwupd.
//! 3. Installed kernel modules that fit a device better than the one it uses.
//! 4. A local folder of fwupd .cab files and .deb/.rpm/.pkg.tar packages.
use super::packages::{self, Kind, Pm};
use super::{cmd, fwupd, helper};
use crate::extras::{self, Change};
use crate::{Device, Shared, Source, Update, found, log, source_status, status};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Checks one update source. `folder` is the chosen folder for `Source::Folder`.
pub fn check_source(src: Source, d: &Shared, devices: &[Device], _win11: bool, folder: Option<&str>) -> Vec<Update> {
    match src {
        Source::System => package_updates(d, devices),
        Source::Catalog => firmware_updates(d, devices),
        Source::Store => module_updates(d, devices),
        Source::Folder => folder.map(|f| folder_updates(d, devices, f)).unwrap_or_default(),
    }
}

// ---------------- 1. Distribution packages ----------------

/// Display devices a graphics package drives: NVIDIA's own driver, or the in-kernel ones.
fn gpu_devices(devices: &[Device], nvidia: bool) -> Vec<&Device> {
    devices
        .iter()
        .filter(|x| crate::purpose(&x.class) == "Display" && !x.instance.starts_with("fwupd:"))
        .filter(|x| {
            let nv = x.inf.starts_with("nvidia") || x.hwids.iter().any(|h| h.starts_with("10de:"));
            nv == nvidia
        })
        .collect()
}

/// Groups driver package upgrades into one update per kind (per device for graphics and
/// DKMS), so the NVIDIA driver's twenty packages are one line, like on Windows.
pub(crate) fn group_upgrades(list: &[packages::Upgrade], devices: &[Device]) -> Vec<Update> {
    let mut groups: BTreeMap<(Kind, String), Vec<&packages::Upgrade>> = BTreeMap::new();
    for u in list.iter().filter(|u| packages::is_name(&u.name) && packages::is_version(&u.new)) {
        let Some(k) = packages::kind(&u.name) else { continue };
        // DKMS packages are each their own driver.
        let sub = if k == Kind::Dkms { u.name.clone() } else { String::new() };
        groups.entry((k, sub)).or_default().push(u);
    }
    let mut out = Vec::new();
    for ((k, sub), ups) in groups {
        let names: Vec<&str> = ups.iter().map(|u| u.name.as_str()).collect();
        let main = ups.iter().find(|u| u.name == packages::primary(k, &names)).copied().unwrap_or(ups[0]);
        let title = match (k, ups.len()) {
            (Kind::Dkms, _) => format!("{} ({})", k.title(), sub),
            (_, 1) => format!("{} ({})", k.title(), main.name),
            (_, n) => format!("{} ({} and {} more)", k.title(), main.name, n - 1),
        };
        let targets: Vec<&Device> = match k {
            Kind::Nvidia => gpu_devices(devices, true),
            Kind::Mesa | Kind::Xorg => gpu_devices(devices, false),
            Kind::Dkms => {
                let stem = sub.trim_end_matches("-dkms").trim_start_matches("akmod-").trim_start_matches("kmod-").replace('-', "_");
                devices.iter().filter(|x| x.inf.split(", ").any(|m| m == stem)).collect()
            }
            _ => vec![],
        };
        let base = Update {
            source: Source::System,
            key: format!("pkg:{}", names.join(",")),
            title,
            version: main.new.clone(),
            installed: main.old.clone(),
            ..Default::default()
        };
        if targets.is_empty() {
            out.push(base); // system-wide: the kernel, firmware, microcode
        } else {
            for t in targets {
                out.push(Update { device: Some(t.instance.clone()), hwid: t.hwids.first().cloned().unwrap_or_default(), ..base.clone() });
            }
        }
    }
    out
}

fn package_updates(d: &Shared, devices: &[Device]) -> Vec<Update> {
    source_status(d, Source::System, "Checking...".into());
    let pm = match packages::detect() {
        Ok(pm) => pm,
        Err(why) => {
            source_status(d, Source::System, why.into());
            return vec![];
        }
    };
    match packages::upgrades(pm) {
        Ok(list) => {
            let ups = group_upgrades(&list, devices);
            let n = ups.iter().map(|u| &u.key).collect::<HashSet<_>>().len();
            source_status(d, Source::System, found(n));
            ups
        }
        Err(e) => {
            log(d, &format!("Distribution packages: {}", e.trim()));
            source_status(d, Source::System, "Couldn't check".into());
            vec![]
        }
    }
}

// ---------------- 2. Firmware (LVFS) ----------------

fn firmware_updates(d: &Shared, devices: &[Device]) -> Vec<Update> {
    source_status(d, Source::Catalog, "Checking...".into());
    match fwupd::updates(devices) {
        Ok(ups) => {
            source_status(d, Source::Catalog, found(ups.len()));
            ups
        }
        Err(e) => {
            if super::has("fwupdmgr") {
                log(d, &format!("Firmware (LVFS): {e}"));
                source_status(d, Source::Catalog, "Couldn't check".into());
            } else {
                source_status(d, Source::Catalog, "fwupd isn't installed".into());
            }
            vec![]
        }
    }
}

// ---------------- 3. Installed modules ----------------

/// Kernel module names as the helper accepts them.
fn is_module(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// For each device: an installed out-of-tree module (a vendor driver) that matches it but
/// isn't the one it uses, or any module for a device that has none.
fn module_updates(d: &Shared, devices: &[Device]) -> Vec<Update> {
    source_status(d, Source::Store, "Reading modules...".into());
    let mut ups = Vec::new();
    let mut info: HashMap<String, Option<super::devices::ModInfo>> = HashMap::new();
    let mut modinfo = |m: &str| info.entry(m.to_string()).or_insert_with(|| cmd("modinfo", &[m]).ok().map(|t| super::devices::parse_modinfo(&t))).clone();
    let mut checked = 0;
    for dev in devices.iter().filter(|x| !x.instance.starts_with("fwupd:")) {
        let Some(alias) = dev.hwids.first().filter(|h| h.starts_with("pci:")).or_else(|| dev.compat.first().filter(|h| h.starts_with("usb:"))) else { continue };
        checked += 1;
        let Ok(out) = cmd("modprobe", &["-R", alias]) else { continue };
        let current: Vec<&str> = dev.inf.split(", ").filter(|s| !s.is_empty()).collect();
        for cand in out.lines().map(str::trim).filter(|m| is_module(m) && !current.contains(m)) {
            let Some(mi) = modinfo(cand) else { continue };
            let vendor = !mi.intree;
            // A vendor module beside an in-kernel driver, or anything for a device with no driver.
            if !(vendor && super::devices::is_builtin(&dev.provider) || dev.inf.is_empty()) {
                continue;
            }
            let old = current.first().copied().filter(|m| is_module(m)).unwrap_or("");
            let what = if mi.description.is_empty() { cand.to_string() } else { format!("{cand}: {}", mi.description) };
            ups.push(Update {
                source: Source::Store,
                key: format!("mod:{cand}:{old}"),
                title: if old.is_empty() { format!("Load {what}") } else { format!("Use {what} instead of {old}") },
                version: if mi.version.is_empty() { super::read("/proc/sys/kernel/osrelease") } else { mi.version.clone() },
                date: String::new(),
                size: String::new(),
                device: Some(dev.instance.clone()),
                hwid: alias.clone(),
                vendor_swap: !old.is_empty(),
                installed: if old.is_empty() { String::new() } else { dev.version.clone() },
            });
            break;
        }
    }
    source_status(d, Source::Store, format!("{} from {checked} devices", found(ups.len())));
    ups
}

// ---------------- 4. Local folder ----------------

/// Firmware archives and driver packages in a folder (and its subfolders, a few levels deep).
fn folder_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0)];
    while let Some((d, depth)) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
            let name = e.file_name().to_string_lossy().to_string();
            if m.is_dir() && depth < 4 {
                stack.push((e.path(), depth + 1));
            } else if m.is_file() && [".cab", ".deb", ".rpm", ".pkg.tar.zst", ".pkg.tar.xz"].iter().any(|x| name.ends_with(x)) {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

fn folder_updates(d: &Shared, devices: &[Device], dir: &str) -> Vec<Update> {
    source_status(d, Source::Folder, "Reading packages...".into());
    let files = folder_files(Path::new(dir));
    let pm = packages::detect().ok();
    let mut ups = Vec::new();
    for f in &files {
        let path = f.to_string_lossy().into_owned();
        if path.ends_with(".cab") {
            let Some((fw, rel)) = fwupd::cab_details(&path) else { continue };
            if packages::cmp_versions(&rel.version, &fw.version) != std::cmp::Ordering::Greater {
                continue;
            }
            let instance = format!("fwupd:{}", fw.id);
            ups.push(Update {
                source: Source::Folder,
                key: format!("file:{path}"),
                title: format!("{} {} ({})", if rel.name.is_empty() { &fw.name } else { &rel.name }, rel.version, f.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()),
                version: rel.version.clone(),
                date: if rel.created > 0 { extras::date(rel.created) } else { String::new() },
                device: devices.iter().any(|x| x.instance == instance).then_some(instance),
                hwid: fw.instance_ids.first().cloned().unwrap_or_default(),
                installed: fw.version.clone(),
                ..Default::default()
            });
            continue;
        }
        let Some((name, version)) = packages::file_package(f) else { continue };
        let Some(kind) = packages::kind(&name) else { continue }; // driver packages only
        let have = pm.and_then(|pm| packages::installed(pm, &name));
        if have.as_deref().is_some_and(|h| packages::cmp_versions(&version, h) != std::cmp::Ordering::Greater) {
            continue;
        }
        let up = packages::Upgrade { name: name.clone(), old: have.clone().unwrap_or_default(), new: version };
        for mut u in group_upgrades(&[up], devices) {
            u.source = Source::Folder;
            u.key = format!("file:{path}");
            u.title = format!("{} ({})", kind.title(), f.file_name().map(|n| n.to_string_lossy()).unwrap_or_default());
            ups.push(u);
        }
    }
    source_status(d, Source::Folder, format!("{} from {} packages", found(ups.len()), files.len()));
    ups
}

// ---------------- Installing ----------------

/// Whether a snapshot can be made before installing (Snapper or Timeshift).
pub fn snapshot_status() -> Result<(), String> {
    if super::has("snapper") || super::has("timeshift") {
        Ok(())
    } else {
        Err("No snapshot tool (Snapper or Timeshift) is set up, so Flash can't make a restore point first.".into())
    }
}

/// Arch doesn't support partial upgrades, so installing any package there upgrades everything.
pub fn install_note(updates: &[Update]) -> Option<&'static str> {
    let pkgs = updates.iter().any(|u| u.source == Source::System || u.key.ends_with(".pkg.tar.zst"));
    (pkgs && packages::detect() == Ok(Pm::Pacman)).then_some("Arch Linux doesn't support partial upgrades, so this runs a full system upgrade (pacman -Syu).")
}

fn names_of(key: &str) -> Vec<String> {
    key.strip_prefix("pkg:").map(|n| n.split(',').filter(|s| packages::is_name(s)).map(String::from).collect()).unwrap_or_default()
}

/// Installs the chosen updates, each through the right channel, then rescans.
pub fn install(d: &Shared, updates: &[Update], restore_point: bool) {
    if restore_point {
        status(d, "Creating a snapshot...");
        match helper("manage", "snapshot", &[]) {
            Ok(out) => {
                log(d, &out);
                log(d, "Snapshot created.");
            }
            Err(e) => log(d, &format!("Snapshot not created: {}", e.trim())),
        }
    }
    let before: Vec<Device> = d.lock().unwrap().devices.clone();
    let stamp = extras::now();
    let pm = packages::detect().ok();
    let mut changes: Vec<Change> = Vec::new();
    let mut done = HashSet::new();
    let device_name = |u: &Update| u.device.as_ref().and_then(|id| before.iter().find(|x| &x.instance == id)).map_or_else(|| u.title.clone(), |x| x.name.clone());

    // 1. Packages: one transaction for all of them.
    let pkg_updates: Vec<&Update> = updates.iter().filter(|u| u.source == Source::System && u.key.starts_with("pkg:")).collect();
    let names: Vec<String> = pkg_updates.iter().flat_map(|u| names_of(&u.key)).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    if !names.is_empty() {
        let was: HashMap<String, String> = pm.map(|pm| names.iter().filter_map(|n| packages::installed(pm, n).map(|v| (n.clone(), v))).collect()).unwrap_or_default();
        status(d, &format!("Installing {} package(s) from your distribution...", names.len()));
        let args: Vec<&str> = names.iter().map(String::as_str).collect();
        match helper("manage", "pkg-upgrade", &args) {
            Ok(out) => log(d, &out),
            Err(e) => log(d, &format!("Error: {}", e.trim())),
        }
        for u in pkg_updates.iter().filter(|u| done.insert(u.key.clone())) {
            let group = names_of(&u.key);
            let now: HashMap<String, String> = pm.map(|pm| group.iter().filter_map(|n| packages::installed(pm, n).map(|v| (n.clone(), v))).collect()).unwrap_or_default();
            let changed: Vec<String> = group.iter().filter(|n| was.get(*n).is_some_and(|w| now.get(*n) != Some(w))).map(|n| format!("{n}={}", was[n])).collect();
            let main = group.iter().find(|n| now.contains_key(*n)).cloned().unwrap_or_default();
            if changed.is_empty() {
                log(d, &format!("Not changed: {} is still on {}.", u.title, if u.installed.is_empty() { "the same version" } else { &u.installed }));
                continue;
            }
            log(d, &format!("Updated {}: {} -> {}", u.title, u.installed, now.get(&main).cloned().unwrap_or_else(|| u.version.clone())));
            changes.push(Change {
                time: stamp,
                device: device_name(u),
                instance: u.device.clone().unwrap_or_default(),
                old_version: u.installed.clone(),
                new_version: now.get(&main).cloned().unwrap_or_else(|| u.version.clone()),
                undo: format!("pkg:{}", changed.join(",")),
                ..Default::default()
            });
            if packages::kind(&main) == Some(Kind::Kernel) {
                log(d, "Restart the PC to start using the new kernel. The previous kernel stays installed as a fallback.");
            }
        }
    }

    // 2. Firmware.
    for u in updates.iter().filter(|u| u.source == Source::Catalog && done.insert(u.key.clone())) {
        let Some(id) = u.key.strip_prefix("fw:").filter(|id| fwupd::is_id(id)) else { continue };
        status(d, &format!("Installing {}...", u.title));
        match fwupd::update(id) {
            Ok(out) => log(d, &out),
            Err(e) => log(d, &format!("Error: {}", e.trim())),
        }
        let now = fwupd::version_of(id).unwrap_or_default();
        if !now.is_empty() && now != u.installed {
            log(d, &format!("Updated {}: {} -> {now}", device_name(u), u.installed));
            changes.push(Change { time: stamp, device: device_name(u), instance: u.device.clone().unwrap_or_default(), old_version: u.installed.clone(), new_version: now, undo: format!("fw:{id}={}", u.installed), ..Default::default() });
        } else {
            log(d, &format!("Not changed yet: {}. Some firmware installs at the next restart.", device_name(u)));
        }
    }

    // 3. Kernel modules.
    for u in updates.iter().filter(|u| u.source == Source::Store && done.insert(u.key.clone())) {
        let mut parts = u.key.strip_prefix("mod:").unwrap_or("").splitn(2, ':');
        let (new, old) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        if !is_module(new) || !(old.is_empty() || is_module(old)) {
            continue;
        }
        status(d, &format!("Installing {}...", u.title));
        let res = if old.is_empty() { helper("manage", "module-load", &[new]) } else { helper("manage", "module-prefer", &[new, old]) };
        match res {
            Ok(out) => {
                log(d, &out);
                let undo = if old.is_empty() { format!("modload:{new}") } else { format!("mod:{new}") };
                changes.push(Change { time: stamp, device: device_name(u), instance: u.device.clone().unwrap_or_default(), old_version: if old.is_empty() { "no driver".into() } else { old.to_string() }, new_version: new.to_string(), new_inf: new.to_string(), old_inf: old.to_string(), undo, ..Default::default() });
            }
            Err(e) => log(d, &format!("Error: {}", e.trim())),
        }
    }

    // 4. Files from a folder.
    for u in updates.iter().filter(|u| u.source == Source::Folder && done.insert(u.key.clone())) {
        let Some(path) = u.key.strip_prefix("file:") else { continue };
        status(d, &format!("Installing {}...", u.title));
        if path.ends_with(".cab") {
            match fwupd::install_cab(path) {
                Ok(out) => log(d, &out),
                Err(e) => log(d, &format!("Error: {}", e.trim())),
            }
            continue;
        }
        let pkg = packages::file_package(Path::new(path));
        match helper("manage", "pkg-install-file", &[path]) {
            Ok(out) => {
                log(d, &out);
                if let (Some((name, _)), Some(pm)) = (pkg, pm) {
                    let now = packages::installed(pm, &name).unwrap_or_default();
                    if !u.installed.is_empty() && now != u.installed {
                        log(d, &format!("Updated {}: {} -> {now}", device_name(u), u.installed));
                        changes.push(Change { time: stamp, device: device_name(u), instance: u.device.clone().unwrap_or_default(), old_version: u.installed.clone(), new_version: now, undo: format!("pkg:{name}={}", u.installed), ..Default::default() });
                    }
                }
            }
            Err(e) => log(d, &format!("Error: {}", e.trim())),
        }
    }

    crate::scan(d);
    if !changes.is_empty() {
        let mut state = extras::load();
        state.history.extend(changes);
        extras::save(&state);
    }
}

/// Changes Flash recorded how to undo.
pub fn can_undo(c: &Change) -> bool {
    !c.undo.is_empty()
}

/// Undoes a change: downgrades the packages, reinstalls the old firmware, or removes the
/// module setting.
pub fn rollback(c: &Change) -> Result<String, String> {
    if let Some(pairs) = c.undo.strip_prefix("pkg:") {
        let pairs: Vec<&str> = pairs.split(',').filter(|p| p.split_once('=').is_some_and(|(n, v)| packages::is_name(n) && packages::is_version(v))).collect();
        if pairs.is_empty() {
            return Err("Nothing recorded to roll back.".into());
        }
        helper("manage", "pkg-downgrade", &pairs)
    } else if let Some((id, version)) = c.undo.strip_prefix("fw:").and_then(|r| r.split_once('=')) {
        fwupd::install_version(id, version)
    } else if let Some(module) = c.undo.strip_prefix("mod:").filter(|m| is_module(m)) {
        helper("manage", "module-unprefer", &[module])
    } else if let Some(module) = c.undo.strip_prefix("modload:").filter(|m| is_module(m)) {
        helper("manage", "module-unload", &[module])
    } else {
        Err("This change can't be undone from Flash; use your package manager.".into())
    }
}

// ---------------- Backup ----------------

/// Saves a copy of every out-of-tree module in use, with a manifest of the kernel, the
/// driver packages and the firmware versions, so the setup can be rebuilt later.
pub fn export_drivers(dir: &str) -> String {
    let stamp = extras::date(extras::now());
    let out = Path::new(dir).join(format!("flash-driver-backup-{stamp}"));
    if let Err(e) = std::fs::create_dir_all(&out) {
        return format!("Couldn't create {}: {e}", out.display());
    }
    let (devices, _, _) = super::read_devices();
    let mut manifest = format!("Flash driver backup, {stamp}\nKernel: {}\n\nDEVICES\n", super::read("/proc/sys/kernel/osrelease"));
    let mut copied = 0;
    let mut seen = HashSet::new();
    for dev in &devices {
        manifest += &format!("{}\t{}\t{}\t{}\n", dev.name, dev.inf, dev.version, dev.provider);
        if super::devices::is_builtin(&dev.provider) || dev.instance.starts_with("fwupd:") {
            continue;
        }
        for m in dev.inf.split(", ").filter(|m| is_module(m) && seen.insert(m.to_string())) {
            let Ok(file) = cmd("modinfo", &["-n", m]) else { continue };
            let file = PathBuf::from(file.trim());
            if let (Some(name), true) = (file.file_name(), file.is_file()) {
                if std::fs::copy(&file, out.join(name)).is_ok() {
                    copied += 1;
                }
            }
        }
    }
    if let Ok(pm) = packages::detect() {
        manifest += "\nDRIVER PACKAGES\n";
        let list = match pm {
            Pm::Apt => cmd("dpkg-query", &["-W", "-f=${Package}\t${Version}\n"]),
            Pm::Dnf | Pm::Zypper => cmd("rpm", &["-qa", "--qf", "%{NAME}\t%{VERSION}-%{RELEASE}\n"]),
            Pm::Pacman => cmd("pacman", &["-Q"]).map(|o| o.replace(' ', "\t")),
        }
        .unwrap_or_default();
        let mut lines: Vec<&str> = list.lines().filter(|l| l.split('\t').next().is_some_and(|n| packages::kind(n).is_some())).collect();
        lines.sort();
        for l in lines {
            manifest += l;
            manifest.push('\n');
        }
    }
    if std::fs::write(out.join("manifest.txt"), &manifest).is_err() {
        return format!("Couldn't write the manifest in {}", out.display());
    }
    format!("Saved {copied} out-of-tree module(s) and a manifest of {} devices to {}.", devices.len(), out.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(instance: &str, inf: &str, hwid: &str) -> Device {
        Device { instance: instance.into(), class: "VGA compatible controller".into(), inf: inf.into(), hwids: vec![hwid.into()], provider: if inf == "nvidia" { "Out-of-tree module".into() } else { "Linux kernel".into() }, ..Default::default() }
    }

    #[test]
    fn groups_driver_packages() {
        let apt = packages::parse_apt(include_str!("../../../tests/fixtures/linux/apt-list-upgradable.txt"));
        let ups = group_upgrades(&apt, &[]);
        // libc isn't a driver; the microcode is, and it belongs to no single device.
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].title, "CPU microcode (amd64-microcode)");
        assert_eq!((ups[0].installed.as_str(), ups[0].version.as_str()), ("3.20231019.1ubuntu2", "3.20251202.1ubuntu0.24.04.1"));
        assert_eq!(ups[0].key, "pkg:amd64-microcode");
        assert!(ups[0].device.is_none() && ups[0].hwid.is_empty());

        let up = |n: &str, o: &str, v: &str| packages::Upgrade { name: n.into(), old: o.into(), new: v.into() };
        let list = [up("libnvidia-gl-550", "550.107", "550.120"), up("nvidia-driver-550", "550.107", "550.120"), up("mesa-dri-drivers", "24.1.0-1", "24.2.4-1"), up("r8125-dkms", "9.013", "9.014"), up("firefox", "1", "2")];
        let devs = [gpu("0000:01:00.0", "nvidia", "10de:2484"), gpu("0000:0c:00.0", "amdgpu", "1002:73bf"), Device { instance: "0000:05:00.0".into(), inf: "r8125".into(), ..Default::default() }];
        let ups = group_upgrades(&list, &devs);
        let nv = ups.iter().find(|u| u.title.starts_with("NVIDIA")).unwrap();
        assert_eq!(nv.title, "NVIDIA driver (nvidia-driver-550 and 1 more)");
        assert_eq!(nv.key, "pkg:libnvidia-gl-550,nvidia-driver-550");
        assert_eq!(nv.device.as_deref(), Some("0000:01:00.0"));
        let mesa = ups.iter().find(|u| u.title.starts_with("Mesa")).unwrap();
        assert_eq!(mesa.device.as_deref(), Some("0000:0c:00.0"));
        let dkms = ups.iter().find(|u| u.title.contains("r8125")).unwrap();
        assert_eq!(dkms.device.as_deref(), Some("0000:05:00.0"));
        assert!(!ups.iter().any(|u| u.key.contains("firefox")));
        assert_eq!(names_of(&nv.key), ["libnvidia-gl-550", "nvidia-driver-550"]);
    }

    #[test]
    fn undo_tokens() {
        let c = |undo: &str| Change { undo: undo.into(), ..Default::default() };
        assert!(can_undo(&c("pkg:mesa=24.1")) && !can_undo(&c("")));
        assert!(rollback(&c("pkg:;rm=1")).is_err());
        assert!(rollback(&c("mod:../x")).is_err());
        assert!(rollback(&c("other")).is_err());
        assert!(names_of("pkg:a,b;c,d").iter().all(|n| packages::is_name(n)));
    }
}
