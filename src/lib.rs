//! Device scanning and driver updates from four sources. On Windows: Windows Update, the
//! Microsoft Update Catalog, the local Driver Store and a folder the user picks. On Linux:
//! the distribution's packages, firmware from the LVFS (fwupd), kernel modules already
//! installed and a folder the user picks. The OS-specific parts live in `platform`.

/// Picks the wording for this OS: `os!("Windows text", "Linux text")`. Both stay side by
/// side so the two versions of every sentence are easy to keep in step.
#[macro_export]
macro_rules! os {
    ($windows:expr, $linux:expr $(,)?) => {
        if cfg!(target_os = "linux") { $linux } else { $windows }
    };
}

pub mod platform;
pub mod bench;
pub mod catalog;
pub mod health;
pub mod monitor;
pub mod storage;
pub mod whea;
pub mod inf;
pub mod app_update;
pub mod extras;
pub mod hwkb;

pub use platform::run_hidden;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Source {
    /// Windows Update / the distribution's packages.
    #[default]
    System,
    /// The Microsoft Update Catalog / firmware from the LVFS.
    Catalog,
    /// The Driver Store / kernel modules already installed.
    Store,
    /// A folder the user picks.
    Folder,
}

impl Source {
    pub const ALL: [Source; 4] = [Source::System, Source::Catalog, Source::Store, Source::Folder];
    pub fn name(self) -> &'static str {
        match self {
            Source::System => os!("Windows Update", "Distribution packages"),
            Source::Catalog => os!("Update Catalog", "Firmware (LVFS)"),
            Source::Store => os!("Driver Store", "Installed modules"),
            Source::Folder => "Local folder",
        }
    }
    pub fn about(self) -> &'static str {
        match self {
            Source::System => os!("Drivers Microsoft offers this PC through Windows Update.", "Driver updates from your distribution: the kernel, linux-firmware, Mesa, NVIDIA and DKMS packages."),
            Source::Catalog => os!("Searches the Microsoft Update Catalog by hardware ID for newer drivers.", "Firmware for this PC's devices, BIOS/UEFI included, from the Linux Vendor Firmware Service through fwupd."),
            Source::Store => os!("Newer driver packages already on this PC that aren't in use.", "Kernel modules already on this PC that fit a device better than the one it uses."),
            Source::Folder => os!("Driver packages (.inf) in a folder you choose, such as a vendor download.", "Firmware (.cab) and driver packages (.deb, .rpm) in a folder you choose, such as a vendor download."),
        }
    }
    fn index(self) -> usize {
        self as usize
    }
}

/// One device. On Windows `inf` is the INF file; on Linux it's the kernel module, and
/// `hwids` holds the modalias and vendor:device IDs.
#[derive(Clone, Debug, Default)]
pub struct Device {
    pub instance: String,
    pub name: String,
    pub class: String,
    pub manufacturer: String,
    pub version: String,
    pub date: String,
    pub provider: String,
    pub inf: String,
    pub signer: String,
    pub problem: u32,
    pub hwids: Vec<String>,
    pub compat: Vec<String>,
}

impl Device {
    pub fn has_driver(&self) -> bool {
        !self.version.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Update {
    pub source: Source,
    pub key: String, // WU UpdateID, catalog GUID, INF path; on Linux a package name, fwupd device ID, module or file path
    pub title: String,
    pub version: String,
    pub date: String,
    pub size: String,
    pub device: Option<String>, // instance ID of the device it's for
    pub hwid: String,
    pub vendor_swap: bool, // replaces a built-in (Windows inbox / in-kernel) driver with the hardware maker's
    pub installed: String, // version installed now, when that isn't the device driver's version (a package)
}

pub struct Data {
    pub devices: Vec<Device>,
    pub updates: Vec<Update>,
    pub log: String,
    pub busy: bool,
    pub status: String,
    pub scans: u64, // bumped after each finished scan so the UI can react to fresh results
    pub enabled: [bool; 4],
    pub source_status: [String; 4],
    pub folder: Option<String>,
    pub win11: bool,
}

impl Default for Data {
    fn default() -> Self {
        Data {
            devices: vec![],
            updates: vec![],
            log: String::new(),
            busy: false,
            status: String::new(),
            scans: 0,
            enabled: [true, true, true, true],
            source_status: Default::default(),
            folder: None,
            win11: true,
        }
    }
}

pub type Shared = Arc<Mutex<Data>>;

pub(crate) static SCANNING: AtomicBool = AtomicBool::new(false);
pub(crate) static STOP: AtomicBool = AtomicBool::new(false);

/// Asks a running scan to stop. Ignored outside scans, so an install is never cut off.
pub fn stop_scan() {
    if SCANNING.load(Ordering::SeqCst) {
        STOP.store(true, Ordering::SeqCst);
    }
}

pub fn is_scanning() -> bool {
    SCANNING.load(Ordering::SeqCst)
}

pub(crate) fn stopping() -> bool {
    SCANNING.load(Ordering::SeqCst) && STOP.load(Ordering::SeqCst)
}

/// Splits tab-separated tool output into rows of exactly `n` columns.
pub fn rows(out: &str, n: usize) -> Vec<Vec<String>> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut v: Vec<String> = l.split('\t').map(|s| s.trim().to_string()).collect();
            v.resize(n, String::new());
            v
        })
        .collect()
}

/// True when a device hardware ID is covered by a less specific ID, e.g.
/// `pci\ven_1022&dev_790b` covers `PCI\VEN_1022&DEV_790B&SUBSYS_...&REV_61`.
pub fn hw_match(device_id: &str, update_id: &str) -> bool {
    !update_id.is_empty() && device_id.to_ascii_lowercase().starts_with(&update_id.to_ascii_lowercase())
}

/// Last dotted number in a title, e.g. "... Driver Update (552.0.0.0)" -> "552.0.0.0".
pub fn version_in(title: &str) -> String {
    title
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .filter(|t| t.matches('.').count() >= 2 && t.split('.').all(|p| !p.is_empty()))
        .last()
        .unwrap_or("")
        .to_string()
}

/// What devices are for, in display order. Every device class maps to one of these.
pub const PURPOSES: [&str; 13] = [
    "Display",
    "Audio",
    "Network",
    "Bluetooth",
    "Storage",
    "Input",
    "USB & ports",
    "Cameras & imaging",
    "Printers",
    "Security",
    "Chipset & system",
    "Software & virtual",
    "Other",
];

/// Groups a device class by what the device is for.
pub fn purpose(class: &str) -> &'static str {
    platform::purpose(class)
}

/// A device's problem code, in plain words.
pub fn problem_text(code: u32) -> &'static str {
    platform::problem_text(code)
}

/// Whether a candidate driver is worth offering for a device. `Some(true)` means it is a
/// vendor driver that would replace a built-in one (Windows inbox or in-kernel); `Some(false)`
/// is a plain upgrade: a higher version, or any driver for a device without one.
/// A newer date with a lower version is a different package line, not an upgrade, so it's skipped.
pub fn upgrade(dev: &Device, date: &str, version: &str, provider: &str) -> Option<bool> {
    if !dev.has_driver() {
        return Some(false);
    }
    if platform::is_builtin(&dev.provider) && !platform::is_builtin(provider) {
        return (date > dev.date.as_str()).then_some(true);
    }
    (inf::version_parts(version) > inf::version_parts(&dev.version)).then_some(false)
}

pub fn log(d: &Shared, s: &str) {
    let mut d = d.lock().unwrap();
    for line in s.trim_end().lines() {
        d.log.push_str(line.trim_end());
        d.log.push('\n');
    }
}

pub(crate) fn status(d: &Shared, s: &str) {
    log(d, s);
    d.lock().unwrap().status = s.to_string();
}

pub(crate) fn source_status(d: &Shared, src: Source, s: String) {
    d.lock().unwrap().source_status[src.index()] = s;
}

pub(crate) fn found(n: usize) -> String {
    if n == 1 { "1 update".into() } else { format!("{n} updates") }
}

pub fn scan(d: &Shared) {
    struct Scanning;
    impl Drop for Scanning {
        fn drop(&mut self) {
            SCANNING.store(false, Ordering::SeqCst);
        }
    }
    STOP.store(false, Ordering::SeqCst);
    SCANNING.store(true, Ordering::SeqCst);
    let _scanning = Scanning;
    let stopped = || {
        let s = stopping();
        if s {
            status(d, "Scan stopped.");
        }
        s
    };

    status(d, "Rescanning hardware...");
    platform::rescan_hardware();
    if stopped() {
        return;
    }
    status(d, "Reading devices...");
    let (devices, err, win11) = platform::read_devices();
    if stopped() {
        return;
    }
    if !err.trim().is_empty() {
        log(d, &format!("Error: {}", err.trim()));
    }
    let (enabled, folder) = {
        let mut g = d.lock().unwrap();
        g.devices = devices.clone();
        g.win11 = win11;
        let on = g.enabled;
        let has_folder = g.folder.is_some();
        for (i, s) in g.source_status.iter_mut().enumerate() {
            *s = if !on[i] { "Off".into() } else if i == Source::Folder.index() && !has_folder { "No folder chosen".into() } else { "Waiting...".into() };
        }
        (g.enabled, g.folder.clone())
    };
    let problems = devices.iter().filter(|x| x.problem != 0).count();
    log(d, &format!("{} devices, {problems} with problems.", devices.len()));
    status(d, "Checking update sources...");

    let mut updates: Vec<Update> = std::thread::scope(|s| {
        let devs = &devices;
        let dir = folder.as_deref().filter(|_| enabled[Source::Folder.index()]);
        let handles: Vec<_> = Source::ALL
            .into_iter()
            .filter(|src| enabled[src.index()] && (*src != Source::Folder || dir.is_some()))
            .map(|src| s.spawn(move || platform::check_source(src, d, devs, win11, dir)))
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    if stopped() {
        return; // keep the previous update list rather than a half-checked one
    }
    if folder.is_none() {
        source_status(d, Source::Folder, "No folder chosen".into());
    }

    // Same driver from several sources: keep the most trusted one (the system's own first).
    let mut seen = std::collections::HashSet::new();
    updates.retain(|u| seen.insert((u.device.clone().unwrap_or_else(|| u.key.clone()), u.version.clone())));
    let hidden = extras::load().hidden;
    updates.retain(|u| !hidden.contains(&extras::hide_key(u)));

    let n = updates.len();
    let mut g = d.lock().unwrap();
    g.updates = updates;
    g.scans += 1;
    drop(g);
    status(d, &format!("Scan finished: {}.", found(n)));
}

/// Installs the chosen updates, each through the right channel, then rescans.
pub fn install(d: &Shared, updates: &[Update], restore_point: bool) {
    platform::install(d, updates, restore_point);
}

/// Picks a driver folder and adds its packages as an update source.
pub fn check_folder(d: &Shared) {
    let Some(dir) = platform::pick_folder(os!("Choose a folder with driver packages (.inf)", "Choose a folder with firmware (.cab) or driver packages")) else { return };
    status(d, &format!("Checking drivers in {dir}..."));
    let (devices, win11) = {
        let mut g = d.lock().unwrap();
        g.folder = Some(dir.clone());
        g.enabled[Source::Folder.index()] = true;
        g.updates.retain(|u| u.source != Source::Folder);
        (g.devices.clone(), g.win11)
    };
    let ups = platform::check_source(Source::Folder, d, &devices, win11, Some(&dir));
    let n = ups.len();
    d.lock().unwrap().updates.extend(ups);
    status(d, &format!("{} in {dir}.", found(n)));
}

/// Exports every third-party driver on this PC to a folder the user picks.
pub fn backup(d: &Shared) {
    let Some(dir) = platform::pick_folder("Choose where to save a backup of your drivers") else { return };
    status(d, &format!("Backing up drivers to {dir}..."));
    log(d, &platform::export_drivers(&dir));
    status(d, &format!("Driver backup saved to {dir}."));
}

/// Undoes one recorded driver change, then rescans.
pub fn undo_change(d: &Shared, index: usize) {
    let mut state = extras::load();
    let Some(c) = state.history.get(index).cloned() else { return };
    status(d, &format!("Rolling back {}...", c.device));
    match platform::rollback(&c) {
        Ok(out) => {
            log(d, &out);
            state.history[index].undone = true;
            extras::save(&state);
            log(d, &format!("Rolled back {} to {}.", c.device, c.old_version));
        }
        Err(e) => log(d, &format!("Couldn't roll back {}: {}", c.device, e.trim())),
    }
    scan(d);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_matches() {
        let r = rows("A\tB\r\n\nC\n", 3);
        assert_eq!(r, vec![vec!["A", "B", ""], vec!["C", "", ""]]);
        assert!(hw_match("PCI\\VEN_8086&DEV_1234&SUBSYS_1&REV_01", "pci\\ven_8086&dev_1234"));
        assert!(!hw_match("PCI\\VEN_8086&DEV_1234", "pci\\ven_8086&dev_9999"));
        assert!(!hw_match("PCI\\VEN_8086&DEV_1234", ""));
    }

    #[test]
    fn versions_and_upgrades() {
        assert_eq!(version_in("Apple, Inc. USBDevice Driver Update (552.0.0.0)"), "552.0.0.0");
        assert_eq!(version_in("AMD - System - 8/30/2017 12:00:00 AM - 5.12.0.38"), "5.12.0.38");
        assert_eq!(version_in("No version here"), "");
        let dev = Device { provider: "AMD".into(), version: "5.12.0.31".into(), date: "2020-01-01".into(), ..Default::default() };
        assert_eq!(upgrade(&dev, "2021-01-01", "5.13.0.0", "AMD"), Some(false));
        assert_eq!(upgrade(&dev, "2026-04-06", "3.0.1.1338", "AMD"), None); // newer date, lower version
        assert_eq!(upgrade(&dev, "2020-01-01", "5.12.0.31", "AMD"), None);
        assert_eq!(upgrade(&Device::default(), "2000-01-01", "1.0", "x"), Some(false));
        assert!(PURPOSES.contains(&purpose("")));
        assert_eq!(Source::ALL.map(|s| s.index()), [0, 1, 2, 3]);
    }
}
