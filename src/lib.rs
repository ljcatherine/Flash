//! Device scanning and driver installation via WMI and the Windows Update Agent.
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::{Arc, Mutex};

const NO_WINDOW: u32 = 0x0800_0000; // CREATE_NO_WINDOW: no console flashes from the GUI

// Every device with an installed driver.
const SCAN: &str = r#"
Get-CimInstance Win32_PnPSignedDriver | Where-Object { $_.DeviceName -and $_.DriverVersion } | Sort-Object DeviceClass, DeviceName | ForEach-Object {
  "{0}`t{1}`t{2}`t{3}`t{4}" -f $_.DeviceClass, $_.DeviceName, $_.DriverVersion, $_.DriverProviderName, $_.HardWareID
}"#;

// Devices whose driver is missing (28) or broken (1, 10, 18).
const MISSING: &str = r#"
Get-CimInstance Win32_PnPEntity | Where-Object { $_.ConfigManagerErrorCode -in 1,10,18,28 } | ForEach-Object {
  $n = if ($_.Name) { $_.Name } else { $_.DeviceID }
  "{0}`t{1}`t{2}" -f $n, $_.ConfigManagerErrorCode, ($_.HardwareID | Select-Object -First 1)
}"#;

// Driver updates Windows Update offers for this PC (covers missing drivers too).
const WU_SEARCH: &str = r#"
$s = New-Object -ComObject Microsoft.Update.Session
$q = $s.CreateUpdateSearcher()
$q.ServerSelection = 2
$r = $q.Search("IsInstalled=0 and Type='Driver' and IsHidden=0")
"#;

const WU_LIST: &str = r#"
foreach ($u in $r.Updates) {
  "{0}`t{1}`t{2}`t{3}" -f $u.Title, $u.DriverVerDate.ToString('yyyy-MM-dd'), $u.DriverHardwareID, $u.Identity.UpdateID
}"#;

// Expects `$ids` (the UpdateIDs to install) to be defined before it.
const WU_INSTALL: &str = r#"
$c = New-Object -ComObject Microsoft.Update.UpdateColl
foreach ($u in $r.Updates) {
  if ($ids -notcontains $u.Identity.UpdateID) { continue }
  if (-not $u.EulaAccepted) { $u.AcceptEula() }; [void]$c.Add($u)
}
if ($c.Count -eq 0) { 'Nothing to install.'; exit }
"Downloading $($c.Count) driver(s)..."
$d = $s.CreateUpdateDownloader(); $d.Updates = $c; [void]$d.Download()
'Installing...'
$i = $s.CreateUpdateInstaller(); $i.Updates = $c; $res = $i.Install()
$codes = 'NotStarted','InProgress','Succeeded','SucceededWithErrors','Failed','Aborted'
for ($k = 0; $k -lt $c.Count; $k++) { '  [{0}] {1}' -f $codes[$res.GetUpdateResult($k).ResultCode], $c.Item($k).Title }
if ($res.RebootRequired) { 'A reboot is required to finish installing.' }
"#;

/// Runs a PowerShell script from a temp file (avoids command-line quoting issues).
/// Returns (stdout, stderr).
pub fn ps(script: &str) -> (String, String) {
    let path = std::env::temp_dir().join(format!("flash-{}.ps1", std::process::id()));
    if let Err(e) = std::fs::write(&path, format!("[Console]::OutputEncoding=[Text.Encoding]::UTF8\n{script}")) {
        return (String::new(), format!("cannot write temp script: {e}"));
    }
    let out = Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&path)
        .creation_flags(NO_WINDOW)
        .output();
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

#[derive(Default)]
pub struct Data {
    pub devices: Vec<Vec<String>>, // class, name, version, provider, hwid
    pub missing: Vec<Vec<String>>, // name, error code, hwid
    pub updates: Vec<Vec<String>>, // title, date, hwid, update id
    pub log: String,
    pub busy: bool,
    pub scans: u64, // bumped after each finished scan so the UI can react to fresh results
}

pub type Shared = Arc<Mutex<Data>>;

pub fn log(d: &Shared, s: &str) {
    let mut d = d.lock().unwrap();
    d.log.push_str(s.trim_end());
    d.log.push('\n');
}

/// Runs a PowerShell query, logging any errors, and returns its rows.
pub fn query(d: &Shared, script: &str, cols: usize) -> Vec<Vec<String>> {
    let (out, err) = ps(script);
    if !err.trim().is_empty() {
        log(d, &format!("Error: {err}"));
    }
    rows(&out, cols)
}

pub fn scan(d: &Shared) {
    log(d, "Rescanning hardware...");
    Command::new("pnputil").arg("/scan-devices").creation_flags(NO_WINDOW).output().ok();
    log(d, "Scanning devices...");
    let devices = query(d, SCAN, 5);
    let missing = query(d, MISSING, 3);
    log(d, &format!("{} devices with drivers, {} missing or broken.", devices.len(), missing.len()));
    {
        let mut g = d.lock().unwrap();
        g.devices = devices;
        g.missing = missing;
    }
    log(d, "Checking Windows Update for newer drivers (can take a minute)...");
    let updates = query(d, &format!("{WU_SEARCH}{WU_LIST}"), 4);
    log(d, &format!("{} driver update(s) available.", updates.len()));
    let mut g = d.lock().unwrap();
    g.updates = updates;
    g.scans += 1;
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

/// Installs the Windows Update drivers with these UpdateIDs, then rescans.
pub fn install(d: &Shared, ids: &[String]) {
    log(d, &format!("Downloading and installing {} driver update(s)...", ids.len()));
    let (out, err) = ps(&format!("{WU_SEARCH}$ids = {}\n{WU_INSTALL}", ps_ids(ids)));
    log(d, &out);
    if !err.trim().is_empty() {
        log(d, &format!("Error: {err}"));
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
        let ids = ["0a1b-2C".to_string(), "x'; rm".to_string(), String::new()];
        assert_eq!(ps_ids(&ids), "@('0a1b-2C')");
    }
}
