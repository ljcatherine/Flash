//! Windows: WMI and PowerShell for devices and health, Windows Update and the Microsoft
//! Update Catalog for drivers, pnputil to install them, Task Scheduler, the registry and
//! Win32 for everything else. Flash runs elevated here (UAC manifest, see build.rs).
use super::{InstallKind, Tool};
use crate::extras::{self, Change, StartupApp};
use crate::health::{self, Report};
use crate::monitor::Monitor;
use crate::storage::{Junk, Kind, delete_contents, dir_size, safe_target};
use crate::whea::{Event, parse_message};
use crate::{Device, Shared, Source, Update, catalog, inf, log, rows, upgrade};
use base64::Engine;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::BufRead;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::{
    System::{
        Com::CoTaskMemFree,
        Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW},
        SystemInformation::GetSystemDirectoryW,
    },
    UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath},
};
use windows_sys::w;

pub(crate) const NO_WINDOW: u32 = 0x0800_0000; // CREATE_NO_WINDOW: no console flashes from the GUI
const DRIVER_STORE: &str = r"C:\Windows\System32\DriverStore\FileRepository";

/// Runs a program without a console window; returns stdout and stderr combined.
pub fn run_hidden(program: &str, args: &[&str]) -> String {
    match Command::new(program).args(args).creation_flags(NO_WINDOW).output() {
        Ok(o) => format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
        Err(e) => format!("cannot start {program}: {e}"),
    }
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
            if !killed && crate::stopping() {
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

// ---------------- Devices ----------------

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

/// Asks Windows to look for new or changed hardware.
pub fn rescan_hardware() {
    run_hidden("pnputil", &["/scan-devices"]);
}

/// Every device with a driver or a problem: (devices, error text, is Windows 11).
pub fn read_devices() -> (Vec<Device>, String, bool) {
    let (out, err) = ps(DEVICES);
    let (devices, build) = parse_devices(&out);
    (devices, err, build == 0 || build >= 22000)
}

/// Windows' own drivers come from Microsoft.
pub fn is_builtin(provider: &str) -> bool {
    provider.to_ascii_lowercase().contains("microsoft")
}

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

/// What to do about a problem when no source found a driver for it.
pub fn problem_fix(code: u32) -> &'static str {
    match code {
        22 => "Enable it in Device Manager",
        14 => "Restart the PC",
        _ => "No driver found. Try a vendor folder or Device Manager",
    }
}

// ---------------- Update sources ----------------

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

/// Checks one update source. `folder` is the chosen folder for `Source::Folder`.
pub fn check_source(src: Source, d: &Shared, devices: &[Device], win11: bool, folder: Option<&str>) -> Vec<Update> {
    match src {
        Source::System => wu_updates(d, devices),
        Source::Catalog => catalog_updates(d, devices, win11),
        Source::Store => inf_updates(d, devices, DRIVER_STORE, Source::Store),
        Source::Folder => folder.map(|f| inf_updates(d, devices, f, Source::Folder)).unwrap_or_default(),
    }
}

fn wu_updates(d: &Shared, devices: &[Device]) -> Vec<Update> {
    crate::source_status(d, Source::System, "Checking...".into());
    let (out, err) = ps(&format!("{WU_SEARCH}{WU_LIST}"));
    if !err.trim().is_empty() {
        log(d, &format!("Windows Update: {}", err.trim()));
        crate::source_status(d, Source::System, "Couldn't check".into());
        return vec![];
    }
    let ups: Vec<Update> = rows(&out, 5)
        .into_iter()
        .map(|u| {
            let dev = devices.iter().find(|x| x.hwids.iter().chain(&x.compat).any(|h| h.eq_ignore_ascii_case(&u[2]))).or_else(|| devices.iter().find(|x| x.hwids.first().is_some_and(|h| crate::hw_match(h, &u[2]))));
            let bytes: f64 = u[4].parse().unwrap_or(0.0);
            let vendor_swap = dev.is_some_and(|x| is_builtin(&x.provider) && !is_builtin(&u[0]));
            Update {
                source: Source::System,
                key: u[3].clone(),
                version: crate::version_in(&u[0]),
                title: u[0].clone(),
                date: u[1].clone(),
                size: if bytes > 0.0 { format!("{:.1} MB", bytes / 1_048_576.0) } else { String::new() },
                device: dev.map(|x| x.instance.clone()),
                hwid: u[2].clone(),
                vendor_swap,
                installed: String::new(),
            }
        })
        .collect();
    crate::source_status(d, Source::System, crate::found(ups.len()));
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
    crate::source_status(d, Source::Catalog, format!("Checking {} devices...", queries.len()));
    let agent = catalog::agent();
    // A few requests at a time keeps it quick without hammering the catalog.
    let results: Vec<(String, Result<Vec<catalog::Entry>, String>)> = std::thread::scope(|s| {
        let handles: Vec<_> = queries
            .chunks(queries.len().div_ceil(6).max(1))
            .map(|chunk| {
                let agent = &agent;
                s.spawn(move || chunk.iter().take_while(|_| !crate::stopping()).map(|q| (q.clone(), catalog::search(agent, q))).collect::<Vec<_>>())
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
                    installed: String::new(),
                });
            }
        }
    }
    let msg = if failed > 0 && failed == queries.len() { "Couldn't reach the catalog".into() } else { crate::found(ups.len()) };
    crate::source_status(d, Source::Catalog, msg);
    ups
}

fn inf_updates(d: &Shared, devices: &[Device], dir: &str, src: Source) -> Vec<Update> {
    crate::source_status(d, src, "Reading driver packages...".into());
    let infs = inf::scan_dir(Path::new(dir));
    let mut by_id: HashMap<String, Vec<&inf::InfDriver>> = HashMap::new();
    // Microsoft's own packages are built-in drivers that Windows services itself.
    for p in infs.iter().filter(|p| !is_builtin(&p.provider)) {
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
                installed: String::new(),
            });
        }
    }
    crate::source_status(d, src, format!("{} from {} packages", crate::found(ups.len()), infs.len()));
    ups
}

// ---------------- Install ----------------

/// A restore point can always be requested on Windows; if System Protection is off,
/// the Activity log says so.
pub fn snapshot_status() -> Result<(), String> {
    Ok(())
}

/// Something the install dialog must say about these updates (nothing on Windows).
pub fn install_note(_updates: &[Update]) -> Option<&'static str> {
    None
}

/// Installs the chosen updates, each through the right channel, then rescans.
pub fn install(d: &Shared, updates: &[Update], restore_point: bool) {
    if restore_point {
        crate::status(d, "Creating a restore point...");
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
    let backups: HashMap<String, String> = updates
        .iter()
        .filter_map(|u| before.iter().find(|x| Some(&x.instance) == u.device.as_ref()))
        .filter_map(|dev| backup_driver(&dev.inf, stamp).map(|b| (dev.instance.clone(), b)))
        .collect();

    let wu: Vec<String> = updates.iter().filter(|u| u.source == Source::System).map(|u| u.key.clone()).collect();
    if !wu.is_empty() {
        crate::status(d, &format!("Installing {} from Windows Update...", crate::found(wu.len())));
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
        crate::status(d, &format!("Downloading {}...", u.title));
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
                crate::status(d, &format!("Installing {}...", u.title));
                for p in fitting {
                    log(d, &run_hidden("pnputil", &["/add-driver", &p.path.to_string_lossy(), "/install"]));
                }
            }
            Err(e) => log(d, &format!("Couldn't download {}: {e}", u.title)),
        }
    }
    std::fs::remove_dir_all(&work).ok();

    for u in updates.iter().filter(|u| matches!(u.source, Source::Store | Source::Folder) && done.insert(u.key.clone())) {
        crate::status(d, &format!("Installing {}...", u.title));
        log(d, &run_hidden("pnputil", &["/add-driver", &u.key, "/install"]));
    }
    crate::scan(d);

    // Report what actually changed: pnputil can succeed without switching the device's driver.
    let after = d.lock().unwrap().devices.clone();
    let mut state = extras::load();
    for u in updates {
        let Some(id) = &u.device else { continue };
        let (Some(old), Some(new)) = (before.iter().find(|x| &x.instance == id), after.iter().find(|x| &x.instance == id)) else { continue };
        if new.version != old.version {
            log(d, &format!("Updated {}: {} -> {}", new.name, old.version, new.version));
            state.history.push(Change {
                time: stamp,
                device: new.name.clone(),
                instance: id.clone(),
                old_version: old.version.clone(),
                new_version: new.version.clone(),
                old_inf: old.inf.clone(),
                new_inf: new.inf.clone(),
                backup: backups.get(id).cloned().unwrap_or_default(),
                undone: false,
                undo: String::new(),
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

/// Exports every third-party driver package to `dir`.
pub fn export_drivers(dir: &str) -> String {
    run_hidden("pnputil", &["/export-driver", "*", dir])
}

// ---------------- Saved state, schedule, rollback, startup apps ----------------

pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Flash")
}

const TASK: &str = r"Flash\Weekly scan";

pub fn schedule_enabled() -> bool {
    Command::new("schtasks").args(["/Query", "/TN", TASK]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Turns the weekly background scan (Sundays at noon) on or off in Task Scheduler.
pub fn set_schedule(on: bool) -> Result<(), String> {
    let out = if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let action = format!("\"{}\" --scheduled", exe.display());
        run_hidden("schtasks", &["/Create", "/F", "/SC", "WEEKLY", "/D", "SUN", "/ST", "12:00", "/RL", "HIGHEST", "/TN", TASK, "/TR", &action])
    } else {
        run_hidden("schtasks", &["/Delete", "/F", "/TN", TASK])
    };
    if out.contains("SUCCESS") { Ok(()) } else { Err(out.trim().to_string()) }
}

/// Saves the driver a device uses now, so an install can be undone. Returns the backup folder.
fn backup_driver(inf: &str, stamp: u64) -> Option<String> {
    if !extras::is_oem(inf) {
        return None; // built into Windows: always still on the PC
    }
    let dir = data_dir().join("backups").join(stamp.to_string()).join(inf);
    std::fs::create_dir_all(&dir).ok()?;
    let out = run_hidden("pnputil", &["/export-driver", inf, &dir.to_string_lossy()]);
    out.to_lowercase().contains("success").then(|| dir.to_string_lossy().into_owned())
}

/// Only third-party (oemNN.inf) drivers can be removed again.
pub fn can_undo(c: &Change) -> bool {
    extras::is_oem(&c.new_inf)
}

/// Undoes a change: re-adds the saved old driver and removes the new one, so Windows
/// falls back to the old driver.
pub fn rollback(c: &Change) -> Result<String, String> {
    if !extras::is_oem(&c.new_inf) {
        return Err("The new driver is one of Windows' own, so it can't be removed. Use Device Manager's Roll Back Driver instead.".into());
    }
    let mut log = String::new();
    if !c.backup.is_empty() {
        log += &run_hidden("pnputil", &["/add-driver", &format!("{}\\*.inf", c.backup), "/subdirs"]);
    }
    let out = run_hidden("pnputil", &["/delete-driver", &c.new_inf, "/uninstall", "/force"]);
    log += &out;
    run_hidden("pnputil", &["/scan-devices"]);
    if out.to_lowercase().contains("deleted successfully") { Ok(log) } else { Err(log) }
}

// StartupApproved values: first byte even = enabled, odd = disabled (Task Manager's format).
// Diagnostics-Performance event 101 records apps that slowed down boot.
const STARTUP: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
$base = 'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\'
$ok = @{}
foreach ($k in 'HKCU:\','HKLM:\') { foreach ($s in 'Run','Run32','StartupFolder') {
  $p = Get-ItemProperty ($k + $base + $s)
  if ($p) { $p.PSObject.Properties | Where-Object { $_.Value -is [byte[]] } | ForEach-Object { $ok[$_.Name.ToLower()] = ($_.Value[0] % 2) -eq 0 } }
} }
$slow = @{}
Get-WinEvent -FilterHashtable @{LogName='Microsoft-Windows-Diagnostics-Performance/Operational'; Id=101} -MaxEvents 300 | ForEach-Object {
  if ($_.Message -match 'File Name\s*:\s*(\S+)' ) { $f = $Matches[1].ToLower()
    if ($_.Message -match 'Degradation Time\s*:\s*(\d+)') { $ms = [int64]$Matches[1]; if ($ms -gt $slow[$f]) { $slow[$f] = $ms } } }
}
Get-CimInstance Win32_StartupCommand | ForEach-Object {
  $n = $_.Name.ToLower(); $e = $ok[$n]; if ($e -eq $null) { $e = $ok[$n + '.lnk'] }; if ($e -eq $null) { $e = $true }
  $d = ''; foreach ($f in $slow.Keys) { if ($_.Command.ToLower().Contains($f)) { $d = $slow[$f] } }
  "$($_.Name)`t$($_.Command)`t$($_.Location)`t$([int]$e)`t$d"
}
"#;

pub fn startup_apps() -> Vec<StartupApp> {
    let (out, _) = ps(STARTUP);
    rows(&out, 5)
        .into_iter()
        .map(|r| StartupApp { name: r[0].clone(), command: r[1].clone(), location: r[2].clone(), enabled: r[3] == "1", delay_ms: r[4].parse().ok() })
        .collect()
}

/// Turns a startup app on or off the same way Task Manager does.
pub fn set_startup(app: &StartupApp, enabled: bool) -> Result<(), String> {
    let loc = app.location.to_lowercase();
    let (hive, sub, value) = if loc.contains("startup") {
        (if loc.contains("common") { "HKLM:" } else { "HKCU:" }, "StartupFolder", format!("{}.lnk", app.name))
    } else {
        (if loc.starts_with("hklm") { "HKLM:" } else { "HKCU:" }, if loc.contains("wow6432") { "Run32" } else { "Run" }, app.name.clone())
    };
    // The value name comes from the registry, so it travels base64-encoded, never as script text.
    let name = base64::engine::general_purpose::STANDARD.encode(value.as_bytes());
    let script = format!(
        "$k = '{hive}\\Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\{sub}'\n\
         if (-not (Test-Path $k)) {{ New-Item $k -Force | Out-Null }}\n\
         $n = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{name}'))\n\
         New-ItemProperty -Path $k -Name $n -PropertyType Binary -Value ([byte[]]({flag},0,0,0,0,0,0,0,0,0,0,0)) -Force -ErrorAction Stop | Out-Null",
        flag = if enabled { 2 } else { 3 }
    );
    let (_, err) = ps(&script);
    if err.trim().is_empty() { Ok(()) } else { Err(err.trim().to_string()) }
}

// ---------------- Health ----------------

const HEALTH: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
$cs = Get-CimInstance Win32_ComputerSystem
$os = Get-CimInstance Win32_OperatingSystem
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$bios = Get-CimInstance Win32_BIOS
$bb = Get-CimInstance Win32_BaseBoard
$ram = @(Get-CimInstance Win32_PhysicalMemory)
$up = (Get-Date) - $os.LastBootUpTime
"SYS`tComputer`t$($cs.Manufacturer) $($cs.Model)"
"SYS`tWindows`t$($os.Caption) (build $($os.BuildNumber))"
"SYS`tProcessor`t$($cpu.Name.Trim()), $($cpu.NumberOfCores) cores / $($cpu.NumberOfLogicalProcessors) threads"
"SYS`tMemory`t" + [math]::Round($cs.TotalPhysicalMemory / 1GB) + " GB, $($ram.Count) module(s) at $($ram[0].ConfiguredClockSpeed) MT/s"
"SYS`tMotherboard`t$($bb.Manufacturer) $($bb.Product)"
"SYS`tBIOS`t$($bios.Manufacturer) $($bios.SMBIOSBIOSVersion), " + $bios.ReleaseDate.ToString('yyyy-MM-dd')
"SYS`tUptime`t$($up.Days)d $($up.Hours)h $($up.Minutes)m"
"RAM`t$("$($ram[0].PartNumber)".Trim())`t$($ram[0].ConfiguredClockSpeed)"
Get-CimInstance Win32_VideoController | ForEach-Object { "GPU`t$($_.Name)`t$($_.DriverVersion)" }
Get-PhysicalDisk | ForEach-Object {
  $r = $_ | Get-StorageReliabilityCounter
  @('DISK', $_.FriendlyName, $_.MediaType, $_.BusType, $_.Size, $_.HealthStatus, $r.Temperature, $r.Wear, $r.ReadErrorsUncorrected, $r.PowerOnHours) -join "`t"
}
$d = @(Get-CimInstance -Namespace root\wmi -ClassName BatteryStaticData)
$f = @(Get-CimInstance -Namespace root\wmi -ClassName BatteryFullChargedCapacity)
$b = @(Get-CimInstance Win32_Battery)
if ($d.Count -and $f.Count) { "BAT`t$($d[0].DesignedCapacity)`t$($f[0].FullChargedCapacity)`t$($b[0].EstimatedChargeRemaining)" }
$since = (Get-Date).AddDays(-30)
$whea = @(Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='Microsoft-Windows-WHEA-Logger'; StartTime=$since}).Count
$power = @(Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='Microsoft-Windows-Kernel-Power'; Id=41; StartTime=$since}).Count
$bsod = @(Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='Microsoft-Windows-WER-SystemErrorReporting'; Id=1001; StartTime=$since}).Count
$tdr = @(Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='Display'; Id=4101; StartTime=$since}).Count
"EVT`t$whea`t$power`t$bsod`t$tdr"
"#;

/// Reads everything except the WHEA groups; takes a few seconds (event log queries).
pub fn health_report() -> Result<Report, String> {
    let (out, err) = ps(HEALTH);
    let r = health::parse(&out);
    if r.system.is_empty() { Err(if err.trim().is_empty() { "No data returned".into() } else { err }) } else { Ok(r) }
}

/// The newest NVIDIA Game Ready driver, when the PC has an NVIDIA card.
pub fn gpu_latest(r: &Report) -> Option<(String, String, String)> {
    r.gpus.iter().any(|(n, _)| crate::hwkb::gpu_kind(n) == crate::hwkb::GpuKind::Nvidia).then(extras::latest_nvidia).flatten()
}

const WHEA: &str = r#"
$since = (Get-Date).AddDays(-DAYS)
Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='Microsoft-Windows-WHEA-Logger'; StartTime=$since} -MaxEvents 2000 -ErrorAction SilentlyContinue | ForEach-Object {
  $_.TimeCreated.ToString('yyyy-MM-dd HH:mm') + "`t" + $_.Id + "`t" + ($_.Message -replace "`r?`n", [string][char]31)
}
"#;

/// WHEA events from the last `days` days, newest first.
pub fn whea_read(days: u32) -> Vec<Event> {
    let (out, _) = ps(&WHEA.replace("DAYS", &days.to_string()));
    rows(&out, 3)
        .into_iter()
        .filter_map(|r| {
            let (fatal, fields) = parse_message(&r[2].replace('\u{1F}', "\n"));
            Some(Event { time: r[0].clone(), id: r[1].parse().ok()?, fatal, fields })
        })
        .collect()
}

// ---------------- Storage ----------------

fn env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from).filter(|p| p.is_absolute())
}

const USER_DIRS: [&str; 6] = ["Desktop", "Documents", "Downloads", "Pictures", "Videos", "Music"];

/// What to measure on the system drive: (drive, parts, your folders).
pub fn breakdown_plan() -> (String, Vec<(&'static str, Vec<PathBuf>)>, Vec<PathBuf>) {
    let sys = env("SystemDrive").map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| "C:".into());
    let home = env("USERPROFILE");
    let user_dirs: Vec<PathBuf> = home.iter().flat_map(|h| USER_DIRS.iter().map(move |d| h.join(d))).collect();
    let parts: Vec<(&'static str, Vec<PathBuf>)> = vec![
        ("Windows", env("SystemRoot").into_iter().collect()),
        ("Programs", ["ProgramFiles", "ProgramFiles(x86)"].iter().filter_map(|v| env(v)).collect()),
        ("Program data", env("ProgramData").into_iter().collect()),
        ("Your files", user_dirs.clone()),
        ("App data & caches", home.iter().map(|h| h.join("AppData")).collect()),
    ];
    (sys, parts, user_dirs)
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

/// Finds and measures everything the cleaner can remove. Takes 10-30 seconds.
pub fn find_junk() -> Vec<Junk> {
    use crate::storage::{app_name, is_oem_inf, is_package_name, old_drivers};
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
    use crate::storage::{is_oem_inf, is_package_name};
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

// ---------------- Monitor ----------------

// Windows' per-engine GPU counters, the same ones Task Manager uses. Each instance is
// pid_<pid>_..._engtype_<type>; GPU load is the busiest engine type.
const GPU_SCRIPT: &str = r#"
$inv = [cultureinfo]::InvariantCulture
while ($true) {
  $s = (Get-Counter '\GPU Engine(*)\Utilization Percentage' -ErrorAction SilentlyContinue).CounterSamples
  $eng = @{}; $pp = @{}
  foreach ($x in $s) {
    if ($x.InstanceName -match 'pid_(\d+)_.*engtype_([A-Za-z0-9]+)') {
      $eng[$Matches[2]] += $x.CookedValue
      $k = "$($Matches[1])|$($Matches[2])"; $pp[$k] += $x.CookedValue
    }
  }
  $tot = 0; foreach ($v in $eng.Values) { if ($v -gt $tot) { $tot = $v } }
  $per = @{}
  foreach ($k in $pp.Keys) { $p = $k.Split('|')[0]; if ($pp[$k] -gt $per[$p]) { $per[$p] = $pp[$k] } }
  $list = ($per.GetEnumerator() | Where-Object { $_.Value -ge 0.1 } | ForEach-Object { "$($_.Key):" + ([math]::Round($_.Value, 1)).ToString($inv) }) -join ','
  [Console]::Out.WriteLine("GPU`t" + ([math]::Round([math]::Min($tot, 100), 1)).ToString($inv) + "`t" + $list)
  [Console]::Out.Flush()
}
"#;

/// GPU sampler: a hidden PowerShell that prints one line per second. Runs until the
/// monitor run `id` stops.
pub fn gpu_sampler(m: &Arc<Mutex<Monitor>>, id: u64) {
    let path = std::env::temp_dir().join(format!("flash-gpu-{}.ps1", std::process::id()));
    if std::fs::write(&path, GPU_SCRIPT).is_err() {
        return;
    }
    let Ok(mut child) = Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(NO_WINDOW)
        .spawn()
    else {
        return;
    };
    if let Some(out) = child.stdout.take() {
        for line in std::io::BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            if !crate::monitor::alive(m, id) {
                break;
            }
            if let Some((total, per)) = crate::monitor::parse_gpu(&line) {
                let mut g = m.lock().unwrap();
                g.gpu_seen = true;
                g.gpu_total = total;
                g.gpu_by_pid = per;
            }
        }
    }
    child.kill().ok();
    child.wait().ok();
    std::fs::remove_file(&path).ok();
}

/// Temperatures and fans: ACPI sensors on any PC, plus nvidia-smi on NVIDIA cards.
pub fn sensors(comps: &mut sysinfo::Components) -> (Vec<(String, f32)>, Vec<(String, String)>) {
    comps.refresh(true);
    let mut temps: Vec<(String, f32)> = comps.list().iter().filter_map(|c| c.temperature().filter(|t| *t > 0.0 && *t < 150.0).map(|t| (c.label().to_string(), t))).collect();
    let mut fans = Vec::new();
    let smi = run_hidden("nvidia-smi", &["--query-gpu=name,temperature.gpu,fan.speed", "--format=csv,noheader,nounits"]);
    for line in smi.lines() {
        let p: Vec<&str> = line.split(',').map(str::trim).collect();
        if p.len() == 3 {
            if let Ok(t) = p[1].parse::<f32>() {
                temps.push((format!("{} (GPU)", p[0]), t));
            }
            if let Ok(f) = p[2].parse::<f32>() {
                fans.push((format!("{} fan", p[0]), format!("{f:.0}%")));
            }
        }
    }
    (temps, fans)
}

// ---------------- Benchmarks ----------------

const NO_BUFFERING: u32 = 0x2000_0000; // FILE_FLAG_NO_BUFFERING
const WRITE_THROUGH: u32 = 0x8000_0000; // FILE_FLAG_WRITE_THROUGH

/// Opens the benchmark file past the Windows file cache.
pub fn open_unbuffered(path: &Path, write: bool) -> std::io::Result<std::fs::File> {
    if write {
        std::fs::OpenOptions::new().write(true).create(true).truncate(true).custom_flags(NO_BUFFERING | WRITE_THROUGH).open(path)
    } else {
        std::fs::OpenOptions::new().read(true).custom_flags(NO_BUFFERING).open(path)
    }
}

// ---------------- Opening things ----------------

/// Opens a URL or file with its default app.
pub fn open(target: &str) {
    run_hidden("cmd", &["/c", "start", "", target]);
}

/// Shows a file selected in Explorer.
pub fn show_in_folder(path: &Path) {
    let arg = format!("/select,{}", path.to_string_lossy());
    Command::new("explorer").arg(arg).spawn().ok();
}

pub fn desktop_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from).map(|p| p.join("Desktop")).filter(|d| d.is_dir())
}

pub fn open_tool(tool: Tool) -> Result<String, String> {
    match tool {
        Tool::DeviceManager => open("devmgmt.msc"),
        Tool::SystemRestore => open("rstrui.exe"),
        Tool::EventViewer => open("eventvwr.msc"),
        Tool::MemoryTest => {
            Command::new("mdsched.exe").spawn().ok();
        }
        Tool::StartupManager => {
            Command::new("taskmgr").args(["/0", "/startup"]).spawn().ok();
        }
    }
    Ok(String::new())
}

// ---------------- Updating Flash ----------------

fn installed_path(exe: &Path) -> Option<PathBuf> {
    let mut value = vec![0_u16; 32768];
    let mut size = (value.len() * size_of::<u16>()) as u32;
    // The per-machine MSI writes this value in the 64-bit registry view.
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("Software\\Flash"),
            w!("InstalledExe"),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            std::ptr::null_mut(),
            value.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if result != 0 || size < 2 {
        return None;
    }
    let registered = OsString::from_wide(&value[..size as usize / 2 - 1]);
    let registered = PathBuf::from(registered);
    (std::fs::canonicalize(&registered).ok()? == std::fs::canonicalize(exe).ok()?).then_some(registered)
}

/// The MSI registers its exe; anything else is the portable or standalone exe.
pub fn install_kind(exe: &Path) -> InstallKind {
    if installed_path(exe).is_some() { InstallKind::Msi } else { InstallKind::Exe }
}

/// The exe an update replaces: the MSI's registered path, or the running exe.
pub fn installed_exe(exe: &Path) -> PathBuf {
    installed_path(exe).unwrap_or_else(|| exe.to_path_buf())
}

fn program_files() -> std::io::Result<PathBuf> {
    let mut path = std::ptr::null_mut();
    // Windows allocates the returned NUL-terminated path with CoTaskMemAlloc.
    let result = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, 0, std::ptr::null_mut(), &mut path) };
    if result < 0 || path.is_null() {
        return Err(std::io::Error::other("Windows could not locate Program Files."));
    }
    let folder = unsafe {
        let len = (0..).find(|&index| *path.add(index) == 0).unwrap();
        let folder = OsString::from_wide(std::slice::from_raw_parts(path, len));
        CoTaskMemFree(path.cast());
        folder
    };
    Ok(folder.into())
}

/// Flash already runs elevated. Keep the payload under Program Files so a
/// standard user cannot replace it between verification and installation.
pub fn update_dir() -> std::io::Result<PathBuf> {
    program_files()
}

fn system_directory() -> std::io::Result<PathBuf> {
    let mut path = [0_u16; 260];
    let len = unsafe { GetSystemDirectoryW(path.as_mut_ptr(), path.len() as u32) } as usize;
    if len == 0 || len >= path.len() {
        return Err(std::io::Error::other("Windows could not locate its system directory."));
    }
    Ok(PathBuf::from(OsString::from_wide(&path[..len])))
}

/// Starts the helper script that swaps the exe or runs the MSI once Flash has closed.
pub fn apply_update(exe: &Path, path: &Path, digest: &str, kind: &InstallKind) -> std::io::Result<()> {
    let system = system_directory()?;
    let powershell = system.join(r"WindowsPowerShell\v1.0\powershell.exe");
    let script: Vec<u8> = include_str!("../apply_update.ps1").encode_utf16().flat_map(u16::to_le_bytes).collect();
    // Only fixed source code goes on the command line. Paths and release
    // data are passed as values, never interpolated into the script.
    Command::new(powershell)
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand"])
        .arg(base64::engine::general_purpose::STANDARD.encode(script))
        .env("PSModulePath", system.join(r"WindowsPowerShell\v1.0\Modules"))
        .env("FLASH_UPDATE_EXE", exe)
        .env("FLASH_UPDATE_PATH", path)
        .env("FLASH_UPDATE_HASH", digest)
        .env("FLASH_UPDATE_VERSION", env!("CARGO_PKG_VERSION"))
        .env("FLASH_UPDATE_KIND", if *kind == InstallKind::Msi { "msi" } else { "exe" })
        .env("FLASH_UPDATE_PARENT", std::process::id().to_string())
        .env("FLASH_UPDATE_SYSTEM", &system)
        .current_dir(system)
        .creation_flags(NO_WINDOW)
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn ids_and_devices() {
        let ids = ["0a1b-2C".to_string(), "x'; rm".to_string(), String::new()];
        assert_eq!(ps_ids(&ids), "@('0a1b-2C')");
        let out = "OS\t26200\nPCI\\X\tSMBus\tSystem\tAMD\t5.12.0.31\t2020-01-01\tAMD\toem1.inf\tMS\t0\tPCI\\VEN_1&DEV_2|PCI\\VEN_1\tPCI\\CC_0C05\n";
        let (devs, build) = parse_devices(out);
        assert_eq!(build, 26200);
        assert_eq!(devs[0].hwids, vec!["PCI\\VEN_1&DEV_2", "PCI\\VEN_1"]);
        assert_eq!(upgrade(&devs[0], "2021-01-01", "5.13.0.0", "AMD"), Some(false));
        assert_eq!(upgrade(&devs[0], "2026-04-06", "3.0.1.1338", "AMD"), None); // newer date, lower version
        assert_eq!(upgrade(&devs[0], "2020-01-01", "5.12.0.31", "AMD"), None);
        let inbox = Device { provider: "Microsoft".into(), version: "10.0.26100.1".into(), date: "2006-06-21".into(), ..Default::default() };
        assert_eq!(upgrade(&inbox, "2026-02-26", "1.1.84.7627", "Logitech USB Driver Update"), Some(true));
    }

    #[test]
    fn purposes() {
        assert_eq!(purpose("AudioEndpoint"), "Audio");
        assert_eq!(purpose("HIDClass"), "Input");
        assert_eq!(purpose("SecurityDevices"), "Security");
        assert_eq!(purpose(""), "Other");
        assert!(["Display", "Net", "System", "PrintQueue", "SoftwareDevice"].iter().all(|c| crate::PURPOSES.contains(&purpose(c))));
    }

    #[test]
    fn stop_kills_a_running_scan_query_only_during_scans() {
        crate::stop_scan(); // not scanning: must be ignored
        assert!(!crate::STOP.load(Ordering::SeqCst));
        crate::SCANNING.store(true, Ordering::SeqCst);
        let stopper = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(800));
            crate::stop_scan();
        });
        let start = std::time::Instant::now();
        let (out, _) = ps("Start-Sleep -Seconds 30; 'finished'");
        let took = start.elapsed();
        stopper.join().unwrap();
        crate::SCANNING.store(false, Ordering::SeqCst);
        crate::STOP.store(false, Ordering::SeqCst);
        assert!(!out.contains("finished"));
        assert!(took < std::time::Duration::from_secs(10), "stop took {took:?}");
    }

    /// Reads this PC's real log: `cargo test --lib live_log -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_log() {
        use crate::whea::{Context, Cpu, explain, group};
        let ctx = Context { cpu: Cpu::Amd, ram_overclocked: Some(false), arch: crate::hwkb::CpuArch::AmdZen3 };
        for g in group(&whea_read(90)) {
            let a = explain(&g, &ctx);
            println!("{} x{} id={} fatal={} [{} | {} | {}] dev={} apic={:?} {}..{}\n  => {} | first fix: {}", a.title, g.count, g.id, g.fatal, g.component, g.source, g.error_type, g.device, g.apic, g.first, g.last, a.meaning, a.fixes[0]);
        }
    }
}
