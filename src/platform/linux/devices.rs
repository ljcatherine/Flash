//! Devices on Linux: every PCI and USB device from the udev database (names from the
//! pci.ids/usb.ids databases), the driver bound to each in sysfs, that driver's module
//! (`modinfo`), and problems from sysfs and this boot's kernel log. Firmware that fwupd can
//! update (UEFI, NVMe, Secure Boot keys...) is listed as devices too, like Windows'
//! "System Firmware".
use super::{cmd, fwupd, has, read};
use crate::Device;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// One device record from `udevadm info --export-db`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct UdevEntry {
    pub path: String,
    pub props: BTreeMap<String, String>,
}

impl UdevEntry {
    pub fn get(&self, key: &str) -> &str {
        self.props.get(key).map_or("", String::as_str)
    }
}

/// Parses `udevadm info --export-db`: blank-line separated records of `P: path` and
/// `E: KEY=value` lines.
pub(crate) fn parse_udev_db(text: &str) -> Vec<UdevEntry> {
    let mut out = Vec::new();
    let mut cur = UdevEntry::default();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !cur.path.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if let Some(p) = line.strip_prefix("P: ") {
            cur.path = p.to_string();
        } else if let Some((k, v)) = line.strip_prefix("E: ").and_then(|kv| kv.split_once('=')) {
            cur.props.insert(k.to_string(), v.to_string());
        }
    }
    if !cur.path.is_empty() {
        out.push(cur);
    }
    out
}

/// What `modinfo` says about one module.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ModInfo {
    pub version: String,
    pub intree: bool,
    pub signer: String,
    pub filename: String,
    pub description: String,
}

pub(crate) fn parse_modinfo(text: &str) -> ModInfo {
    let mut m = ModInfo::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        match k.trim() {
            "version" => m.version = v.to_string(),
            "intree" => m.intree = v == "Y",
            "signer" => m.signer = v.to_string(),
            "filename" => {
                m.filename = v.to_string();
                // Built into the kernel image: modinfo prints no "intree" line for these.
                m.intree |= v == "(builtin)";
            }
            "description" if m.description.is_empty() => m.description = v.to_string(),
            _ => {}
        }
    }
    m
}

/// Everything about the running system the device list needs, gathered once per scan so
/// building the list itself stays a pure function (and testable with captured output).
#[derive(Default)]
pub(crate) struct Facts {
    /// Kernel release (`uname -r`) and its build date (yyyy-mm-dd).
    pub kernel: String,
    pub kernel_date: String,
    /// driver name -> module name, for drivers that are loadable modules.
    pub driver_module: HashMap<String, String>,
    /// module name -> modinfo.
    pub modules: HashMap<String, ModInfo>,
    /// USB device name ("1-5") -> authorized (false: switched off).
    pub usb_authorized: HashMap<String, bool>,
    /// modalias -> modules that could drive it (`modprobe -R`), for devices without a driver.
    pub candidates: HashMap<String, Vec<String>>,
    /// device name ("0000:03:00.0", "1-5") -> problem code from the kernel log.
    pub log_problems: HashMap<String, u32>,
}

// Problem codes. Where Windows has a code that means the same, Linux uses it too.
pub(crate) const CANNOT_START: u32 = 10;
pub(crate) const RESTART: u32 = 14;
pub(crate) const DISABLED: u32 = 22;
pub(crate) const NO_DRIVER: u32 = 28;
pub(crate) const NOT_LOADED: u32 = 31;
pub(crate) const UNSIGNED: u32 = 52;
pub(crate) const FIRMWARE_MISSING: u32 = 101;
pub(crate) const FIRMWARE_FAILED: u32 = 102;

/// The provider of every driver that ships inside the kernel.
pub(crate) const KERNEL: &str = "Linux kernel";

/// In-kernel drivers are Linux's built-in drivers.
pub fn is_builtin(provider: &str) -> bool {
    provider == KERNEL
}

/// Linux problem codes, in plain words.
pub fn problem_text(code: u32) -> &'static str {
    match code {
        0 => "Working",
        CANNOT_START => "Device cannot start",
        RESTART => "Restart required",
        DISABLED => "Disabled",
        NO_DRIVER => "No driver installed",
        NOT_LOADED => "Driver could not load",
        UNSIGNED => "Driver signature could not be verified",
        FIRMWARE_MISSING => "Firmware file missing",
        FIRMWARE_FAILED => "Firmware update failed",
        _ => "Problem reported",
    }
}

/// What to do about a problem when no source found a driver for it.
pub fn problem_fix(code: u32) -> &'static str {
    match code {
        DISABLED => "Enable it again (it was switched off in sysfs or by a USB policy)",
        RESTART => "Restart the PC",
        NOT_LOADED => "Its driver didn't load: check it isn't blacklisted in /etc/modprobe.d",
        CANNOT_START => "The driver failed to start it: see the kernel log",
        UNSIGNED => "Sign the module for Secure Boot (mokutil), or turn Secure Boot off",
        FIRMWARE_MISSING => "Update linux-firmware, or install your distribution's firmware package",
        FIRMWARE_FAILED => "Try the firmware update again, or check fwupdmgr get-history",
        _ => "No driver found. Try a vendor folder or your distribution's packages",
    }
}

/// Groups a device class name (from pci.ids/usb.ids or Flash's own tables) by purpose.
pub fn purpose(class: &str) -> &'static str {
    let c = class.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| c.contains(w));
    if any(&["vga", "3d controller", "display"]) {
        "Display"
    } else if any(&["bluetooth"]) {
        "Bluetooth"
    } else if any(&["audio", "sound"]) {
        "Audio"
    } else if any(&["sata", "non-volatile", "nvme", "raid", "ide ", "ide interface", "scsi", "mass storage", "sd host", "storage", "sas "]) {
        "Storage"
    } else if any(&["smbus", "host bridge", "isa bridge", "pci bridge", "iommu", "system peripheral", "instrumentation", "signal processing", "memory controller", "processor", "system firmware", "communication controller", "bridge"]) {
        "Chipset & system"
    } else if any(&["ethernet", "network", "wireless", "wi-fi", "communications"]) {
        "Network"
    } else if any(&["human interface", "keyboard", "mouse", "input"]) {
        "Input"
    } else if any(&["usb", "hub", "serial bus", "thunderbolt"]) {
        "USB & ports"
    } else if any(&["video", "imaging", "camera"]) {
        "Cameras & imaging"
    } else if any(&["printer"]) {
        "Printers"
    } else if any(&["smart card", "encryption", "security", "secure boot", "tpm", "fingerprint"]) {
        "Security"
    } else if any(&["virtual", "virtio"]) {
        "Software & virtual"
    } else {
        "Other"
    }
}

/// PCI class code (`PCI_CLASS`, hex) to a name, for devices pci.ids doesn't name.
fn pci_class_name(code: u32) -> &'static str {
    match (code >> 16, (code >> 8) & 0xff) {
        (0x01, 0x06) => "SATA controller",
        (0x01, 0x08) => "Non-Volatile memory controller",
        (0x01, 0x04) => "RAID bus controller",
        (0x01, _) => "Mass storage controller",
        (0x02, 0x00) => "Ethernet controller",
        (0x02, _) => "Network controller",
        (0x03, 0x00) => "VGA compatible controller",
        (0x03, 0x02) => "3D controller",
        (0x03, _) => "Display controller",
        (0x04, 0x00) => "Multimedia video controller",
        (0x04, _) => "Audio device",
        (0x05, _) => "Memory controller",
        (0x06, _) => "Bridge",
        (0x07, _) => "Communication controller",
        (0x08, _) => "Generic system peripheral",
        (0x09, _) => "Input device controller",
        (0x0c, 0x03) => "USB controller",
        (0x0c, 0x05) => "SMBus",
        (0x0c, _) => "Serial bus controller",
        (0x0d, 0x11) => "Bluetooth",
        (0x0d, _) => "Wireless controller",
        (0x10, _) => "Encryption controller",
        (0x11, _) => "Signal processing controller",
        (0x12, _) => "Processing accelerators",
        _ => "PCI device",
    }
}

/// PCI classes that always need a driver to be useful (a missing one is a problem).
fn pci_needs_driver(code: u32) -> bool {
    matches!(code >> 16, 0x01 | 0x02 | 0x03 | 0x04 | 0x0d) || code >> 8 == 0x0c03
}

/// USB interface class (from `INTERFACE=class/subclass/protocol`, decimal) to a name.
fn usb_class_name(class: u32, sub: u32, proto: u32) -> &'static str {
    match class {
        1 => "Audio",
        2 => "Communications",
        3 => "Human Interface Device",
        6 => "Imaging",
        7 => "Printer",
        8 => "Mass storage",
        9 => "Hub",
        0x0a => "Communications data",
        0x0b => "Smart card",
        0x0e => "Video",
        0x10 => "Audio/Video",
        0xe0 if sub == 1 && proto == 1 => "Bluetooth",
        0xe0 => "Wireless",
        0xef => "Miscellaneous",
        0xff => "Vendor specific",
        _ => "USB device",
    }
}

fn usb_needs_driver(class: u32) -> bool {
    matches!(class, 1 | 2 | 3 | 7 | 8 | 0x0e | 0xe0)
}

fn triple(s: &str) -> (u32, u32, u32) {
    let mut it = s.split('/').map(|p| p.trim().parse::<u32>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// Last path segment ("/devices/.../usb1/1-5" -> "1-5").
fn sysname(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Turns udev's `ID_MODEL` (underscores for spaces) into a name.
fn tidy(s: &str) -> String {
    s.replace('_', " ").trim().to_string()
}

/// Version, date, provider and signer for a bound driver.
fn driver_details(f: &Facts, driver: &str) -> (String, String, String, String, String) {
    match f.driver_module.get(driver) {
        Some(module) => {
            // Without modinfo's answer, assume the usual case: a module that ships with the kernel.
            let Some(info) = f.modules.get(module).filter(|i| !i.intree) else {
                let signer = f.modules.get(module).map(|i| i.signer.clone()).unwrap_or_default();
                return (module.clone(), f.kernel.clone(), f.kernel_date.clone(), KERNEL.into(), signer);
            };
            let dkms = ["/updates/", "/dkms/", "/extra/"].iter().any(|d| info.filename.contains(d));
            let provider = if dkms { "DKMS module" } else { "Out-of-tree module" };
            let version = if info.version.is_empty() { f.kernel.clone() } else { info.version.clone() };
            (module.clone(), version, String::new(), provider.into(), info.signer.clone())
        }
        // Built into the kernel image, not a loadable module.
        None => (driver.to_string(), f.kernel.clone(), f.kernel_date.clone(), KERNEL.into(), String::new()),
    }
}

/// A problem for a device that has no driver: what the kernel log says, or whether any
/// installed module could drive it.
fn driverless_problem(f: &Facts, name: &str, modalias: &str, needs_driver: bool) -> u32 {
    if let Some(code) = f.log_problems.get(name) {
        return *code;
    }
    let candidates = f.candidates.get(modalias).map_or(0, Vec::len);
    if candidates > 0 {
        NOT_LOADED
    } else if needs_driver {
        NO_DRIVER
    } else {
        0
    }
}

/// Builds the device list from the udev database and the gathered facts.
pub(crate) fn build(entries: &[UdevEntry], f: &Facts) -> Vec<Device> {
    let mut devices = Vec::new();
    for e in entries.iter().filter(|e| e.get("SUBSYSTEM") == "pci") {
        let slot = e.get("PCI_SLOT_NAME").to_string();
        if slot.is_empty() {
            continue;
        }
        let code = u32::from_str_radix(e.get("PCI_CLASS"), 16).unwrap_or(0);
        let class = [e.get("ID_PCI_SUBCLASS_FROM_DATABASE"), e.get("ID_PCI_CLASS_FROM_DATABASE")].into_iter().find(|s| !s.is_empty()).unwrap_or(pci_class_name(code)).to_string();
        let ids = e.get("PCI_ID").to_ascii_lowercase();
        let subsys = e.get("PCI_SUBSYS_ID").to_ascii_lowercase();
        let modalias = e.get("MODALIAS").to_string();
        let driver = e.get("DRIVER");
        let mut dev = Device {
            instance: slot.clone(),
            name: [e.get("ID_MODEL_FROM_DATABASE").to_string(), format!("PCI device {ids}")].into_iter().find(|s| !s.is_empty() && s != "PCI device ").unwrap_or_default(),
            class,
            manufacturer: e.get("ID_VENDOR_FROM_DATABASE").to_string(),
            hwids: [modalias.clone(), ids.clone(), format!("{ids} {subsys}")].into_iter().filter(|s| !s.trim().is_empty()).collect(),
            compat: vec![format!("pci class {code:06x}")],
            ..Default::default()
        };
        if driver.is_empty() {
            dev.problem = driverless_problem(f, &slot, &modalias, pci_needs_driver(code));
            if dev.problem == 0 {
                continue; // like Windows: devices with neither a driver nor a problem aren't listed
            }
        } else {
            (dev.inf, dev.version, dev.date, dev.provider, dev.signer) = driver_details(f, driver);
            dev.problem = f.log_problems.get(&slot).copied().unwrap_or(0);
        }
        devices.push(dev);
    }

    // USB: drivers bind to a device's interfaces, so gather those first.
    let interfaces: Vec<&UdevEntry> = entries.iter().filter(|e| e.get("SUBSYSTEM") == "usb" && e.get("DEVTYPE") == "usb_interface").collect();
    for e in entries.iter().filter(|e| e.get("SUBSYSTEM") == "usb" && e.get("DEVTYPE") == "usb_device") {
        let name = sysname(&e.path).to_string();
        let prefix = format!("{}/", e.path);
        let ifs: Vec<&&UdevEntry> = interfaces.iter().filter(|i| i.path.starts_with(&prefix) && !i.path[prefix.len()..].contains('/')).collect();
        let classes: Vec<(u32, u32, u32)> = ifs.iter().map(|i| triple(i.get("INTERFACE"))).collect();
        let main = classes.iter().find(|c| c.0 == 0xe0 && c.1 == 1 && c.2 == 1).or_else(|| classes.iter().find(|c| c.0 != 0xff && c.0 != 0x0a)).or(classes.first()).copied().unwrap_or_else(|| triple(e.get("TYPE")));
        let vid = e.get("ID_VENDOR_ID").to_ascii_lowercase();
        let pid = e.get("ID_MODEL_ID").to_ascii_lowercase();
        let drivers: Vec<&str> = {
            let mut seen = HashSet::new();
            ifs.iter().map(|i| i.get("DRIVER")).filter(|d| !d.is_empty() && seen.insert(*d)).collect()
        };
        let model = [e.get("ID_MODEL_FROM_DATABASE").to_string(), tidy(e.get("ID_MODEL"))].into_iter().find(|s| !s.is_empty()).unwrap_or_else(|| format!("USB device {vid}:{pid}"));
        let mut dev = Device {
            instance: format!("usb:{name}"),
            name: model,
            class: usb_class_name(main.0, main.1, main.2).to_string(),
            manufacturer: [e.get("ID_VENDOR_FROM_DATABASE").to_string(), tidy(e.get("ID_VENDOR"))].into_iter().find(|s| !s.is_empty()).unwrap_or_default(),
            hwids: [format!("usb:v{}p{}", vid.to_ascii_uppercase(), pid.to_ascii_uppercase()), format!("{vid}:{pid}")].into_iter().filter(|s| s.len() > 6).collect(),
            compat: ifs.iter().map(|i| i.get("MODALIAS").to_string()).filter(|m| !m.is_empty()).collect(),
            ..Default::default()
        };
        if f.usb_authorized.get(&name) == Some(&false) {
            dev.problem = DISABLED;
        } else if let Some(driver) = drivers.first() {
            (dev.inf, dev.version, dev.date, dev.provider, dev.signer) = driver_details(f, driver);
            if drivers.len() > 1 {
                dev.inf = drivers.iter().map(|d| f.driver_module.get(*d).map_or(*d, String::as_str)).collect::<Vec<_>>().join(", ");
            }
            dev.problem = f.log_problems.get(&name).copied().unwrap_or(0);
        } else {
            let modalias = dev.compat.first().cloned().unwrap_or_default();
            dev.problem = driverless_problem(f, &name, &modalias, usb_needs_driver(main.0));
            if dev.problem == 0 {
                continue;
            }
        }
        devices.push(dev);
    }
    devices.sort_by(|a, b| a.class.to_lowercase().cmp(&b.class.to_lowercase()).then_with(|| a.name.cmp(&b.name)));
    devices
}

/// Problems the kernel logged this boot, by device name: missing firmware, failed probes,
/// and modules refused for their signature.
pub(crate) fn log_problems(log: &str) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    for line in log.lines() {
        let msg = line.split_once("kernel: ").map_or(line, |(_, m)| m);
        // "<driver> <device>: message"
        let device = msg.split_once(": ").and_then(|(head, _)| head.split_whitespace().nth(1)).unwrap_or("");
        let code = if msg.contains("Direct firmware load for") && msg.contains("failed with error") || msg.contains("firmware: failed to load") {
            FIRMWARE_MISSING
        } else if msg.contains("probe with driver") && msg.contains("failed with error") {
            CANNOT_START
        } else if let Some(rest) = msg.split_once(": probe of ").map(|(_, r)| r) {
            // Older kernels: "<driver>: probe of <device> failed with error -5"
            if let Some(dev) = rest.split_whitespace().next() {
                out.entry(dev.to_string()).or_insert(CANNOT_START);
            }
            continue;
        } else {
            continue;
        };
        if !device.is_empty() {
            out.entry(device.to_string()).or_insert(code);
        }
    }
    out
}

/// Whether the kernel refused an unsigned module this boot (Secure Boot).
fn refused_unsigned(log: &str) -> bool {
    log.contains("Loading of unsigned module is rejected") || log.contains("Key was rejected by service")
}

/// Kernel build date (yyyy-mm-dd) from /proc/sys/kernel/version,
/// e.g. "#1-NixOS SMP PREEMPT_DYNAMIC Tue Nov 10 18:07:06 UTC 2026".
pub(crate) fn kernel_date(version: &str) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let words: Vec<&str> = version.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        if let Some(m) = MONTHS.iter().position(|m| m == w) {
            let day = words.get(i + 1).and_then(|d| d.parse::<u32>().ok());
            let year = words[i..].iter().find_map(|y| y.parse::<u32>().ok().filter(|y| *y > 1990));
            if let (Some(d), Some(y)) = (day, year) {
                return format!("{y:04}-{:02}-{d:02}", m + 1);
            }
        }
    }
    String::new()
}

/// Asks udev to finish processing hardware events (a rescan of the bus needs root).
pub fn rescan_hardware() {
    if has("udevadm") {
        super::run_hidden("udevadm", &["settle", "--timeout=5"]);
    }
}

/// The module behind a bound driver, from sysfs.
fn module_of(bus: &str, driver: &str) -> Option<String> {
    let link = std::fs::read_link(format!("/sys/bus/{bus}/drivers/{driver}/module")).ok()?;
    link.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Every device with a driver or a problem: (devices, error text, is Windows 11 = always true).
pub fn read_devices() -> (Vec<Device>, String, bool) {
    let db = match cmd("udevadm", &["info", "--export-db"]) {
        Ok(db) => db,
        Err(e) => return (vec![], format!("couldn't read the udev database: {e}"), true),
    };
    let entries = parse_udev_db(&db);
    let mut f = Facts { kernel: read("/proc/sys/kernel/osrelease"), kernel_date: kernel_date(&read("/proc/sys/kernel/version")), ..Default::default() };
    for e in &entries {
        let bus = e.get("SUBSYSTEM");
        let driver = e.get("DRIVER");
        if (bus == "pci" || bus == "usb") && !driver.is_empty() && !f.driver_module.contains_key(driver) {
            if let Some(m) = module_of(bus, driver) {
                f.driver_module.insert(driver.to_string(), m);
            }
        }
        if bus == "usb" && e.get("DEVTYPE") == "usb_device" {
            let name = sysname(&e.path);
            let auth = read(format!("/sys/bus/usb/devices/{name}/authorized"));
            f.usb_authorized.insert(name.to_string(), auth != "0");
        }
    }
    let modules: HashSet<String> = f.driver_module.values().cloned().collect();
    for m in modules {
        if let Ok(text) = cmd("modinfo", &[&m]) {
            f.modules.insert(m, parse_modinfo(&text));
        }
    }
    // This boot's kernel log: missing firmware and failed probes.
    let log = cmd("journalctl", &["-k", "-b", "0", "-o", "short-iso", "--no-pager", "-q"]).unwrap_or_default();
    f.log_problems = log_problems(&log);
    let unsigned = refused_unsigned(&log);
    // Which installed modules could drive the devices that have none.
    for e in &entries {
        let alias = e.get("MODALIAS");
        if !alias.is_empty() && e.get("DRIVER").is_empty() && (e.get("SUBSYSTEM") == "pci" || e.get("DEVTYPE") == "usb_interface") {
            if let Ok(out) = cmd("modprobe", &["-R", alias]) {
                let mods: Vec<String> = out.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
                if !mods.is_empty() {
                    f.candidates.insert(alias.to_string(), mods);
                }
            }
        }
    }
    let mut devices = build(&entries, &f);
    if unsigned {
        for d in devices.iter_mut().filter(|d| d.problem == NOT_LOADED) {
            d.problem = UNSIGNED;
        }
    }
    devices.extend(fwupd::devices());
    (devices, String::new(), true)
}

/// fwupd's device: the plugin decides the class.
pub(crate) fn firmware_class(plugin: &str, name: &str) -> &'static str {
    match plugin {
        "nvme" => "NVMe firmware",
        "uefi_capsule" if name.contains("System") => "System firmware",
        "uefi_capsule" => "UEFI device firmware",
        p if p.starts_with("uefi_") => "Secure Boot keys",
        "tpm" => "TPM firmware",
        "ata" | "scsi" => "Storage firmware",
        "thunderbolt" | "intel_usb4" | "usb4" => "Thunderbolt firmware",
        "logitech_hidpp" | "logitech_bulkcontroller" | "elantp" | "synaptics_rmi" | "pixart_rf" => "Input device firmware",
        "dell_dock" | "synaptics_mst" | "realtek_mst" | "vli" | "ccgx" | "genesys" => "USB & dock firmware",
        "intel_me" | "intel_mei" | "pci_mei" | "amd_pmc" | "cpu" | "msr" | "pci_psp" => "System firmware",
        _ => "Device firmware",
    }
}

/// Lets the shared tests check a device exists for each purpose name.
#[allow(dead_code)]
pub(crate) fn all_purposes_reachable() -> bool {
    let samples = ["VGA compatible controller", "Audio device", "Ethernet controller", "Bluetooth", "Non-Volatile memory controller", "Human Interface Device", "USB controller", "Video", "Printer", "Encryption controller", "Host bridge", "virtio", "Vendor specific"];
    let got: HashSet<&str> = samples.iter().map(|s| purpose(s)).collect();
    crate::PURPOSES.iter().all(|p| got.contains(p))
}

/// Whether a sysfs path exists (for tests that need a running Linux).
#[allow(dead_code)]
pub(crate) fn sysfs() -> bool {
    Path::new("/sys/bus/pci").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UDEV: &str = include_str!("../../../tests/fixtures/linux/udev-export-db.txt");
    const MODINFO_NVIDIA: &str = include_str!("../../../tests/fixtures/linux/modinfo-nvidia.txt");
    const MODINFO_R8169: &str = include_str!("../../../tests/fixtures/linux/modinfo-r8169.txt");

    fn facts() -> Facts {
        let mut f = Facts { kernel: "6.18.63".into(), kernel_date: kernel_date("#1-NixOS SMP PREEMPT_DYNAMIC Tue Nov 10 18:07:06 UTC 2026"), ..Default::default() };
        for (drv, m) in [("nvidia", "nvidia"), ("r8169", "r8169"), ("xhci_hcd", "xhci_pci"), ("btusb", "btusb"), ("snd_hda_intel", "snd_hda_intel"), ("nvme", "nvme")] {
            f.driver_module.insert(drv.into(), m.into());
        }
        f.modules.insert("nvidia".into(), parse_modinfo(MODINFO_NVIDIA));
        f.modules.insert("r8169".into(), parse_modinfo(MODINFO_R8169));
        f
    }

    #[test]
    fn parses_the_real_udev_database() {
        let entries = parse_udev_db(UDEV);
        assert!(entries.len() > 50);
        let gpu = entries.iter().find(|e| e.get("PCI_SLOT_NAME") == "0000:0b:00.0").expect("the GPU");
        assert_eq!(gpu.get("DRIVER"), "nvidia");
        assert_eq!(gpu.get("ID_MODEL_FROM_DATABASE"), "TU106 [GeForce RTX 2060 Rev. A]");
        let devs = build(&entries, &facts());
        let gpu = devs.iter().find(|d| d.instance == "0000:0b:00.0").unwrap();
        assert_eq!(purpose(&gpu.class), "Display");
        assert_eq!(gpu.inf, "nvidia");
        assert_eq!(gpu.version, "595.71.05"); // the out-of-tree module's own version
        assert_eq!(gpu.provider, "Out-of-tree module");
        assert!(gpu.hwids.iter().any(|h| h == "10de:1f08"));
        assert!(gpu.hwids[0].starts_with("pci:v000010DEd00001F08"));
        let nic = devs.iter().find(|d| d.inf == "r8169").unwrap();
        assert_eq!((nic.version.as_str(), nic.provider.as_str(), nic.date.as_str()), ("6.18.63", KERNEL, "2026-11-10"));
        assert_eq!(purpose(&nic.class), "Network");
        // Host bridges have no driver and need none: not listed, like on Windows.
        assert!(!devs.iter().any(|d| d.class == "Host bridge" && d.inf.is_empty()));
        // USB devices take their class from their interfaces.
        let bt = devs.iter().find(|d| d.instance.starts_with("usb:") && d.inf.contains("btusb"));
        assert!(bt.is_none_or(|b| purpose(&b.class) == "Bluetooth"));
        assert!(devs.iter().filter(|d| d.instance.starts_with("usb:")).all(|d| !d.class.is_empty()));
        assert!(devs.iter().all(|d| d.problem == 0), "{:?}", devs.iter().filter(|d| d.problem != 0).map(|d| &d.name).collect::<Vec<_>>());
        assert!(all_purposes_reachable());
    }

    #[test]
    fn finds_problems() {
        let entries = parse_udev_db(UDEV);
        let mut f = facts();
        // The network card's driver isn't bound but a module could drive it.
        let mut entries2 = entries.clone();
        let nic = entries2.iter_mut().find(|e| e.get("DRIVER") == "r8169").unwrap();
        nic.props.remove("DRIVER");
        let alias = nic.get("MODALIAS").to_string();
        let slot = nic.get("PCI_SLOT_NAME").to_string();
        f.candidates.insert(alias, vec!["r8169".into()]);
        let d = build(&entries2, &f);
        assert_eq!(d.iter().find(|x| x.instance == slot).unwrap().problem, NOT_LOADED);
        // Without any module it's "no driver"; the kernel log's reason wins over both.
        f.candidates.clear();
        assert_eq!(build(&entries2, &f).iter().find(|x| x.instance == slot).unwrap().problem, NO_DRIVER);
        f.log_problems = log_problems(&format!("2026-11-21T10:00:00+11:00 host kernel: r8169 {slot}: Direct firmware load for rtl_nic/rtl8168h-2.fw failed with error -2\n"));
        assert_eq!(build(&entries2, &f).iter().find(|x| x.instance == slot).unwrap().problem, FIRMWARE_MISSING);
        // A switched-off USB device.
        let first_usb = entries.iter().find(|e| e.get("DEVTYPE") == "usb_device").map(|e| sysname(&e.path).to_string()).unwrap();
        f.usb_authorized.insert(first_usb.clone(), false);
        let d = build(&entries, &f);
        assert_eq!(d.iter().find(|x| x.instance == format!("usb:{first_usb}")).unwrap().problem, DISABLED);
        assert_eq!(problem_text(DISABLED), "Disabled");
        assert_eq!(problem_text(FIRMWARE_MISSING), "Firmware file missing");
    }

    #[test]
    fn reads_kernel_log_problems() {
        let log = "\
2026-11-21T10:00:01+11:00 pc kernel: amdgpu 0000:03:00.0: Direct firmware load for amdgpu/gc_11_0_1_mes_2.bin failed with error -2
2026-11-21T10:00:02+11:00 pc kernel: iwlwifi 0000:04:00.0: probe with driver iwlwifi failed with error -110
2026-11-21T10:00:03+11:00 pc kernel: r8169: probe of 0000:05:00.0 failed with error -5
2026-11-21T10:00:04+11:00 pc kernel: bluetooth hci0: Direct firmware load for rtl_bt/rtl8761bu_fw.bin failed with error -2
2026-11-21T10:00:05+11:00 pc kernel: usb 1-5: new high-speed USB device number 2 using xhci_hcd
";
        let p = log_problems(log);
        assert_eq!(p["0000:03:00.0"], FIRMWARE_MISSING);
        assert_eq!(p["0000:04:00.0"], CANNOT_START);
        assert_eq!(p["0000:05:00.0"], CANNOT_START);
        assert!(!p.contains_key("1-5"));
        assert_eq!(kernel_date("#1 SMP PREEMPT_DYNAMIC Thu Oct 24 15:00:01 UTC 2024"), "2024-10-24");
        assert_eq!(kernel_date("#45-Ubuntu SMP PREEMPT_DYNAMIC Fri Aug 30 12:02:04 UTC 2024"), "2024-08-30");
    }

    #[test]
    fn modinfo_fields() {
        let n = parse_modinfo(MODINFO_NVIDIA);
        assert_eq!(n.version, "595.71.05");
        assert!(!n.intree);
        assert!(parse_modinfo("name:           usbcore\nfilename:       (builtin)\nlicense:        GPL\n").intree); // captured on this PC
        let r = parse_modinfo(MODINFO_R8169);
        assert!(r.intree && r.version.is_empty());
        assert_eq!(r.description, "RealTek RTL-8169 Gigabit Ethernet driver");
    }
}
