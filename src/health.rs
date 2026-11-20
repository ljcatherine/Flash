//! Hardware health: a system summary, per-drive reliability counters, battery wear,
//! and hardware error / crash events from the last 30 days.
use crate::{ps, rows};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiskHealth {
    pub name: String,
    pub media: String, // SSD / HDD / Unspecified
    pub bus: String,   // NVMe / SATA / USB ...
    pub size: u64,
    pub status: String, // Healthy / Warning / Unhealthy
    pub temp_c: Option<u32>,
    pub wear_pct: Option<u32>, // share of rated life used
    pub read_errors: Option<u64>,
    pub hours: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Battery {
    pub design_mwh: u64,
    pub full_mwh: u64,
    pub charge_pct: u32,
}

impl Battery {
    /// Capacity left compared with when it was new, in percent.
    pub fn health_pct(&self) -> u32 {
        if self.design_mwh == 0 { 0 } else { ((self.full_mwh * 100) / self.design_mwh).min(100) as u32 }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub system: Vec<(String, String)>,
    pub gpus: Vec<(String, String)>, // name, driver version
    pub disks: Vec<DiskHealth>,
    pub battery: Option<Battery>,
    pub hardware_errors: u32,      // WHEA-Logger events
    pub unexpected_shutdowns: u32, // Kernel-Power 41
    pub crashes: u32,              // blue screens (BugCheck 1001)
    pub ram_part: String,          // memory module part number
    pub ram_running: u32,          // MT/s the memory actually runs at
    pub whea: Vec<crate::whea::Group>, // hardware errors from the last 90 days, grouped
}

impl Report {
    /// What the WHEA advice needs to know about this PC.
    pub fn whea_context(&self) -> crate::whea::Context {
        let cpu = self.system.iter().find(|(k, _)| k == "Processor").map_or("", |(_, v)| v.as_str()).to_lowercase();
        let cpu = if cpu.contains("amd") || cpu.contains("ryzen") {
            crate::whea::Cpu::Amd
        } else if cpu.contains("intel") {
            crate::whea::Cpu::Intel
        } else {
            crate::whea::Cpu::Other
        };
        let ram_overclocked = rated_speed(&self.ram_part).filter(|_| self.ram_running > 0).map(|rated| self.ram_running + 200 >= rated);
        crate::whea::Context { cpu, ram_overclocked }
    }
}

const SCRIPT: &str = r#"
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
"EVT`t$whea`t$power`t$bsod"
"#;

/// A counter of 0 usually means the drive doesn't report it.
fn opt<T: std::str::FromStr + PartialEq + Default>(s: &str) -> Option<T> {
    s.trim().parse().ok().filter(|v| *v != T::default())
}

pub fn parse(out: &str) -> Report {
    let mut r = Report::default();
    for row in rows(out, 10) {
        match row[0].as_str() {
            "SYS" if !row[2].trim().is_empty() => r.system.push((row[1].clone(), row[2].trim().trim_end_matches(',').to_string())),
            "GPU" => r.gpus.push((row[1].clone(), row[2].clone())),
            "DISK" => r.disks.push(DiskHealth {
                name: row[1].clone(),
                media: row[2].clone(),
                bus: row[3].clone(),
                size: row[4].parse().unwrap_or(0),
                status: row[5].clone(),
                temp_c: opt(&row[6]),
                wear_pct: row[7].trim().parse().ok(),
                read_errors: row[8].trim().parse().ok(),
                hours: opt(&row[9]),
            }),
            "BAT" => {
                r.battery = Some(Battery { design_mwh: row[1].parse().unwrap_or(0), full_mwh: row[2].parse().unwrap_or(0), charge_pct: row[3].parse().unwrap_or(0) }).filter(|b| b.design_mwh > 0)
            }
            "RAM" => {
                r.ram_part = row[1].clone();
                r.ram_running = row[2].parse().unwrap_or(0);
            }
            "EVT" => {
                r.hardware_errors = row[1].parse().unwrap_or(0);
                r.unexpected_shutdowns = row[2].parse().unwrap_or(0);
                r.crashes = row[3].parse().unwrap_or(0);
            }
            _ => {}
        }
    }
    r
}

/// Reads everything; takes a few seconds (event log queries).
pub fn read() -> Result<Report, String> {
    let (out, err) = ps(SCRIPT);
    let mut r = parse(&out);
    r.whea = crate::whea::group(&crate::whea::read(90));
    if r.system.is_empty() { Err(if err.trim().is_empty() { "No data returned".into() } else { err }) } else { Ok(r) }
}

/// The speed a memory kit is sold at, read from its part number (e.g. Corsair
/// CMW16GX4M2E3200C16, G.Skill F4-3600C16D-16GVKC). Windows only reports the
/// running speed, which is the slower JEDEC default until XMP/EXPO is turned on.
/// shortcut: only finds speeds written as 4 digits; Kingston-style "KF432" codes aren't decoded.
pub fn rated_speed(part: &str) -> Option<u32> {
    const SPEEDS: [u32; 19] = [2400, 2666, 2933, 3000, 3200, 3333, 3466, 3600, 3733, 3866, 4000, 4133, 4400, 4800, 5200, 5600, 6000, 6400, 7200];
    let digits: Vec<(usize, u32)> = part
        .char_indices()
        .filter(|(i, _)| part[*i..].len() >= 4 && part[*i..*i + 4].chars().all(|c| c.is_ascii_digit()))
        .filter_map(|(i, _)| part[i..i + 4].parse().ok().map(|n| (i, n)))
        .collect();
    digits.into_iter().map(|(_, n)| n).find(|n| SPEEDS.contains(n))
}

/// Memory running well below its rated speed: (rated, running).
pub fn ram_slow(r: &Report) -> Option<(u32, u32)> {
    let rated = rated_speed(&r.ram_part)?;
    (r.ram_running > 0 && rated > r.ram_running + 200).then_some((rated, r.ram_running))
}

/// Plain verdict for a drive: (is_ok, words).
pub fn disk_verdict(d: &DiskHealth) -> (bool, String) {
    if !d.status.eq_ignore_ascii_case("healthy") && !d.status.is_empty() {
        return (false, format!("Windows reports: {}", d.status));
    }
    if d.read_errors.unwrap_or(0) > 0 {
        return (false, format!("{} uncorrected read errors", d.read_errors.unwrap_or(0)));
    }
    if d.wear_pct.unwrap_or(0) >= 90 {
        return (false, "Near the end of its rated life".into());
    }
    if d.temp_c.unwrap_or(0) >= 70 {
        return (false, "Running hot".into());
    }
    (true, "Healthy".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_report() {
        let out = "SYS\tComputer\tASUS ROG\nSYS\tBIOS\t\nGPU\tNVIDIA RTX\t32.0.15.6094\nDISK\tSamsung 980\tSSD\tNVMe\t1000204886016\tHealthy\t41\t3\t0\t5120\nBAT\t50000\t42000\t80\nEVT\t2\t1\t0\n";
        let r = parse(out);
        assert_eq!(r.system, vec![("Computer".to_string(), "ASUS ROG".to_string())]); // empty BIOS skipped
        assert_eq!(r.gpus[0].1, "32.0.15.6094");
        assert_eq!((r.disks[0].temp_c, r.disks[0].wear_pct, r.disks[0].hours), (Some(41), Some(3), Some(5120)));
        assert_eq!(r.battery.as_ref().unwrap().health_pct(), 84);
        assert_eq!((r.hardware_errors, r.unexpected_shutdowns, r.crashes), (2, 1, 0));
        assert!(disk_verdict(&r.disks[0]).0);
        assert_eq!(rated_speed("CMW16GX4M2E3200C16"), Some(3200));
        assert_eq!(rated_speed("F4-3600C16D-16GVKC"), Some(3600));
        assert_eq!(rated_speed("M378A1K43CB2-CTD"), None);
        let slow = Report { ram_part: "CMW16GX4M2E3200C16".into(), ram_running: 2133, ..Default::default() };
        assert_eq!(ram_slow(&slow), Some((3200, 2133)));
        assert_eq!(slow.whea_context().ram_overclocked, Some(false));
        let amd = Report { system: vec![("Processor".into(), "AMD Ryzen 5 5600".into())], ..Default::default() };
        assert_eq!(amd.whea_context().cpu, crate::whea::Cpu::Amd);
        assert_eq!(ram_slow(&Report { ram_part: "CMW16GX4M2E3200C16".into(), ram_running: 3200, ..Default::default() }), None);
        let bad = DiskHealth { status: "Healthy".into(), read_errors: Some(4), ..Default::default() };
        assert!(!disk_verdict(&bad).0);
    }
}
