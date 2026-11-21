//! Firmware through fwupd and the Linux Vendor Firmware Service (LVFS): device firmware,
//! UEFI/BIOS capsules and Secure Boot key updates. fwupd checks its own polkit rules, so
//! Flash runs `fwupdmgr` as the user.
use super::{cmd, cmd_any, devices, has};
use crate::{Device, Source, Update};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, Default)]
struct Root {
    #[serde(rename = "Devices", default)]
    devices: Vec<FwDevice>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct FwDevice {
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "DeviceId", default)]
    pub id: String,
    #[serde(rename = "Vendor", default)]
    pub vendor: String,
    #[serde(rename = "Version", default)]
    pub version: String,
    #[serde(rename = "Plugin", default)]
    pub plugin: String,
    #[serde(rename = "Flags", default)]
    pub flags: Vec<String>,
    #[serde(rename = "InstanceIds", default)]
    pub instance_ids: Vec<String>,
    #[serde(rename = "Guid", default)]
    pub guids: Vec<String>,
    /// fwupd's FwupdUpdateState: 1 pending, 2 success, 3 failed, 4 needs reboot.
    #[serde(rename = "UpdateState", default)]
    pub update_state: Value,
    #[serde(rename = "Releases", default)]
    pub releases: Vec<FwRelease>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct FwRelease {
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "Summary", default)]
    pub summary: String,
    #[serde(rename = "Version", default)]
    pub version: String,
    #[serde(rename = "Created", default)]
    pub created: u64,
    #[serde(rename = "Size", default)]
    pub size: u64,
    #[serde(rename = "Flags", default)]
    pub flags: Vec<String>,
}

impl FwDevice {
    pub fn has_flag(&self, f: &str) -> bool {
        self.flags.iter().any(|x| x == f)
    }

    fn state(&self) -> u64 {
        match &self.update_state {
            Value::Number(n) => n.as_u64().unwrap_or(0),
            Value::String(s) => match s.as_str() {
                "pending" => 1,
                "success" => 2,
                "failed" | "failed-transient" => 3,
                "needs-reboot" => 4,
                _ => 0,
            },
            _ => 0,
        }
    }
}

/// Parses `fwupdmgr get-devices --json` or `get-updates --json`.
pub(crate) fn parse(json: &str) -> Vec<FwDevice> {
    serde_json::from_str::<Root>(json).map(|r| r.devices).unwrap_or_default()
}

/// Flash's device for a piece of firmware fwupd can update.
pub(crate) fn to_device(d: &FwDevice) -> Device {
    let problem = match d.state() {
        1 | 4 => devices::RESTART,
        3 => devices::FIRMWARE_FAILED,
        _ => 0,
    };
    Device {
        instance: format!("fwupd:{}", d.id),
        name: d.name.clone(),
        class: devices::firmware_class(&d.plugin, &d.name).to_string(),
        manufacturer: d.vendor.clone(),
        version: d.version.clone(),
        date: String::new(),
        provider: if d.vendor.is_empty() { "fwupd".into() } else { d.vendor.clone() },
        inf: format!("fwupd {}", d.plugin),
        signer: if d.has_flag("signed-payload") { "Signed firmware (LVFS)".into() } else { String::new() },
        problem,
        hwids: d.instance_ids.clone(),
        compat: d.guids.clone(),
    }
}

/// The firmware fwupd can update on this PC, as devices.
pub(crate) fn devices() -> Vec<Device> {
    if !has("fwupdmgr") {
        return vec![];
    }
    let json = cmd("fwupdmgr", &["get-devices", "--json"]).unwrap_or_default();
    parse(&json).iter().filter(|d| d.has_flag("updatable") && !d.version.is_empty() && is_id(&d.id)).map(to_device).collect()
}

/// fwupd device IDs are SHA-1 hashes.
pub(crate) fn is_id(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Firmware versions as fwupd prints them.
pub(crate) fn is_version(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// The newest offered release for each device that has one.
pub(crate) fn to_updates(fw: &[FwDevice], devices: &[Device]) -> Vec<Update> {
    let mut out = Vec::new();
    for d in fw.iter().filter(|d| is_id(&d.id)) {
        // Newer fwupd marks upgrades; older versions list only newer releases here.
        let Some(r) = d.releases.iter().find(|r| r.flags.iter().any(|f| f == "is-upgrade")).or_else(|| d.releases.iter().find(|r| r.version != d.version)) else { continue };
        if !is_version(&r.version) {
            continue;
        }
        let instance = format!("fwupd:{}", d.id);
        let title = if r.name.is_empty() { format!("{} firmware", d.name) } else { format!("{} {}", r.name, r.version) };
        out.push(Update {
            source: Source::Catalog,
            key: format!("fw:{}", d.id),
            title,
            version: r.version.clone(),
            date: if r.created > 0 { crate::extras::date(r.created) } else { String::new() },
            size: if r.size > 0 { crate::storage::human(r.size) } else { String::new() },
            device: devices.iter().any(|x| x.instance == instance).then_some(instance),
            hwid: d.instance_ids.first().cloned().unwrap_or_default(),
            vendor_swap: false,
            installed: d.version.clone(),
        });
    }
    out
}

/// Updates from the LVFS. Refreshes the metadata first (fwupd allows that without a password).
pub(crate) fn updates(devices: &[Device]) -> Result<Vec<Update>, String> {
    if !has("fwupdmgr") {
        return Err("fwupd isn't installed".into());
    }
    cmd_any("fwupdmgr", &["refresh", "--no-unreported-check"]);
    let (out, code) = cmd_any("fwupdmgr", &["get-updates", "--json", "--no-unreported-check", "--no-metadata-check"]).ok_or("couldn't run fwupdmgr")?;
    if out.trim_start().starts_with('{') {
        Ok(to_updates(&parse(&out), devices))
    } else if code == 2 {
        Ok(vec![]) // "No updates available"
    } else {
        Err(out.lines().last().unwrap_or("fwupd didn't answer").trim().to_string())
    }
}

/// The firmware version fwupd reports for one device now.
pub(crate) fn version_of(id: &str) -> Option<String> {
    let json = cmd("fwupdmgr", &["get-devices", "--json"]).ok()?;
    parse(&json).into_iter().find(|d| d.id == id).map(|d| d.version)
}

/// Installs the newest firmware for a device. fwupd asks for a password if its rules need one.
pub(crate) fn update(id: &str) -> Result<String, String> {
    if !is_id(id) {
        return Err(format!("bad fwupd device ID: {id}"));
    }
    cmd("fwupdmgr", &["update", id, "-y", "--no-reboot-check", "--no-unreported-check"])
}

/// Puts a device back on an older firmware version from the LVFS.
pub(crate) fn install_version(id: &str, version: &str) -> Result<String, String> {
    if !is_id(id) || !is_version(version) {
        return Err("bad firmware version request".into());
    }
    cmd("fwupdmgr", &["install", id, version, "-y", "--allow-older", "--allow-reinstall", "--no-reboot-check", "--no-unreported-check"])
}

/// Installs a firmware archive (.cab) from a folder.
pub(crate) fn install_cab(path: &str) -> Result<String, String> {
    cmd("fwupdmgr", &["install", path, "-y", "--no-reboot-check", "--no-unreported-check"])
}

/// What a .cab offers for this PC: (device ID, release version), if it fits a device.
pub(crate) fn cab_details(path: &str) -> Option<(FwDevice, FwRelease)> {
    let json = cmd("fwupdmgr", &["get-details", path, "--json"]).ok()?;
    let fw = parse(&json);
    fw.into_iter().find_map(|d| {
        let r = d.releases.first().cloned()?;
        (is_id(&d.id) && is_version(&r.version)).then_some((d, r))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICES: &str = include_str!("../../../tests/fixtures/linux/fwupd-devices.json");

    #[test]
    fn parses_real_fwupd_output() {
        let fw = parse(DEVICES);
        assert!(fw.len() >= 15);
        let nvme = fw.iter().find(|d| d.name == "SSD 990 PRO with Heatsink 1TB").unwrap();
        assert_eq!((nvme.version.as_str(), nvme.plugin.as_str()), ("5B2QJXD7", "nvme"));
        assert!(nvme.has_flag("updatable"));
        let system = fw.iter().find(|d| d.name == "System Firmware").unwrap();
        assert_eq!(system.state(), 2);
        let devs: Vec<Device> = fw.iter().filter(|d| d.has_flag("updatable") && !d.version.is_empty()).map(to_device).collect();
        assert!(devs.iter().any(|d| d.class == "System firmware" && crate::purpose(&d.class) == "Chipset & system"));
        assert!(devs.iter().any(|d| d.class == "NVMe firmware" && crate::purpose(&d.class) == "Storage"));
        assert!(devs.iter().any(|d| d.class == "Secure Boot keys" && crate::purpose(&d.class) == "Security"));
        assert!(devs.iter().all(|d| d.problem == 0));
        // "UEFI CA" lists its current release (2023), which isn't an update.
        assert!(to_updates(&fw, &devs).is_empty());
    }

    #[test]
    fn offers_the_upgrade_release() {
        let mut fw = parse(DEVICES);
        let ca = fw.iter_mut().find(|d| d.name == "UEFI CA").unwrap();
        ca.version = "2011".into();
        let devs: Vec<Device> = fw.iter().filter(|d| d.has_flag("updatable") && !d.version.is_empty()).map(to_device).collect();
        let ups = to_updates(&fw, &devs);
        assert_eq!(ups.len(), 1);
        let u = &ups[0];
        assert_eq!((u.version.as_str(), u.installed.as_str()), ("2023", "2011"));
        assert_eq!(u.title, "Secure Boot Signature Database 2023");
        assert_eq!(u.date, "2025-04-29");
        assert!(u.key.starts_with("fw:") && is_id(&u.key[3..]));
        assert_eq!(u.device.as_deref(), Some(&*format!("fwupd:{}", &u.key[3..])));
        assert!(!is_version("--force") && is_version("1.2.3-4"));
    }
}
