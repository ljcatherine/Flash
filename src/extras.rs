//! Saved state (hidden updates, install history), the weekly scheduled scan, driver
//! rollback, startup apps, BIOS and GPU driver checks, and the health report.
use crate::{Device, Update, health, whea};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One driver change Flash made, kept so it can be undone.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub time: u64, // unix seconds
    pub device: String,
    pub instance: String,
    pub old_version: String,
    pub new_version: String,
    pub old_inf: String,
    pub new_inf: String,
    pub backup: String, // folder holding the exported old driver, if any
    pub undone: bool,
    /// Linux: how to undo it ("pkg:<name>=<old version>", "fw:<device id>=<old version>" or
    /// "mod:<module>"). Empty on Windows, which undoes with `backup` and `new_inf`.
    #[serde(default)]
    pub undo: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub hidden: Vec<String>,
    pub history: Vec<Change>,
}

pub fn data_dir() -> PathBuf {
    crate::platform::data_dir()
}

pub fn load() -> State {
    std::fs::read(data_dir().join("state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save(state: &State) {
    let dir = data_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        if let Ok(json) = serde_json::to_vec_pretty(state) {
            std::fs::write(dir.join("state.json"), json).ok();
        }
    }
}

/// What "hide this update" remembers: the device and the version offered.
pub fn hide_key(u: &Update) -> String {
    format!("{}|{}", u.device.as_deref().unwrap_or(&u.hwid), u.version)
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Unix seconds to yyyy-mm-dd (UTC), using Howard Hinnant's civil-from-days.
pub fn date(secs: u64) -> String {
    let z = (secs / 86400) as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{m:02}-{d:02}", yoe + era * 400 + i64::from(m <= 2))
}

pub fn is_oem(inf: &str) -> bool {
    let l = inf.to_ascii_lowercase();
    l.len() > 7 && l.starts_with("oem") && l.ends_with(".inf") && l[3..l.len() - 4].chars().all(|c| c.is_ascii_digit())
}

// ---------- 1. Weekly scheduled scan ----------

pub fn schedule_enabled() -> bool {
    crate::platform::schedule_enabled()
}

/// Turns the weekly background scan (Sundays at noon) on or off: Task Scheduler on
/// Windows, a systemd user timer on Linux.
pub fn set_schedule(on: bool) -> Result<(), String> {
    crate::platform::set_schedule(on)
}

// ---------- 2. Rollback history ----------

/// Whether Flash can undo this change itself.
pub fn can_undo(c: &Change) -> bool {
    crate::platform::can_undo(c)
}

// ---------- 6. Startup apps ----------

#[derive(Clone, Debug, PartialEq)]
pub struct StartupApp {
    pub name: String,
    pub command: String,
    pub location: String,
    pub enabled: bool,
    pub delay_ms: Option<u64>, // worst boot slowdown the system recorded for it
}

/// Programs that start when you sign in, with whether each is on.
pub fn startup_apps() -> Vec<StartupApp> {
    crate::platform::startup_apps()
}

/// Turns a startup app on or off the way the system's own startup settings do.
pub fn set_startup(app: &StartupApp, enabled: bool) -> Result<(), String> {
    crate::platform::set_startup(app, enabled)
}

// ---------- 7. BIOS ----------

/// Where to get the newest BIOS for this motherboard.
pub fn bios_page(board_maker: &str, model: &str) -> String {
    let m = model.replace(' ', "%20");
    let maker = board_maker.to_lowercase();
    if maker.contains("asus") {
        format!("https://www.asus.com/supportonly/{m}/helpdesk_bios/")
    } else if maker.contains("micro-star") || maker.contains("msi") {
        format!("https://www.msi.com/search/{m}")
    } else if maker.contains("gigabyte") {
        format!("https://www.gigabyte.com/Search?kw={m}")
    } else if maker.contains("asrock") {
        format!("https://www.asrock.com/support/index.asp?cat=BIOS&Model={m}")
    } else {
        format!("https://www.bing.com/search?q={}%20{m}%20BIOS%20download", board_maker.replace(' ', "%20"))
    }
}

// ---------- 9. GPU driver ----------

/// NVIDIA's driver number from the Windows version: 32.0.16.1714 -> 617.14.
pub fn nvidia_version(windows: &str) -> Option<String> {
    let p: Vec<&str> = windows.split('.').collect();
    let digits = format!("{}{}", p.get(2)?, p.get(3)?);
    let last5 = digits.get(digits.len().checked_sub(5)?..)?;
    Some(format!("{}.{}", &last5[..3], &last5[3..]))
}

/// Latest GeForce Game Ready driver: (version, date, download URL). One driver covers all
/// current GeForce cards, so any recent card's lookup gives the answer.
pub fn latest_nvidia() -> Option<(String, String, String)> {
    let url = "https://gfwsl.geforce.com/services_toolkit/services/com/nvidia/services/AjaxDriverService.php?func=DriverManualLookup&psid=120&pfid=929&osID=57&languageCode=1033&beta=0&isWHQL=1&dltype=-1&dch=1&upCRD=0&qnf=0&sort1=0&numberOfResults=1";
    let body: serde_json::Value = serde_json::from_reader(crate::catalog::agent().get(url).call().ok()?.into_reader()).ok()?;
    let info = &body["IDS"][0]["downloadInfo"];
    let dl = info["DownloadURL"].as_str()?.to_string();
    dl.starts_with("https://us.download.nvidia.com/").then(|| (info["Version"].as_str().unwrap_or("").to_string(), info["ReleaseDateTime"].as_str().unwrap_or("").to_string(), dl))
}

pub fn newer(a: &str, b: &str) -> bool {
    crate::inf::version_parts(a) > crate::inf::version_parts(b)
}

// ---------- 8. Health report ----------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A self-contained HTML page summarising this PC, for sharing when asking for help.
pub fn report_html(r: &health::Report, devices: &[Device], updates: &[Update], bench: &[(String, String)]) -> String {
    let mut h = String::from("<!doctype html><meta charset=utf-8><title>Flash health report</title><style>body{font:15px system-ui;max-width:900px;margin:32px auto;padding:0 16px;color:#111}h1{margin-bottom:0}table{border-collapse:collapse;width:100%;margin:8px 0 20px}td,th{border:1px solid #ddd;padding:6px 8px;text-align:left;vertical-align:top}th{background:#f3f5f8}.bad{color:#b00020}</style>");
    h += &format!("<h1>Flash health report</h1><p>Created {} by Flash {}</p><h2>This PC</h2><table>", date(now()), env!("CARGO_PKG_VERSION"));
    for (k, v) in &r.system {
        h += &format!("<tr><th>{}</th><td>{}</td></tr>", esc(k), esc(v));
    }
    for (name, drv) in &r.gpus {
        h += &format!("<tr><th>Graphics</th><td>{} (driver {})</td></tr>", esc(name), esc(drv));
    }
    h += "</table><h2>Drives</h2><table><tr><th>Drive</th><th>Type</th><th>Temp</th><th>Life used</th><th>Status</th></tr>";
    for d in &r.disks {
        let (ok, text) = health::disk_verdict(d);
        h += &format!("<tr><td>{}</td><td>{} {}</td><td>{}</td><td>{}</td><td class={}>{}</td></tr>", esc(&d.name), esc(&d.bus), esc(&d.media), d.temp_c.map_or("-".into(), |t| format!("{t} °C")), d.wear_pct.map_or("-".into(), |w| format!("{w}%")), if ok { "ok" } else { "bad" }, esc(&text));
    }
    h += &format!("</table><h2>Last 30 days</h2><p>{} hardware errors, {} {}, {} unexpected shutdowns.</p>", r.hardware_errors, r.crashes, crate::os!("blue screens", "kernel panics"), r.unexpected_shutdowns);
    if !r.whea.is_empty() {
        let ctx = r.whea_context();
        h += "<h2>Hardware errors (90 days)</h2>";
        for g in &r.whea {
            let a = whea::explain(g, &ctx);
            let event = if g.id == 0 { String::new() } else { format!("Event {} · ", g.id) };
            h += &format!("<h3>{} · {} times</h3><p>{event}{} · {} · {} · {} to {}</p><p>{}</p><ol>", esc(&a.title), g.count, esc(&g.component), esc(&g.source), esc(&g.error_type), esc(&g.first), esc(&g.last), esc(&a.meaning));
            for f in &a.fixes {
                h += &format!("<li>{}</li>", esc(f));
            }
            h += "</ol>";
        }
    }
    let problems: Vec<&Device> = devices.iter().filter(|d| d.problem != 0).collect();
    h += &format!("<h2>Device problems ({})</h2><table><tr><th>Device</th><th>Problem</th></tr>", problems.len());
    for d in problems {
        h += &format!("<tr><td>{}</td><td>{}</td></tr>", esc(&d.name), crate::problem_text(d.problem));
    }
    h += &format!("</table><h2>Driver updates available ({})</h2><table><tr><th>Update</th><th>Version</th><th>Source</th></tr>", updates.len());
    for u in updates {
        h += &format!("<tr><td>{}</td><td>{}</td><td>{}</td></tr>", esc(&u.title), esc(&u.version), u.source.name());
    }
    h += "</table>";
    if !bench.is_empty() {
        h += "<h2>Benchmarks</h2><table>";
        for (k, v) in bench {
            h += &format!("<tr><th>{}</th><td>{}</td></tr>", esc(k), esc(v));
        }
        h += "</table>";
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_795_000_000), "2026-11-18");
        assert_eq!(nvidia_version("32.0.16.1714").as_deref(), Some("617.14"));
        assert_eq!(nvidia_version("32.0.15.6094").as_deref(), Some("560.94"));
        assert!(newer("621.24", "617.14") && !newer("617.14", "617.14"));
        assert!(is_oem("oem12.inf") && !is_oem("usbccgp.inf"));
        assert!(bios_page("ASUSTeK COMPUTER INC.", "PRIME B550M-A WIFI II").contains("PRIME%20B550M-A%20WIFI%20II/helpdesk_bios"));
        assert_eq!(esc("<a&\">"), "&lt;a&amp;&quot;&gt;");
        let u = Update { source: crate::Source::Catalog, key: "k".into(), title: "t".into(), version: "1.2".into(), date: String::new(), size: String::new(), device: Some("PCI\\X".into()), hwid: "h".into(), vendor_swap: false, installed: String::new() };
        assert_eq!(hide_key(&u), "PCI\\X|1.2");
        let html = report_html(&health::Report::default(), &[], &[u], &[]);
        assert!(html.contains("Driver updates available (1)"));
    }
}
