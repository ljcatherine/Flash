//! Hardware health on Linux: the system summary (/proc, DMI), drive health from udisks2
//! (smartctl through the helper when udisks2 isn't there), battery wear from power_supply,
//! and hardware errors from the kernel log: machine checks, PCIe AER, EDAC and APEI, fed to
//! the same knowledge base as Windows' WHEA events.
use super::{cmd, cmd_any, has, helper, helper_read, read};
use crate::health::{Battery, DiskHealth, Report};
use crate::whea::Event;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------- Kernel log ----------------

/// The kernel log of the last 90 days, kept for a minute so one Health refresh reads it once.
static LOG: Mutex<Option<(Instant, String)>> = Mutex::new(None);

fn kernel_log() -> String {
    let mut cache = LOG.lock().unwrap();
    if let Some((at, text)) = cache.as_ref() {
        if at.elapsed() < Duration::from_secs(60) {
            return text.clone();
        }
    }
    // Members of wheel/adm/systemd-journal can read it directly; others go through the helper.
    let text = match cmd("journalctl", &["-k", "--since", "-90d", "-o", "short-iso", "--no-pager", "-q"]) {
        Ok(t) if t.lines().count() > 3 => t,
        _ => helper("logs", "kernel-log", &["90"]).unwrap_or_default(),
    };
    *cache = Some((Instant::now(), text.clone()));
    text
}

/// "2026-11-21T23:10:45+11:00 host kernel: message" -> ("2026-11-21 23:10", "message").
fn split_line(line: &str) -> Option<(String, &str)> {
    let (stamp, rest) = line.split_once(' ')?;
    if stamp.len() < 16 || !stamp.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    let msg = rest.split_once("kernel: ").map(|(_, m)| m)?;
    Some((format!("{} {}", &stamp[..10], &stamp[11..16]), msg))
}

/// Architectural machine-check error codes (the low 16 bits of MCi_STATUS), named the way
/// WHEA names them, so the knowledge base reads both the same.
pub(crate) fn mca_error_type(status: u64) -> &'static str {
    let code = (status & 0xffff) as u16;
    match code {
        0x0001 => return "Unclassified Error",
        0x0002 => return "Microcode ROM Parity Error",
        0x0003 => return "External Error",
        0x0004 => return "FRC Error",
        0x0005 => return "Internal Parity Error",
        0x0400 => return "Internal Timer Error",
        0x0401..=0x07ff => return "Internal Unclassified Error",
        _ => {}
    }
    let c = code & !0x1000; // the "corrected error filtering" bit
    if c & 0xfff0 == 0x0010 {
        "Translation Lookaside Buffer Error"
    } else if c & 0xff80 == 0x0080 {
        "Memory Controller Error"
    } else if c & 0xff00 == 0x0100 {
        "Cache Hierarchy Error"
    } else if c & 0xf800 == 0x0800 {
        "Bus/Interconnect Error"
    } else {
        "Unknown Error"
    }
}

fn mce_event(time: &str, cpu: &str, bank: &str, status: u64, fatal: Option<bool>) -> Event {
    let uc = status >> 61 & 1 == 1;
    let fatal = fatal.unwrap_or(uc);
    let ty = mca_error_type(status);
    let component = if ty == "Memory Controller Error" { "Memory" } else { "Processor Core" };
    let fields = [
        ("component", component.to_string()),
        ("error source", if fatal { "Machine Check Exception" } else { "Corrected Machine Check" }.to_string()),
        ("error type", ty.to_string()),
        // Linux names the CPU thread by number; Flash shows it as "CPU n".
        ("processor apic id", cpu.to_string()),
        ("bank", bank.to_string()),
        ("status", format!("0x{status:016x}")),
    ];
    Event { time: time.to_string(), id: 0, fatal, fields: fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect() }
}

fn event(time: &str, fatal: bool, fields: &[(&str, String)]) -> Event {
    Event { time: time.to_string(), id: 0, fatal, fields: fields.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k.to_string(), v.clone())).collect() }
}

/// Hardware errors in a kernel log, newest first. `modalias` names the device at a PCI
/// address (so the Health page can show which device it was).
pub(crate) fn parse_hw_errors(log: &str, modalias: &dyn Fn(&str) -> String) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::new();
    let mut seen: HashSet<(String, String, u64)> = HashSet::new();
    let mut summaries: Vec<String> = Vec::new();
    let mut amd_severity: Option<bool> = None;
    let mut ghes: Option<(String, bool, String, String)> = None; // time, fatal, section, device
    let flush_ghes = |g: &mut Option<(String, bool, String, String)>, out: &mut Vec<Event>| {
        if let Some((time, fatal, section, device)) = g.take() {
            let s = section.to_lowercase();
            let (component, source) = if s.contains("pcie") {
                ("PCI Express Endpoint", "Advanced Error Reporting (PCI Express)")
            } else if s.contains("memory") {
                ("Memory", "APEI Generic Hardware Error Source")
            } else if s.contains("processor") {
                ("Processor Core", "APEI Generic Hardware Error Source")
            } else {
                ("", "APEI Generic Hardware Error Source")
            };
            out.push(event(&time, fatal, &[("component", component.into()), ("error source", source.into()), ("error type", section), ("bus:device:function", device.clone()), ("device name", if device.is_empty() { String::new() } else { modalias(&device) })]));
        }
    };
    for line in log.lines() {
        let Some((time, msg)) = split_line(line) else { continue };
        // APEI / GHES ("firmware first"): one error spans several lines.
        if msg.contains("[Hardware Error]: Hardware error from APEI") {
            flush_ghes(&mut ghes, &mut out);
            ghes = Some((time.clone(), false, String::new(), String::new()));
            continue;
        }
        if let Some(g) = ghes.as_mut() {
            if msg.starts_with('{') && msg.contains("[Hardware Error]:") {
                let detail = msg.split_once("[Hardware Error]:").map_or("", |(_, d)| d).trim();
                if let Some(sev) = detail.strip_prefix("event severity:") {
                    g.1 = !sev.trim().eq_ignore_ascii_case("corrected") && !sev.trim().eq_ignore_ascii_case("info");
                } else if let Some(sec) = detail.strip_prefix("section_type:") {
                    g.2 = sec.trim().to_string();
                } else if let Some(dev) = detail.strip_prefix("device_id:") {
                    g.3 = dev.trim().to_string();
                }
                continue;
            }
            flush_ghes(&mut ghes, &mut out);
        }
        if msg.contains("Machine check events logged") {
            summaries.push(time.clone());
            continue;
        }
        if msg.contains("[Hardware Error]: Corrected error") || msg.contains("[Hardware Error]: Deferred error") {
            amd_severity = Some(false);
            continue;
        }
        if msg.contains("[Hardware Error]: Uncorrected") {
            amd_severity = Some(true);
            continue;
        }
        // AMD's decoder: "[Hardware Error]: CPU:2 (19:21:2) MC5_STATUS[...]: 0x9c00000000070f0f"
        if let Some(rest) = msg.split_once("[Hardware Error]: CPU:").map(|(_, r)| r) {
            if let (Some(cpu), Some(bank), Some(status)) = (rest.split_whitespace().next(), rest.split_once(" MC").and_then(|(_, b)| b.split_once("_STATUS")).map(|(b, _)| b), rest.rsplit_once("]: 0x").map(|(_, s)| s.trim())) {
                if let Ok(status) = u64::from_str_radix(status, 16) {
                    if seen.insert((time.clone(), cpu.to_string(), status)) {
                        out.push(mce_event(&time, cpu, bank, status, amd_severity.take()));
                    }
                }
            }
            continue;
        }
        // Generic: "mce: [Hardware Error]: CPU 5: Machine Check: 0 Bank 1: 9c20000000080110"
        if msg.contains("Machine Check") && msg.contains(" Bank ") {
            let cpu = msg.split_once("CPU ").and_then(|(_, r)| r.split(':').next()).unwrap_or("").trim();
            let bank = msg.split_once(" Bank ").and_then(|(_, r)| r.split(':').next()).unwrap_or("").trim();
            let status = msg.rsplit_once(": ").map(|(_, s)| s.trim().trim_start_matches("0x")).and_then(|s| u64::from_str_radix(s, 16).ok());
            if let Some(status) = status {
                if seen.insert((time.clone(), cpu.to_string(), status)) {
                    out.push(mce_event(&time, cpu, bank, status, None));
                }
            }
            continue;
        }
        // PCIe AER, reported by the device itself: "<driver> <addr>: PCIe Bus Error: severity=..., type=..., (...)"
        if let Some((head, detail)) = msg.split_once(": PCIe Bus Error: ") {
            let mut parts = head.split_whitespace();
            let driver = parts.next().unwrap_or("");
            let addr = parts.next().unwrap_or("").to_string();
            let severity = detail.split_once("severity=").map_or("", |(_, r)| r.split(", type=").next().unwrap_or(""));
            let ty = detail.split_once("type=").map_or("", |(_, r)| r.split(',').next().unwrap_or(""));
            let component = if driver == "pcieport" { "PCI Express Root Port" } else { "PCI Express Endpoint" };
            let fatal = severity.to_lowercase().starts_with("uncorrect");
            out.push(event(&time, fatal, &[("component", component.into()), ("error source", "Advanced Error Reporting (PCI Express)".into()), ("error type", format!("{ty} ({severity})")), ("bus:device:function", addr.clone()), ("device name", modalias(&addr))]));
            continue;
        }
        // EDAC: "EDAC MC0: 2 CE <what> on <where> (...)"
        if let Some(rest) = msg.strip_prefix("EDAC MC") {
            let mut w = rest.split_whitespace();
            let _mc = w.next();
            let (Some(n), Some(kind)) = (w.next().and_then(|n| n.parse::<usize>().ok()), w.next()) else { continue };
            if kind != "CE" && kind != "UE" {
                continue;
            }
            let location = rest.split_once(" on ").map_or("", |(_, l)| l.split(" (").next().unwrap_or("")).trim().to_string();
            for _ in 0..n.min(1000) {
                out.push(event(&time, kind == "UE", &[("component", "Memory".into()), ("error source", "EDAC memory controller".into()), ("error type", if kind == "UE" { "Uncorrected memory error" } else { "Corrected memory error" }.into()), ("memory location", location.clone())]));
            }
        }
    }
    flush_ghes(&mut ghes, &mut out);
    // "Machine check events logged" with no details (the decoder wasn't loaded): still an error.
    for t in summaries {
        if !out.iter().any(|e| e.time == t) {
            out.push(mce_event(&t, "", "", 0, Some(false)));
        }
    }
    out.sort_by(|a, b| b.time.cmp(&a.time));
    out
}

/// Graphics driver resets: NVIDIA Xid errors, amdgpu GPU resets and ring timeouts,
/// i915/xe GPU hangs. Times "yyyy-mm-dd hh:mm".
pub(crate) fn gpu_resets(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(split_line)
        .filter(|(_, m)| m.contains("NVRM: Xid") || m.contains("GPU reset begin") || (m.contains("ring ") && m.contains("timeout") && m.contains("amdgpu")) || m.contains("GPU HANG") || (m.starts_with("xe ") && m.contains("GT") && m.contains("reset")))
        .map(|(t, _)| t)
        .collect()
}

/// The modalias of a PCI device, to tie an error to it.
fn pci_modalias(addr: &str) -> String {
    let valid = addr.len() <= 16 && addr.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
    if valid { read(format!("/sys/bus/pci/devices/{addr}/modalias")) } else { String::new() }
}

/// Errors counted by EDAC and AER since this boot that the kernel log doesn't have (it may
/// be kept only in memory, or already rotated).
fn live_counters(events: &mut Vec<Event>) {
    let boot = boot_time();
    let this_boot = |e: &&Event| e.time >= boot;
    let have_memory = events.iter().filter(this_boot).any(|e| e.get("component") == "Memory");
    if !have_memory {
        for mc in std::fs::read_dir("/sys/devices/system/edac/mc").into_iter().flatten().flatten() {
            for (file, fatal) in [("ce_count", false), ("ue_count", true)] {
                let n: usize = read(mc.path().join(file)).parse().unwrap_or(0);
                for _ in 0..n.min(1000) {
                    events.push(event(&boot, fatal, &[("component", "Memory".into()), ("error source", "EDAC memory controller (counted since boot)".into()), ("error type", if fatal { "Uncorrected memory error" } else { "Corrected memory error" }.into())]));
                }
            }
        }
    }
    let logged: HashSet<String> = events.iter().filter(this_boot).map(|e| e.get("bus:device:function").to_string()).collect();
    for dev in std::fs::read_dir("/sys/bus/pci/devices").into_iter().flatten().flatten() {
        let addr = dev.file_name().to_string_lossy().to_string();
        if logged.contains(&addr) {
            continue;
        }
        for (file, total, fatal) in [("aer_dev_correctable", "TOTAL_ERR_COR", false), ("aer_dev_nonfatal", "TOTAL_ERR_NONFATAL", true), ("aer_dev_fatal", "TOTAL_ERR_FATAL", true)] {
            let n = aer_total(&read(dev.path().join(file)), total);
            for _ in 0..n.min(500) {
                events.push(event(&boot, fatal, &[("component", "PCI Express Endpoint".into()), ("error source", "Advanced Error Reporting (PCI Express)".into()), ("error type", if fatal { "Uncorrected (counted since boot)" } else { "Corrected (counted since boot)" }.into()), ("bus:device:function", addr.clone()), ("device name", pci_modalias(&addr))]));
            }
        }
    }
}

/// The TOTAL_ERR_* line of an aer_dev_* counter file.
pub(crate) fn aer_total(text: &str, key: &str) -> usize {
    text.lines().find_map(|l| l.strip_prefix(key).map(|n| n.trim().parse().unwrap_or(0))).unwrap_or(0)
}

/// When this boot started, as "yyyy-mm-dd hh:mm" local time.
fn boot_time() -> String {
    let up: f64 = read("/proc/uptime").split_whitespace().next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let start = super::now().saturating_sub(up as u64);
    cmd("date", &["-d", &format!("@{start}"), "+%Y-%m-%d %H:%M"]).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// "yyyy-mm-dd hh:mm" for `days` ago, local time.
fn days_ago(days: u32) -> String {
    let t = super::now().saturating_sub(u64::from(days) * 86400);
    cmd("date", &["-d", &format!("@{t}"), "+%Y-%m-%d %H:%M"]).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Hardware error events from the last `days` days, newest first.
pub fn whea_read(days: u32) -> Vec<Event> {
    let log = kernel_log();
    let mut events = parse_hw_errors(&log, &pci_modalias);
    live_counters(&mut events);
    let since = days_ago(days);
    events.retain(|e| e.time >= since);
    events.sort_by(|a, b| b.time.cmp(&a.time));
    events
}

// ---------------- Crashes and shutdowns ----------------

/// Boots in the window that didn't end with "Journal stopped" (power loss, hang or panic).
/// `list` is `journalctl --list-boots -o json`; `stopped` holds the boot IDs that shut down cleanly.
pub(crate) fn unclean_boots(list: &str, stopped: &HashSet<String>, since_usec: u64) -> usize {
    let boots: Vec<Value> = serde_json::from_str(list).unwrap_or_default();
    boots
        .iter()
        .filter(|b| b["index"].as_i64().is_some_and(|i| i < 0)) // not the running boot
        .filter(|b| b["first_entry"].as_u64().is_some_and(|t| t >= since_usec))
        .filter(|b| b["boot_id"].as_str().is_some_and(|id| !stopped.contains(id)))
        .count()
}

pub(crate) fn stopped_boots(json_lines: &str) -> HashSet<String> {
    json_lines.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter_map(|v| v["_BOOT_ID"].as_str().map(String::from)).collect()
}

fn unexpected_shutdowns(days: u32) -> u32 {
    let Ok(list) = cmd("journalctl", &["--list-boots", "-o", "json", "--no-pager", "-q"]) else { return 0 };
    let since = format!("-{days}d");
    let stopped = cmd_any("journalctl", &["--since", &since, "-o", "json", "--output-fields=_BOOT_ID", "--no-pager", "-q", "SYSLOG_IDENTIFIER=systemd-journald", "MESSAGE=Journal stopped"]).map(|(o, _)| o).unwrap_or_default();
    let since_usec = super::now().saturating_sub(u64::from(days) * 86400) * 1_000_000;
    unclean_boots(&list, &stopped_boots(&stopped), since_usec) as u32
}

/// Kernel panics in pstore (the helper lists file times and whether each is a panic).
pub(crate) fn panics(pstore: &str, since: u64) -> u32 {
    let mut times: Vec<u64> = pstore
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() == 3 && f[2] == "1").then(|| f[0].parse().ok()).flatten()
        })
        .filter(|t| *t >= since)
        .collect();
    // One crash writes several dmesg-* parts within a few seconds.
    times.sort_unstable();
    times.dedup_by(|a, b| a.abs_diff(*b) < 120);
    times.len() as u32
}

// ---------------- Drives ----------------

fn data<'a>(props: &'a Value, key: &str) -> &'a Value {
    &props[key]["data"]
}

/// Drive health from udisks2's `GetManagedObjects` (busctl --json). `nvme_attrs` returns
/// `SmartGetAttributes` for an NVMe drive's object path.
pub(crate) fn parse_udisks(json: &str, nvme_attrs: &dyn Fn(&str) -> Value) -> Vec<DiskHealth> {
    let root: Value = serde_json::from_str(json).unwrap_or_default();
    let Some(objects) = root["data"][0].as_object() else { return vec![] };
    let mut out = Vec::new();
    for (path, ifs) in objects {
        let drive = &ifs["org.freedesktop.UDisks2.Drive"];
        if !drive.is_object() {
            continue;
        }
        let size = data(drive, "Size").as_u64().unwrap_or(0);
        let optical = data(drive, "MediaCompatibility").as_array().is_some_and(|m| m.iter().any(|x| x.as_str().is_some_and(|s| s.starts_with("optical"))));
        if size == 0 || optical {
            continue;
        }
        let vendor = data(drive, "Vendor").as_str().unwrap_or("").trim();
        let model = data(drive, "Model").as_str().unwrap_or("").trim();
        let name = if vendor.is_empty() || model.starts_with(vendor) { model.to_string() } else { format!("{vendor} {model}") };
        let rotation = data(drive, "RotationRate").as_i64().unwrap_or(-1);
        let media = match rotation {
            0 => "SSD",
            r if r > 0 => "HDD",
            _ => "Unspecified",
        };
        let nvme = &ifs["org.freedesktop.UDisks2.NVMe.Controller"];
        let ata = &ifs["org.freedesktop.UDisks2.Drive.Ata"];
        let bus = if nvme.is_object() {
            "NVMe"
        } else if ata.is_object() {
            "SATA"
        } else {
            match data(drive, "ConnectionBus").as_str().unwrap_or("") {
                "usb" => "USB",
                "sdio" => "SD",
                "" => "",
                _ => "Other",
            }
        };
        let kelvin = |v: &Value| v.as_f64().filter(|k| *k > 200.0).map(|k| (k - 273.15).round() as u32);
        let mut d = DiskHealth { name, media: media.into(), bus: bus.into(), size, ..Default::default() };
        if nvme.is_object() {
            let warnings = data(nvme, "SmartCriticalWarning").as_array().map_or(0, Vec::len);
            d.status = if warnings > 0 { "Warning".into() } else { "Healthy".into() };
            d.temp_c = kelvin(data(nvme, "SmartTemperature"));
            d.hours = data(nvme, "SmartPowerOnHours").as_u64().filter(|h| *h > 0);
            let attrs = nvme_attrs(path);
            let a = &attrs["data"][0];
            d.wear_pct = a["percent_used"]["data"].as_u64().map(|p| p as u32);
            d.read_errors = a["media_errors"]["data"].as_u64();
        } else if ata.is_object() && data(ata, "SmartSupported").as_bool() == Some(true) {
            d.status = if data(ata, "SmartFailing").as_bool() == Some(true) { "Unhealthy".into() } else { "Healthy".into() };
            d.temp_c = kelvin(data(ata, "SmartTemperature"));
            d.hours = data(ata, "SmartPowerOnSeconds").as_u64().map(|s| s / 3600).filter(|h| *h > 0);
            d.read_errors = data(ata, "SmartNumBadSectors").as_i64().filter(|n| *n >= 0).map(|n| n as u64);
        }
        out.push(d);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// One drive from `smartctl -j -a` (the helper runs it when udisks2 isn't available).
pub(crate) fn parse_smartctl(json: &str) -> Option<DiskHealth> {
    let v: Value = serde_json::from_str(json).ok()?;
    let name = v["model_name"].as_str().or(v["model_family"].as_str())?.to_string();
    let nvme = v["device"]["type"].as_str() == Some("nvme");
    let rotation = v["rotation_rate"].as_u64();
    let log = &v["nvme_smart_health_information_log"];
    Some(DiskHealth {
        name,
        media: if nvme || rotation == Some(0) { "SSD".into() } else if rotation.is_some() { "HDD".into() } else { "Unspecified".into() },
        bus: if nvme { "NVMe".into() } else { v["device"]["protocol"].as_str().unwrap_or("").replace("ATA", "SATA") },
        size: v["user_capacity"]["bytes"].as_u64().unwrap_or(0),
        status: match v["smart_status"]["passed"].as_bool() {
            Some(true) => "Healthy".into(),
            Some(false) => "Unhealthy".into(),
            None => String::new(),
        },
        temp_c: v["temperature"]["current"].as_u64().map(|t| t as u32).filter(|t| *t > 0),
        wear_pct: log["percentage_used"].as_u64().map(|p| p as u32),
        read_errors: log["media_errors"].as_u64(),
        hours: v["power_on_time"]["hours"].as_u64().filter(|h| *h > 0),
    })
}

fn disks() -> Vec<DiskHealth> {
    if has("busctl") {
        if let Ok(json) = cmd("busctl", &["--json=short", "call", "org.freedesktop.UDisks2", "/org/freedesktop/UDisks2", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects"]) {
            let attrs = |path: &str| -> Value {
                cmd("busctl", &["--json=short", "call", "org.freedesktop.UDisks2", path, "org.freedesktop.UDisks2.NVMe.Controller", "SmartGetAttributes", "a{sv}", "0"]).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
            };
            let list = parse_udisks(&json, &attrs);
            if !list.is_empty() {
                return list;
            }
        }
    }
    // No udisks2: ask smartctl through the helper (read-only, no password with the policy installed).
    let mut out = Vec::new();
    for e in std::fs::read_dir("/sys/block").into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !(name.starts_with("nvme") || name.starts_with("sd") || name.starts_with("vd")) || name.chars().any(|c| !c.is_ascii_alphanumeric()) {
            continue;
        }
        let size = read(e.path().join("size")).parse::<u64>().unwrap_or(0) * 512;
        let json = helper_read("smart", &[&format!("/dev/{name}")]).unwrap_or_default();
        let model = read(e.path().join("device/model"));
        out.push(parse_smartctl(&json).unwrap_or(DiskHealth { name: if model.is_empty() { name.clone() } else { model }, size, media: "Unspecified".into(), ..Default::default() }));
    }
    out
}

// ---------------- System summary ----------------

/// "Model, N cores / M threads" from /proc/cpuinfo.
pub(crate) fn cpu_summary(cpuinfo: &str) -> String {
    let mut model = String::new();
    let mut threads = 0;
    let mut cores: HashSet<(String, String)> = HashSet::new();
    let mut phys = String::new();
    for line in cpuinfo.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "processor" => threads += 1,
            "model name" if model.is_empty() => model = v.to_string(),
            "physical id" => phys = v.to_string(),
            "core id" => {
                cores.insert((phys.clone(), v.to_string()));
            }
            _ => {}
        }
    }
    let cores = if cores.is_empty() { threads } else { cores.len() };
    format!("{model}, {cores} cores / {threads} threads")
}

/// The first installed memory module from `dmidecode -t 17`: (modules, part number, MT/s).
pub(crate) fn dmi_memory(text: &str) -> (usize, String, u32) {
    let mut modules = 0;
    let mut part = String::new();
    let mut speed = 0;
    for block in text.split("\n\n").filter(|b| b.contains("Memory Device")) {
        let field = |key: &str| block.lines().find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_string())).unwrap_or_default();
        let size = field("Size:");
        if size.is_empty() || size.starts_with("No Module") || size == "Unknown" {
            continue;
        }
        modules += 1;
        if part.is_empty() {
            part = field("Part Number:");
            let s = [field("Configured Memory Speed:"), field("Configured Clock Speed:"), field("Speed:")].into_iter().find(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit())).unwrap_or_default();
            speed = s.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
        }
    }
    (modules, part, speed)
}

/// mm/dd/yyyy (DMI) to yyyy-mm-dd.
pub(crate) fn dmi_date(s: &str) -> String {
    let p: Vec<&str> = s.split('/').collect();
    if p.len() == 3 && p[2].len() == 4 { format!("{}-{}-{}", p[2], p[0], p[1]) } else { s.to_string() }
}

fn battery() -> Option<Battery> {
    for e in std::fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
        let p = e.path();
        if read(p.join("type")) != "Battery" || read(p.join("scope")) == "Device" {
            continue;
        }
        let n = |f: &str| read(p.join(f)).parse::<u64>().unwrap_or(0);
        let (design, full) = if n("energy_full_design") > 0 {
            (n("energy_full_design") / 1000, n("energy_full") / 1000) // µWh -> mWh
        } else {
            let v = n("voltage_min_design").max(1);
            (n("charge_full_design") * v / 1_000_000_000, n("charge_full") * v / 1_000_000_000) // µAh x µV -> mWh
        };
        if design > 0 {
            return Some(Battery { design_mwh: design, full_mwh: full, charge_pct: n("capacity") as u32 });
        }
    }
    None
}

/// "Vendor model" for a GPU: the marketing name in pci.ids' brackets, with a short vendor.
pub(crate) fn gpu_name(vendor: &str, model: &str) -> String {
    let short = if vendor.contains("NVIDIA") {
        "NVIDIA"
    } else if vendor.contains("AMD") || vendor.contains("ATI") {
        "AMD"
    } else if vendor.contains("Intel") {
        "Intel"
    } else {
        vendor
    };
    let marketing = model.split_once('[').and_then(|(_, r)| r.split_once(']')).map_or(model, |(m, _)| m);
    format!("{short} {}", marketing.trim())
}

fn gpus() -> Vec<(String, String)> {
    let db = cmd("udevadm", &["info", "--export-db"]).unwrap_or_default();
    let kernel = read("/proc/sys/kernel/osrelease");
    let mesa = super::packages::installed_any(&["mesa-dri-drivers", "libgl1-mesa-dri", "mesa", "Mesa-dri"]).map(|v| {
        let v = v.split_once(':').map_or(v.as_str(), |(_, r)| r);
        v.split(['-', '+', '~']).next().unwrap_or(v).to_string()
    });
    let smi = cmd("nvidia-smi", &["--query-gpu=name", "--format=csv,noheader"]).unwrap_or_default();
    let mut nv_names = smi.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty());
    let mut out = Vec::new();
    for e in super::devices::parse_udev_db(&db).iter().filter(|e| e.get("SUBSYSTEM") == "pci" && e.get("PCI_CLASS").len() >= 5 && e.get("PCI_CLASS").starts_with('3')) {
        let driver = e.get("DRIVER");
        let mut name = gpu_name(e.get("ID_VENDOR_FROM_DATABASE"), e.get("ID_MODEL_FROM_DATABASE"));
        let version = if driver == "nvidia" {
            if let Some(n) = nv_names.next() {
                name = n;
            }
            read("/sys/module/nvidia/version")
        } else {
            match &mesa {
                Some(m) => format!("Mesa {m}, kernel {kernel}"),
                None => format!("{} in kernel {kernel}", if driver.is_empty() { "no driver" } else { driver }),
            }
        };
        out.push((name, version));
    }
    out
}

/// Reads everything except the hardware error groups.
pub fn health_report() -> Result<Report, String> {
    let mut r = Report::default();
    let dmi = |f: &str| read(format!("/sys/class/dmi/id/{f}"));
    let os = super::os_release();
    let kernel = read("/proc/sys/kernel/osrelease");
    let mem_kb: u64 = read("/proc/meminfo").lines().find_map(|l| l.strip_prefix("MemTotal:")).and_then(|v| v.split_whitespace().next()?.parse().ok()).unwrap_or(0);
    let (modules, part, speed) = helper_read("dmi-memory", &[]).map(|t| dmi_memory(&t)).unwrap_or_default();
    r.ram_part = part;
    r.ram_running = speed;
    let up = read("/proc/uptime").split_whitespace().next().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) as u64;
    let memory = if modules > 0 && speed > 0 { format!("{} GB, {modules} module(s) at {speed} MT/s", (mem_kb as f64 / 1048576.0).round()) } else { format!("{} GB", (mem_kb as f64 / 1048576.0).round()) };
    for (k, v) in [
        ("Computer", format!("{} {}", dmi("sys_vendor"), dmi("product_name"))),
        ("System", format!("{} (kernel {kernel})", os.get("PRETTY_NAME").cloned().unwrap_or_else(|| "Linux".into()))),
        ("Processor", cpu_summary(&std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default())),
        ("Memory", memory),
        ("Motherboard", format!("{} {}", dmi("board_vendor"), dmi("board_name"))),
        ("BIOS", format!("{} {}, {}", dmi("bios_vendor"), dmi("bios_version"), dmi_date(&dmi("bios_date")))),
        ("Uptime", format!("{}d {}h {}m", up / 86400, up / 3600 % 24, up / 60 % 60)),
    ] {
        if !v.trim().is_empty() && v.trim() != "," {
            r.system.push((k.into(), v.trim().trim_end_matches(',').to_string()));
        }
    }
    r.gpus = gpus();
    r.disks = disks();
    r.battery = battery();
    let since30 = days_ago(30);
    r.hardware_errors = whea_read(30).len() as u32;
    r.display_resets = gpu_resets(&kernel_log()).iter().filter(|t| **t >= since30).count() as u32;
    r.unexpected_shutdowns = unexpected_shutdowns(30);
    r.crashes = helper_read("pstore", &[]).map_or(0, |p| panics(&p, super::now().saturating_sub(30 * 86400)));
    if r.system.is_empty() { Err("No data returned".into()) } else { Ok(r) }
}

/// Linux has no single "latest driver" to look up: the Health page compares the GPU driver
/// with what the distribution offers on the Updates page instead.
pub fn gpu_latest(_: &Report) -> Option<(String, String, String)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whea::{Context, Cpu, Kind, classify, explain, group};

    const LOG: &str = include_str!("../../../tests/fixtures/linux/kernel-hardware-errors.log");
    const UDISKS: &str = include_str!("../../../tests/fixtures/linux/udisks-drives.json");
    const ATTRS: &str = include_str!("../../../tests/fixtures/linux/udisks-nvme-smart-attributes.json");

    #[test]
    fn decodes_machine_check_codes() {
        assert_eq!(mca_error_type(0x9c00000000070f0f), "Bus/Interconnect Error");
        assert_eq!(mca_error_type(0x9c20000000080110), "Cache Hierarchy Error");
        assert_eq!(mca_error_type(0xbe00000000000135), "Cache Hierarchy Error");
        assert_eq!(mca_error_type(0x0000000000000014), "Translation Lookaside Buffer Error");
        assert_eq!(mca_error_type(0x000000000000009f), "Memory Controller Error");
        assert_eq!(mca_error_type(0x0000000000000400), "Internal Timer Error");
        assert_eq!(mca_error_type(0x0000000000000405), "Internal Unclassified Error");
        assert_eq!(mca_error_type(0x0000000000001110), "Cache Hierarchy Error"); // filtering bit set
    }

    #[test]
    fn reads_hardware_errors_from_the_kernel_log() {
        let ev = parse_hw_errors(LOG, &|addr| format!("pci:{addr}"));
        let gs = group(&ev);
        let bus = gs.iter().find(|g| g.error_type == "Bus/Interconnect Error").unwrap();
        assert_eq!((bus.count, bus.fatal, bus.component.as_str()), (2, false, "Processor Core"));
        assert_eq!(bus.apic.iter().collect::<Vec<_>>(), ["2"]);
        assert_eq!((bus.first.as_str(), bus.last.as_str()), ("2026-11-09 17:54", "2026-11-12 09:00"));
        let amd = Context { cpu: Cpu::Amd, ram_overclocked: None, arch: crate::hwkb::CpuArch::AmdZen3 };
        assert_eq!(classify(bus), Kind::CpuBus);
        assert!(explain(bus, &amd).title.contains("Infinity Fabric"));
        let cache = gs.iter().find(|g| g.error_type == "Cache Hierarchy Error").unwrap();
        assert_eq!(cache.apic.iter().collect::<Vec<_>>(), ["5"]);
        let nvme = gs.iter().find(|g| g.bdf == "0000:01:00.0").unwrap();
        assert_eq!((nvme.fatal, nvme.component.as_str(), nvme.device.as_str()), (false, "PCI Express Endpoint", "pci:0000:01:00.0"));
        assert_eq!(classify(nvme), Kind::Pcie);
        let port = gs.iter().find(|g| g.bdf == "0000:00:03.1").unwrap();
        assert!(port.fatal && port.component == "PCI Express Root Port");
        let edac = gs.iter().find(|g| g.source.starts_with("EDAC")).unwrap();
        assert_eq!((edac.count, classify(edac)), (2, Kind::Memory));
        let ghes = gs.iter().find(|g| g.source.starts_with("APEI")).unwrap();
        assert_eq!((ghes.count, ghes.fatal, classify(ghes)), (1, false, Kind::Memory));
        // The summary-only line at 03:00 becomes a generic corrected machine check.
        assert!(ev.iter().any(|e| e.time == "2026-11-14 03:00" && e.get("component") == "Processor Core"));
        assert_eq!(ev.len(), 9);
        assert!(ev.windows(2).all(|w| w[0].time >= w[1].time)); // newest first
        assert_eq!(gpu_resets(LOG), ["2026-11-18 22:01", "2026-11-19 12:00"]);
        assert_eq!(aer_total("RxErr 0\nBadTLP 2\nTOTAL_ERR_COR 3\n", "TOTAL_ERR_COR"), 3);
    }

    #[test]
    fn reads_real_udisks_drive_health() {
        let attrs: Value = serde_json::from_str(ATTRS).unwrap();
        let disks = parse_udisks(UDISKS, &|p: &str| if p.contains("CT1000P2SSD8") { attrs.clone() } else { Value::Null });
        assert_eq!(disks.len(), 3);
        let ct = disks.iter().find(|d| d.name == "CT1000P2SSD8").unwrap();
        assert_eq!((ct.bus.as_str(), ct.media.as_str(), ct.status.as_str()), ("NVMe", "SSD", "Healthy"));
        assert_eq!(ct.wear_pct, Some(3));
        assert_eq!(ct.read_errors, Some(0));
        assert!(ct.hours.is_some_and(|h| h > 10_000));
        assert!(ct.temp_c.is_some_and(|t| (20..90).contains(&t)));
        let usb = disks.iter().find(|d| d.bus == "USB").unwrap();
        assert!(usb.status.is_empty() && crate::health::disk_verdict(usb).0);
    }

    #[test]
    fn system_summary_parsers() {
        assert_eq!(cpu_summary("processor\t: 0\nmodel name\t: AMD Ryzen 5 5600 6-Core Processor\nphysical id\t: 0\ncore id\t: 0\n\nprocessor\t: 1\nmodel name\t: AMD Ryzen 5 5600 6-Core Processor\nphysical id\t: 0\ncore id\t: 0\n\nprocessor\t: 2\nphysical id\t: 0\ncore id\t: 1\n"), "AMD Ryzen 5 5600 6-Core Processor, 2 cores / 3 threads");
        let dmi = "Handle 0x0010, DMI type 17, 92 bytes\nMemory Device\n\tSize: No Module Installed\n\nHandle 0x0011, DMI type 17, 92 bytes\nMemory Device\n\tSize: 16 GB\n\tSpeed: 3200 MT/s\n\tPart Number: CMK32GX4M2E3200C16  \n\tConfigured Memory Speed: 2133 MT/s\n\nHandle 0x0012, DMI type 17, 92 bytes\nMemory Device\n\tSize: 16 GB\n\tPart Number: CMK32GX4M2E3200C16\n";
        assert_eq!(dmi_memory(dmi), (2, "CMK32GX4M2E3200C16".to_string(), 2133));
        assert_eq!(dmi_date("09/10/2025"), "2025-09-10");
        assert_eq!(gpu_name("NVIDIA Corporation", "TU106 [GeForce RTX 2060 Rev. A]"), "NVIDIA GeForce RTX 2060 Rev. A");
        assert_eq!(gpu_name("Advanced Micro Devices, Inc. [AMD/ATI]", "Navi 32 [Radeon RX 7700 XT / 7800 XT]"), "AMD Radeon RX 7700 XT / 7800 XT");
        assert_eq!(crate::hwkb::gpu_kind(&gpu_name("Intel Corporation", "DG2 [Arc A770]")), crate::hwkb::GpuKind::IntelArc);
        assert_eq!(crate::hwkb::gpu_kind(&gpu_name("Intel Corporation", "Alder Lake-S GT1 [UHD Graphics 730]")), crate::hwkb::GpuKind::IntelIntegrated);
    }

    #[test]
    fn crashes_and_shutdowns() {
        let list = r#"[{"index":-2,"boot_id":"aaa","first_entry":1795000000000000,"last_entry":1795000100000000},{"index":-1,"boot_id":"bbb","first_entry":1795100000000000,"last_entry":1795100100000000},{"index":0,"boot_id":"ccc","first_entry":1795200000000000,"last_entry":1795200100000000}]"#;
        let stopped = stopped_boots("{\"_BOOT_ID\":\"aaa\"}\n");
        assert_eq!(unclean_boots(list, &stopped, 0), 1); // bbb ended without "Journal stopped"
        assert_eq!(unclean_boots(list, &stopped, 1795050000000000), 1);
        assert_eq!(unclean_boots(list, &stopped_boots("{\"_BOOT_ID\":\"aaa\"}\n{\"_BOOT_ID\":\"bbb\"}\n"), 0), 0);
        // Two parts of one crash, then another crash a day later; the old one is outside the window.
        assert_eq!(panics("1795000000\tdmesg-efi-1\t1\n1795000003\tdmesg-efi-2\t1\n1795086400\tdmesg-efi-3\t1\n1700000000\tdmesg-efi-4\t1\n1795090000\tdmesg-efi-5\t0\n", 1790000000), 2);
    }
}
