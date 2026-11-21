//! Device scanning and driver updates from four sources: Windows Update, the
//! Microsoft Update Catalog, the local Driver Store and a folder the user picks.
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

use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const NO_WINDOW: u32 = 0x0800_0000; // CREATE_NO_WINDOW: no console flashes from the GUI
const DRIVER_STORE: &str = r"C:\Windows\System32\DriverStore\FileRepository";

// Every present device that has a driver or a problem, joined with its driver details.
const DEVICES: &str = r#"
"OS`t" + [Environment]::OSVersion.Version.Build
$drv = @{}
Get-CimInstance Win32_PnPSignedDriver | ForEach-Object { if ($_.DeviceID) { $drv[$_.DeviceID] = $_ } }
Get-CimInstance Win32_PnPEntity | ForEach-Object {
  $d = $drv[$_.DeviceID]
  if (-not $d.DriverVersion -and $_.ConfigManagerErrorCode -eq 0) { return }
  $name = if ($_.Name) { $_.Name } else { $_.DeviceID }
  $date = if ($d.DriverDate) { $d.DriverDate.ToString('yyyy-MM-dd') } else { '' }
  ($_.DeviceID, $name, $_.PNPClass, $_.Manufacturer, $d.DriverVersion, $date, $d.DriverProviderName, $d.InfName, $d.Signer,
   $_.ConfigManagerErrorCode, ($_.HardwareID -join '|'), ($_.CompatibleID -join '|')) -join "`t"
}"#;

const WU_SEARCH: &str = r#"
$s = New-Object -ComObject Microsoft.Update.Session
$q = $s.CreateUpdateSearcher()
$q.ServerSelection = 2
$r = $q.Search("IsInstalled=0 and Type='Driver' and IsHidden=0")
"#;

const WU_LIST: &str = r#"
foreach ($u in $r.Updates) {
  "{0}`t{1}`t{2}`t{3}`t{4}" -f $u.Title, $u.DriverVerDate.ToString('yyyy-MM-dd'), $u.DriverHardwareID, $u.Identity.UpdateID, $u.MaxDownloadSize
}"#;

// Expects `$ids` (the UpdateIDs to install) to be defined before it.
const WU_INSTALL: &str = r#"
$c = New-Object -ComObject Microsoft.Update.UpdateColl
foreach ($u in $r.Updates) {
  if ($ids -notcontains $u.Identity.UpdateID) { continue }
  if (-not $u.EulaAccepted) { $u.AcceptEula() }; [void]$c.Add($u)
}
if ($c.Count -eq 0) { 'Nothing to install.'; exit }
"Downloading $($c.Count) driver(s) from Windows Update..."
$d = $s.CreateUpdateDownloader(); $d.Updates = $c; [void]$d.Download()
$i = $s.CreateUpdateInstaller(); $i.Updates = $c; $res = $i.Install()
$codes = 'NotStarted','InProgress','Succeeded','SucceededWithErrors','Failed','Aborted'
for ($k = 0; $k -lt $c.Count; $k++) { '  [{0}] {1}' -f $codes[$res.GetUpdateResult($k).ResultCode], $c.Item($k).Title }
if ($res.RebootRequired) { 'A reboot is required to finish installing.' }
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    WindowsUpdate,
    Catalog,
    DriverStore,
    Folder,
}

impl Source {
    pub const ALL: [Source; 4] = [Source::WindowsUpdate, Source::Catalog, Source::DriverStore, Source::Folder];
    pub fn name(self) -> &'static str {
        match self {
            Source::WindowsUpdate => "Windows Update",
            Source::Catalog => "Update Catalog",
            Source::DriverStore => "Driver Store",
            Source::Folder => "Local folder",
        }
    }
    pub fn about(self) -> &'static str {
        match self {
            Source::WindowsUpdate => "Drivers Microsoft offers this PC through Windows Update.",
            Source::Catalog => "Searches the Microsoft Update Catalog by hardware ID for newer drivers.",
            Source::DriverStore => "Newer driver packages already on this PC that aren't in use.",
            Source::Folder => "Driver packages (.inf) in a folder you choose, such as a vendor download.",
        }
    }
    fn index(self) -> usize {
        self as usize
    }
}

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

#[derive(Clone, Debug)]
pub struct Update {
    pub source: Source,
    pub key: String, // WU UpdateID, catalog GUID, or INF path
    pub title: String,
    pub version: String,
    pub date: String,
    pub size: String,
    pub device: Option<String>, // instance ID of the device it's for
    pub hwid: String,
    pub vendor_swap: bool, // replaces a Windows built-in driver with the hardware maker's
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

/// Runs a program without a console window; returns stdout and stderr combined.
pub fn run_hidden(program: &str, args: &[&str]) -> String {
    match Command::new(program).args(args).creation_flags(NO_WINDOW).output() {
        Ok(o) => format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
        Err(e) => format!("cannot start {program}: {e}"),
    }
}

static SCANNING: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);

/// Asks a running scan to stop. Ignored outside scans, so an install is never cut off.
pub fn stop_scan() {
    if SCANNING.load(Ordering::SeqCst) {
        STOP.store(true, Ordering::SeqCst);
    }
}

pub fn is_scanning() -> bool {
    SCANNING.load(Ordering::SeqCst)
}

fn stopping() -> bool {
    SCANNING.load(Ordering::SeqCst) && STOP.load(Ordering::SeqCst)
}

/// Runs a PowerShell script from a temp file (avoids command-line quoting issues).
/// Returns (stdout, stderr). During a scan, a stop request kills the script at once.
pub fn ps(script: &str) -> (String, String) {
    let path = std::env::temp_dir().join(format!("flash-{}-{:?}.ps1", std::process::id(), std::thread::current().id()));
    if let Err(e) = std::fs::write(&path, format!("[Console]::OutputEncoding=[Text.Encoding]::UTF8\n{script}")) {
        return (String::new(), format!("cannot write temp script: {e}"));
    }
    let child = Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(NO_WINDOW)
        .spawn();
    let out = child.and_then(|child| {
        let pid = child.id().to_string();
        // Read output on another thread so a stop can kill the process meanwhile.
        let waiter = std::thread::spawn(move || child.wait_with_output());
        let mut killed = false;
        while !waiter.is_finished() {
            if !killed && stopping() {
                run_hidden("taskkill", &["/PID", &pid, "/T", "/F"]);
                killed = true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        waiter.join().unwrap_or_else(|_| Err(std::io::Error::other("PowerShell reader panicked")))
    });
    std::fs::remove_file(&path).ok();
    match out {
        Ok(o) => (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned()),
        Err(e) => (String::new(), format!("cannot start PowerShell: {e}")),
    }
}

/// Splits tab-separated PowerShell output into rows of exactly `n` columns.
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

/// True when a device hardware ID is covered by a WU driver ID. WU IDs are less specific,
/// e.g. `pci\ven_1022&dev_790b` covers `PCI\VEN_1022&DEV_790B&SUBSYS_...&REV_61`.
pub fn hw_match(device_id: &str, update_id: &str) -> bool {
    !update_id.is_empty() && device_id.to_ascii_lowercase().starts_with(&update_id.to_ascii_lowercase())
}

/// PowerShell array literal of update IDs. Only GUID characters are allowed, so
/// nothing can break out of the quotes.
pub fn ps_ids(ids: &[String]) -> String {
    let ok: Vec<String> = ids
        .iter()
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
        .map(|id| format!("'{id}'"))
        .collect();
    format!("@({})", ok.join(","))
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

/// What devices are for, in display order. Every Windows device class maps to one of these.
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

/// Groups a Windows device class (PNPClass) by what the device is for.
pub fn purpose(class: &str) -> &'static str {
    match class.to_ascii_lowercase().as_str() {
        "display" | "monitor" => "Display",
        "media" | "audioendpoint" | "audioprocessingobject" => "Audio",
        "net" | "netservice" | "nettrans" | "netclient" => "Network",
        "bluetooth" | "bthledevice" => "Bluetooth",
        "diskdrive" | "hdc" | "scsiadapter" | "volume" | "volumesnapshot" | "cdrom" | "sdhost" | "floppydisk" | "storagevolume" => "Storage",
        "keyboard" | "mouse" | "hidclass" => "Input",
        "usb" | "usbdevice" | "ports" => "USB & ports",
        "camera" | "image" => "Cameras & imaging",
        "printer" | "printqueue" => "Printers",
        "securitydevices" | "biometric" | "smartcardreader" | "smartcard" => "Security",
        "system" | "processor" | "computer" | "firmware" | "battery" | "sensor" => "Chipset & system",
        "softwaredevice" | "softwarecomponent" | "extension" | "legacydriver" => "Software & virtual",
        _ => "Other",
    }
}

/// Device Manager problem codes, in plain words.
pub fn problem_text(code: u32) -> &'static str {
    match code {
        0 => "Working",
        1 => "Not configured correctly",
        3 => "Driver may be corrupted",
        10 => "Device cannot start",
        12 => "Not enough free resources",
        14 => "Restart required",
        18 => "Drivers need reinstalling",
        19 => "Registry configuration damaged",
        21 => "Being removed",
        22 => "Disabled",
        24 => "Not present or not working",
        28 => "No driver installed",
        31 => "Driver could not load",
        37 => "Driver failed to initialize",
        39 => "Driver corrupted or missing",
        43 => "Stopped after reporting problems",
        52 => "Driver signature could not be verified",
        _ => "Problem reported",
    }
}

pub fn parse_devices(out: &str) -> (Vec<Device>, u32) {
    let mut build = 0;
    let mut devices = Vec::new();
    for r in rows(out, 12) {
        if r[0] == "OS" {
            build = r[1].parse().unwrap_or(0);
            continue;
        }
        let list = |s: &str| s.split('|').filter(|x| !x.is_empty()).map(String::from).collect();
        devices.push(Device {
            instance: r[0].clone(),
            name: r[1].clone(),
            class: r[2].clone(),
            manufacturer: r[3].clone(),
            version: r[4].clone(),
            date: r[5].clone(),
            provider: r[6].clone(),
            inf: r[7].clone(),
            signer: r[8].clone(),
            problem: r[9].parse().unwrap_or(0),
            hwids: list(&r[10]),
            compat: list(&r[11]),
        });
    }
    devices.sort_by(|a, b| a.class.to_lowercase().cmp(&b.class.to_lowercase()).then_with(|| a.name.cmp(&b.name)));
    (devices, build)
}

fn is_microsoft(s: &str) -> bool {
    s.to_ascii_lowercase().contains("microsoft")
}

/// Whether a candidate driver is worth offering for a device. `Some(true)` means it is a
/// vendor driver that would replace a Windows built-in one (newer date, different numbering);
/// `Some(false)` is a plain upgrade: a higher version, or any driver for a device without one.
/// A newer date with a lower version is a different package line, not an upgrade, so it's skipped.
pub fn upgrade(dev: &Device, date: &str, version: &str, provider: &str) -> Option<bool> {
    if !dev.has_driver() {
        return Some(false);
    }
    if is_microsoft(&dev.provider) && !is_microsoft(provider) {
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

fn status(d: &Shared, s: &str) {
    log(d, s);
    d.lock().unwrap().status = s.to_string();
}

fn source_status(d: &Shared, src: Source, s: String) {
    d.lock().unwrap().source_status[src.index()] = s;
}

fn found(n: usize) -> String {
    if n == 1 { "1 update".into() } else { format!("{n} updates") }
}

fn wu_updates(d: &Shared, devices: &[Device]) -> Vec<Update> {
    source_status(d, Source::WindowsUpdate, "Checking...".into());
    let (out, err) = ps(&format!("{WU_SEARCH}{WU_LIST}"));
    if !err.trim().is_empty() {
        log(d, &format!("Windows Update: {}", err.trim()));
        source_status(d, Source::WindowsUpdate, "Couldn't check".into());
        return vec![];
    }
    let ups: Vec<Update> = rows(&out, 5)
        .into_iter()
        .map(|u| {
            let dev = devices.iter().find(|x| x.hwids.iter().chain(&x.compat).any(|h| h.eq_ignore_ascii_case(&u[2]))).or_else(|| devices.iter().find(|x| x.hwids.first().is_some_and(|h| hw_match(h, &u[2]))));
            let bytes: f64 = u[4].parse().unwrap_or(0.0);
            let vendor_swap = dev.is_some_and(|x| is_microsoft(&x.provider) && !is_microsoft(&u[0]));
            Update {
                source: Source::WindowsUpdate,
                key: u[3].clone(),
                version: version_in(&u[0]),
                title: u[0].clone(),
                date: u[1].clone(),
                size: if bytes > 0.0 { format!("{:.1} MB", bytes / 1_048_576.0) } else { String::new() },
                device: dev.map(|x| x.instance.clone()),
                hwid: u[2].clone(),
                vendor_swap,
            }
        })
        .collect();
    source_status(d, Source::WindowsUpdate, found(ups.len()));
    ups
}

fn catalog_updates(d: &Shared, devices: &[Device], win11: bool) -> Vec<Update> {
    let mut groups: HashMap<String, Vec<&Device>> = HashMap::new();
    for dev in devices {
        if let Some(q) = catalog::query_id(&dev.hwids) {
            groups.entry(q).or_default().push(dev);
        }
    }
    let queries: Vec<String> = groups.keys().cloned().collect();
    source_status(d, Source::Catalog, format!("Checking {} devices...", queries.len()));
    let agent = catalog::agent();
    // A few requests at a time keeps it quick without hammering the catalog.
    let results: Vec<(String, Result<Vec<catalog::Entry>, String>)> = std::thread::scope(|s| {
        let handles: Vec<_> = queries
            .chunks(queries.len().div_ceil(6).max(1))
            .map(|chunk| {
                let agent = &agent;
                s.spawn(move || chunk.iter().take_while(|_| !stopping()).map(|q| (q.clone(), catalog::search(agent, q))).collect::<Vec<_>>())
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });

    let failed = results.iter().filter(|(_, r)| r.is_err()).count();
    if let Some((_, Err(e))) = results.iter().find(|(_, r)| r.is_err()) {
        log(d, &format!("Update Catalog: {failed} search(es) failed, e.g. {e}"));
    }
    let mut ups = Vec::new();
    for (q, res) in results {
        let Ok(entries) = res else { continue };
        for dev in &groups[&q] {
            let best = entries
                .iter()
                .filter(|e| catalog::fits_os(e, win11) && e.version.starts_with(|c: char| c.is_ascii_digit()))
                .filter_map(|e| upgrade(dev, &e.date, &e.version, &e.title).map(|swap| (e, swap)))
                .max_by(|a, b| inf::cmp_driver(&a.0.date, &a.0.version, &b.0.date, &b.0.version));
            if let Some((e, vendor_swap)) = best {
                ups.push(Update {
                    source: Source::Catalog,
                    key: e.guid.clone(),
                    title: e.title.clone(),
                    version: e.version.clone(),
                    date: e.date.clone(),
                    size: e.size.clone(),
                    device: Some(dev.instance.clone()),
                    hwid: q.clone(),
                    vendor_swap,
                });
            }
        }
    }
    let msg = if failed > 0 && failed == queries.len() { "Couldn't reach the catalog".into() } else { found(ups.len()) };
    source_status(d, Source::Catalog, msg);
    ups
}

fn inf_updates(d: &Shared, devices: &[Device], dir: &str, src: Source) -> Vec<Update> {
    source_status(d, src, "Reading driver packages...".into());
    let infs = inf::scan_dir(Path::new(dir));
    let mut by_id: HashMap<String, Vec<&inf::InfDriver>> = HashMap::new();
    // Microsoft's own packages are built-in drivers that Windows services itself.
    for p in infs.iter().filter(|p| !is_microsoft(&p.provider)) {
        for id in &p.ids {
            by_id.entry(id.to_ascii_lowercase()).or_default().push(p);
        }
    }
    let mut ups = Vec::new();
    for dev in devices {
        // Hardware IDs only: compatible-ID matches are generic drivers Windows ranks lower.
        let best = dev
            .hwids
            .iter()
            .filter_map(|h| by_id.get(&h.to_ascii_lowercase()))
            .flatten()
            .filter_map(|p| upgrade(dev, &p.date, &p.version, &p.provider).map(|swap| (*p, swap)))
            .max_by(|a, b| inf::cmp_driver(&a.0.date, &a.0.version, &b.0.date, &b.0.version));
        if let Some((p, vendor_swap)) = best {
            let file = p.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            let provider = if p.provider.is_empty() { "Driver package" } else { &p.provider };
            ups.push(Update {
                source: src,
                key: p.path.to_string_lossy().into_owned(),
                title: format!("{provider} ({file})"),
                version: p.version.clone(),
                date: p.date.clone(),
                size: String::new(),
                device: Some(dev.instance.clone()),
                hwid: dev.hwids.first().cloned().unwrap_or_default(),
                vendor_swap,
            });
        }
    }
    source_status(d, src, format!("{} from {} packages", found(ups.len()), infs.len()));
    ups
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
    run_hidden("pnputil", &["/scan-devices"]);
    if stopped() {
        return;
    }
    status(d, "Reading devices...");
    let (out, err) = ps(DEVICES);
    if stopped() {
        return;
    }
    if !err.trim().is_empty() {
        log(d, &format!("Error: {}", err.trim()));
    }
    let (devices, build) = parse_devices(&out);
    let win11 = build == 0 || build >= 22000;
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
        let wu = enabled[0].then(|| s.spawn(|| wu_updates(d, devs)));
        let cat = enabled[1].then(|| s.spawn(|| catalog_updates(d, devs, win11)));
        let store = enabled[2].then(|| s.spawn(|| inf_updates(d, devs, DRIVER_STORE, Source::DriverStore)));
        let dir = folder.as_deref().filter(|_| enabled[3]);
        let local = dir.map(|f| s.spawn(move || inf_updates(d, devs, f, Source::Folder)));
        [wu, cat, store, local].into_iter().flatten().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    if stopped() {
        return; // keep the previous update list rather than a half-checked one
    }
    if folder.is_none() {
        source_status(d, Source::Folder, "No folder chosen".into());
    }

    // Same driver from several sources: keep the most trusted one (WU first, as pushed).
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
    if restore_point {
        status(d, "Creating a restore point...");
        let (out, err) = ps("Checkpoint-Computer -Description 'Flash driver update' -RestorePointType MODIFY_SETTINGS");
        log(d, &out);
        if err.trim().is_empty() {
            log(d, "Restore point created.");
        } else {
            log(d, &format!("Restore point not created: {}", err.trim()));
        }
    }
    let before: Vec<Device> = d.lock().unwrap().devices.clone();
    // Save each target device's current driver first, so the change can be undone later.
    let stamp = extras::now();
    let backups: std::collections::HashMap<String, String> = updates
        .iter()
        .filter_map(|u| before.iter().find(|x| Some(&x.instance) == u.device.as_ref()))
        .filter_map(|dev| extras::backup_driver(&dev.inf, stamp).map(|b| (dev.instance.clone(), b)))
        .collect();

    let wu: Vec<String> = updates.iter().filter(|u| u.source == Source::WindowsUpdate).map(|u| u.key.clone()).collect();
    if !wu.is_empty() {
        status(d, &format!("Installing {} from Windows Update...", found(wu.len())));
        let (out, err) = ps(&format!("{WU_SEARCH}$ids = {}\n{WU_INSTALL}", ps_ids(&wu)));
        log(d, &out);
        if !err.trim().is_empty() {
            log(d, &format!("Error: {}", err.trim()));
        }
    }

    let agent = catalog::agent();
    let work = std::env::temp_dir().join("flash-catalog");
    // The same package can be listed for several devices; install it once.
    let mut done = std::collections::HashSet::new();
    for u in updates.iter().filter(|u| u.source == Source::Catalog && done.insert(u.key.clone())) {
        status(d, &format!("Downloading {}...", u.title));
        match catalog::fetch(&agent, &u.key, &work) {
            Ok(dir) => {
                // Catalog searches match the chip, not the PC model, so a package may be built for
                // another maker's machines. Only add INFs that list one of the target devices' exact IDs.
                let targets: Vec<String> = updates
                    .iter()
                    .filter(|x| x.key == u.key)
                    .filter_map(|x| before.iter().find(|dev| Some(&dev.instance) == x.device.as_ref()))
                    .flat_map(|dev| dev.hwids.iter().map(|h| h.to_ascii_lowercase()))
                    .collect();
                let infs = inf::scan_dir(&dir);
                let fitting: Vec<&inf::InfDriver> = infs.iter().filter(|p| p.ids.iter().any(|id| targets.contains(&id.to_ascii_lowercase()))).collect();
                if fitting.is_empty() {
                    log(d, &format!("Skipped {}: none of its {} driver file(s) list this device's hardware IDs, so it's built for a different PC model. Nothing was added.", u.title, infs.len()));
                    continue;
                }
                status(d, &format!("Installing {}...", u.title));
                for p in fitting {
                    log(d, &run_hidden("pnputil", &["/add-driver", &p.path.to_string_lossy(), "/install"]));
                }
            }
            Err(e) => log(d, &format!("Couldn't download {}: {e}", u.title)),
        }
    }
    std::fs::remove_dir_all(&work).ok();

    for u in updates.iter().filter(|u| matches!(u.source, Source::DriverStore | Source::Folder) && done.insert(u.key.clone())) {
        status(d, &format!("Installing {}...", u.title));
        log(d, &run_hidden("pnputil", &["/add-driver", &u.key, "/install"]));
    }
    scan(d);

    // Report what actually changed: pnputil can succeed without switching the device's driver.
    let after = d.lock().unwrap().devices.clone();
    let mut state = extras::load();
    for u in updates {
        let Some(id) = &u.device else { continue };
        let (Some(old), Some(new)) = (before.iter().find(|x| &x.instance == id), after.iter().find(|x| &x.instance == id)) else { continue };
        if new.version != old.version {
            log(d, &format!("Updated {}: {} -> {}", new.name, old.version, new.version));
            state.history.push(extras::Change {
                time: stamp,
                device: new.name.clone(),
                instance: id.clone(),
                old_version: old.version.clone(),
                new_version: new.version.clone(),
                old_inf: old.inf.clone(),
                new_inf: new.inf.clone(),
                backup: backups.get(id).cloned().unwrap_or_default(),
                undone: false,
            });
        } else {
            log(d, &format!("Not changed: {} is still on {}. Windows kept the current driver as the better match.", new.name, new.version));
        }
    }
    extras::save(&state);
}

/// Asks for a folder with the Windows folder picker. Titles are fixed strings, never user input.
pub fn pick_folder(title: &'static str) -> Option<String> {
    let (out, _) = ps(&format!("$f = (New-Object -ComObject Shell.Application).BrowseForFolder(0, '{title}', 0x51, 0)\nif ($f) {{ $f.Self.Path }}"));
    let path = out.trim().to_string();
    (!path.is_empty() && Path::new(&path).is_dir()).then_some(path)
}

/// Picks a driver folder and adds its packages as an update source.
pub fn check_folder(d: &Shared) {
    let Some(dir) = pick_folder("Choose a folder with driver packages (.inf)") else { return };
    status(d, &format!("Checking drivers in {dir}..."));
    let devices = {
        let mut g = d.lock().unwrap();
        g.folder = Some(dir.clone());
        g.enabled[Source::Folder.index()] = true;
        g.updates.retain(|u| u.source != Source::Folder);
        g.devices.clone()
    };
    let ups = inf_updates(d, &devices, &dir, Source::Folder);
    let n = ups.len();
    d.lock().unwrap().updates.extend(ups);
    status(d, &format!("{} in {dir}.", found(n)));
}

/// Exports every third-party driver on this PC to a folder the user picks.
pub fn backup(d: &Shared) {
    let Some(dir) = pick_folder("Choose where to save a backup of your drivers") else { return };
    status(d, &format!("Backing up drivers to {dir}..."));
    log(d, &run_hidden("pnputil", &["/export-driver", "*", &dir]));
    status(d, &format!("Driver backup saved to {dir}."));
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
        let ids = ["0a1b-2C".to_string(), "x'; rm".to_string(), String::new()];
        assert_eq!(ps_ids(&ids), "@('0a1b-2C')");
    }

    #[test]
    fn stop_kills_a_running_scan_query_only_during_scans() {
        stop_scan(); // not scanning: must be ignored
        assert!(!STOP.load(Ordering::SeqCst));
        SCANNING.store(true, Ordering::SeqCst);
        let stopper = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(800));
            stop_scan();
        });
        let start = std::time::Instant::now();
        let (out, _) = ps("Start-Sleep -Seconds 30; 'finished'");
        let took = start.elapsed();
        stopper.join().unwrap();
        SCANNING.store(false, Ordering::SeqCst);
        STOP.store(false, Ordering::SeqCst);
        assert!(!out.contains("finished"));
        assert!(took < std::time::Duration::from_secs(10), "stop took {took:?}");
    }

    #[test]
    fn purposes() {
        assert_eq!(purpose("AudioEndpoint"), "Audio");
        assert_eq!(purpose("HIDClass"), "Input");
        assert_eq!(purpose("SecurityDevices"), "Security");
        assert_eq!(purpose(""), "Other");
        assert!(["Display", "Net", "System", "PrintQueue", "SoftwareDevice"].iter().all(|c| PURPOSES.contains(&purpose(c))));
    }

    #[test]
    fn versions_and_devices() {
        assert_eq!(version_in("Apple, Inc. USBDevice Driver Update (552.0.0.0)"), "552.0.0.0");
        assert_eq!(version_in("AMD - System - 8/30/2017 12:00:00 AM - 5.12.0.38"), "5.12.0.38");
        assert_eq!(version_in("No version here"), "");
        let out = "OS\t26200\nPCI\\X\tSMBus\tSystem\tAMD\t5.12.0.31\t2020-01-01\tAMD\toem1.inf\tMS\t0\tPCI\\VEN_1&DEV_2|PCI\\VEN_1\tPCI\\CC_0C05\n";
        let (devs, build) = parse_devices(out);
        assert_eq!(build, 26200);
        assert_eq!(devs[0].hwids, vec!["PCI\\VEN_1&DEV_2", "PCI\\VEN_1"]);
        assert_eq!(upgrade(&devs[0], "2021-01-01", "5.13.0.0", "AMD"), Some(false));
        assert_eq!(upgrade(&devs[0], "2026-04-06", "3.0.1.1338", "AMD"), None); // newer date, lower version
        assert_eq!(upgrade(&devs[0], "2020-01-01", "5.12.0.31", "AMD"), None);
        let inbox = Device { provider: "Microsoft".into(), version: "10.0.26100.1".into(), date: "2006-06-21".into(), ..Default::default() };
        assert_eq!(upgrade(&inbox, "2026-02-26", "1.1.84.7627", "Logitech USB Driver Update"), Some(true));
        assert_eq!(upgrade(&Device::default(), "2000-01-01", "1.0", "x"), Some(false));
    }
}

/// Undoes one recorded driver change, then rescans.
pub fn undo_change(d: &Shared, index: usize) {
    let mut state = extras::load();
    let Some(c) = state.history.get(index).cloned() else { return };
    status(d, &format!("Rolling back {}...", c.device));
    match extras::rollback(&c) {
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
