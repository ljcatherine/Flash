//! Live GPU use and sensors on Linux. GPU use per app comes from DRM fdinfo (amdgpu, i915,
//! xe, nouveau...) and `nvidia-smi pmon` for NVIDIA's driver; the total from amdgpu's
//! gpu_busy_percent, nvidia-smi, or the busiest engine across all apps. Temperatures and
//! fans come from every hwmon sensor, plus nvidia-smi.
use super::{has, read};
use crate::monitor::{Monitor, alive};
use std::collections::HashMap;
use std::io::BufRead;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One DRM client's busy time per engine (nanoseconds), from /proc/<pid>/fdinfo/<fd>.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct DrmClient {
    pub id: String,
    pub engines: HashMap<String, (u64, u64)>, // engine -> (busy ns, capacity)
}

/// Parses one fdinfo file; None unless it's a DRM client with engine counters.
pub(crate) fn parse_fdinfo(text: &str) -> Option<DrmClient> {
    let mut c = DrmClient::default();
    let mut caps: HashMap<String, u64> = HashMap::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        if k == "drm-client-id" {
            c.id = v.to_string();
        } else if let Some(engine) = k.strip_prefix("drm-engine-capacity-") {
            caps.insert(engine.to_string(), v.parse().unwrap_or(1));
        } else if let Some(engine) = k.strip_prefix("drm-engine-") {
            let ns = v.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
            c.engines.insert(engine.to_string(), (ns, 1));
        }
    }
    for (e, cap) in caps {
        if let Some(x) = c.engines.get_mut(&e) {
            x.1 = cap.max(1);
        }
    }
    (!c.id.is_empty() && !c.engines.is_empty()).then_some(c)
}

/// Every DRM client: (pid, client) for each fd that's a /dev/dri node.
fn drm_clients() -> Vec<(u32, DrmClient)> {
    let mut out = Vec::new();
    for p in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = p.file_name().to_string_lossy().parse::<u32>() else { continue };
        for fd in std::fs::read_dir(p.path().join("fd")).into_iter().flatten().flatten() {
            if !std::fs::read_link(fd.path()).is_ok_and(|t| t.starts_with("/dev/dri")) {
                continue;
            }
            let info = p.path().join("fdinfo").join(fd.file_name());
            if let Some(c) = std::fs::read_to_string(info).ok().as_deref().and_then(parse_fdinfo) {
                out.push((pid, c));
            }
        }
    }
    out
}

/// Utilisation in percent between two readings of the same clients:
/// (busiest engine type across all clients, busiest engine per pid).
pub(crate) fn drm_usage(before: &HashMap<(u32, String), DrmClient>, now: &[(u32, DrmClient)], dt: Duration) -> (f32, HashMap<u32, f32>) {
    let ns = dt.as_nanos().max(1) as f64;
    let mut per_pid: HashMap<u32, f32> = HashMap::new();
    let mut per_engine: HashMap<String, f64> = HashMap::new();
    let mut counted = std::collections::HashSet::new();
    for (pid, c) in now {
        // The same client can be open on several fds; count it once.
        if !counted.insert((*pid, c.id.clone())) {
            continue;
        }
        let Some(old) = before.get(&(*pid, c.id.clone())) else { continue };
        let mut busiest = 0.0f64;
        for (engine, (busy, cap)) in &c.engines {
            let prev = old.engines.get(engine).map_or(*busy, |x| x.0);
            let pct = busy.saturating_sub(prev) as f64 / ns / *cap as f64 * 100.0;
            busiest = busiest.max(pct);
            *per_engine.entry(engine.clone()).or_default() += pct;
        }
        if busiest >= 0.1 {
            let e = per_pid.entry(*pid).or_default();
            *e = e.max(busiest.min(100.0) as f32);
        }
    }
    let total = per_engine.values().cloned().fold(0.0, f64::max).min(100.0) as f32;
    (total, per_pid)
}

/// One `nvidia-smi pmon -s u` line: (pid, SM %), skipping headers and idle ("-") rows.
pub(crate) fn parse_pmon(line: &str) -> Option<(u32, f32)> {
    if line.trim_start().starts_with('#') {
        return None;
    }
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 4 {
        return None;
    }
    Some((f[1].parse().ok()?, f[3].parse().ok()?))
}

/// The highest amdgpu busy percentage, if there's an amdgpu card.
fn amdgpu_busy() -> Option<f32> {
    std::fs::read_dir("/sys/class/drm").ok()?.flatten().filter_map(|e| read(e.path().join("device/gpu_busy_percent")).parse::<f32>().ok()).reduce(f32::max)
}

fn nvidia_total() -> Option<f32> {
    let o = Command::new("nvidia-smi").args(["--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"]).stderr(Stdio::null()).output().ok()?;
    String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| l.trim().parse::<f32>().ok()).reduce(f32::max)
}

/// Samples GPU use once a second until the monitor run `id` stops.
pub fn gpu_sampler(m: &Arc<Mutex<Monitor>>, id: u64) {
    let nvidia = has("nvidia-smi") && std::path::Path::new("/sys/module/nvidia").exists();
    // nvidia-smi pmon streams one line per process per second.
    let pmon: Arc<Mutex<HashMap<u32, f32>>> = Arc::default();
    let mut child = None;
    if nvidia {
        if let Ok(mut c) = Command::new("nvidia-smi").args(["pmon", "-s", "u", "-d", "1"]).stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null()).spawn() {
            if let Some(out) = c.stdout.take() {
                let (pm, gm) = (pmon.clone(), m.clone());
                std::thread::spawn(move || {
                    let mut batch: HashMap<u32, f32> = HashMap::new();
                    for line in std::io::BufReader::new(out).lines() {
                        let Ok(line) = line else { break };
                        if !alive(&gm, id) {
                            break;
                        }
                        // A new header starts a new second.
                        if line.starts_with("# gpu") {
                            *pm.lock().unwrap() = std::mem::take(&mut batch);
                        } else if let Some((pid, sm)) = parse_pmon(&line) {
                            *batch.entry(pid).or_default() += sm;
                        }
                    }
                });
            }
            child = Some(c);
        }
    }
    let mut before: HashMap<(u32, String), DrmClient> = HashMap::new();
    let mut last = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(1000));
        if !alive(m, id) {
            break;
        }
        let clients = drm_clients();
        let (drm_total, mut per_pid) = drm_usage(&before, &clients, last.elapsed());
        last = Instant::now();
        before = clients.into_iter().map(|(pid, c)| ((pid, c.id.clone()), c)).collect();
        for (pid, sm) in pmon.lock().unwrap().iter() {
            let e = per_pid.entry(*pid).or_default();
            *e = e.max(*sm);
        }
        let total = [amdgpu_busy(), nvidia.then(nvidia_total).flatten(), (!before.is_empty()).then_some(drm_total)].into_iter().flatten().reduce(f32::max);
        let mut g = m.lock().unwrap();
        if let Some(t) = total {
            g.gpu_seen = true;
            g.gpu_total = t.min(100.0);
        }
        g.gpu_by_pid = per_pid;
    }
    if let Some(mut c) = child {
        c.kill().ok();
        c.wait().ok();
    }
}

/// hwmon fans: ("<chip> <label>", "1200 RPM") for each spinning fan.
fn hwmon_fans() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for h in std::fs::read_dir("/sys/class/hwmon").into_iter().flatten().flatten() {
        let chip = read(h.path().join("name"));
        for f in std::fs::read_dir(h.path()).into_iter().flatten().flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            let Some(n) = name.strip_prefix("fan").and_then(|r| r.strip_suffix("_input")) else { continue };
            let rpm: u32 = read(f.path()).parse().unwrap_or(0);
            if rpm == 0 {
                continue;
            }
            let label = read(h.path().join(format!("fan{n}_label")));
            out.push((format!("{chip} {}", if label.is_empty() { format!("fan {n}") } else { label }), format!("{rpm} RPM")));
        }
    }
    out.sort();
    out
}

/// Temperatures and fans: every hwmon sensor (CPU, GPU, NVMe, board), plus nvidia-smi.
pub fn sensors(comps: &mut sysinfo::Components) -> (Vec<(String, f32)>, Vec<(String, String)>) {
    comps.refresh(true);
    let mut temps: Vec<(String, f32)> = comps.list().iter().filter_map(|c| c.temperature().filter(|t| *t > 0.0 && *t < 150.0).map(|t| (c.label().to_string(), t))).collect();
    let mut fans = hwmon_fans();
    if has("nvidia-smi") {
        let smi = super::run_hidden("nvidia-smi", &["--query-gpu=name,temperature.gpu,fan.speed", "--format=csv,noheader,nounits"]);
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
    }
    // amdgpu's own sensors say which GPU they are.
    for (name, _) in temps.iter_mut() {
        if name.starts_with("amdgpu") && !name.contains("GPU") {
            name.push_str(" (GPU)");
        }
    }
    temps.sort_by(|a, b| a.0.cmp(&b.0));
    (temps, fans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_fdinfo_and_usage() {
        // amdgpu's format, two engines; i915 adds capacity for engines with several instances.
        let amd = "pos:\t0\nflags:\t02100002\nmnt_id:\t26\nino:\t1046\ndrm-driver:\tamdgpu\ndrm-client-id:\t42\ndrm-pdev:\t0000:03:00.0\ndrm-engine-gfx:\t1000000000 ns\ndrm-engine-compute:\t0 ns\ndrm-memory-vram:\t1048576 KiB\n";
        let c = parse_fdinfo(amd).unwrap();
        assert_eq!(c.id, "42");
        assert_eq!(c.engines["gfx"], (1_000_000_000, 1));
        assert!(parse_fdinfo("pos:\t0\nflags:\t02\n").is_none());
        let i915 = parse_fdinfo("drm-driver:\ti915\ndrm-client-id:\t7\ndrm-engine-video:\t0 ns\ndrm-engine-capacity-video:\t2\n").unwrap();
        assert_eq!(i915.engines["video"], (0, 2));

        let before: HashMap<(u32, String), DrmClient> = [((100, "42".to_string()), c.clone())].into();
        let mut later = c.clone();
        later.engines.insert("gfx".into(), (1_500_000_000, 1)); // +0.5 s busy in 1 s
        let (total, per) = drm_usage(&before, &[(100, later.clone()), (100, later)], Duration::from_secs(1));
        assert!((total - 50.0).abs() < 0.1, "{total}");
        assert!((per[&100] - 50.0).abs() < 0.1);
    }

    #[test]
    fn pmon_lines() {
        // Captured from nvidia-smi 595 on this PC.
        assert_eq!(parse_pmon("# gpu         pid   type     sm    mem    enc    dec    jpg    ofa    command "), None);
        assert_eq!(parse_pmon("    0       1738     G      -      -      -      -      -      -    kwin_wayland   "), None);
        assert_eq!(parse_pmon("    0       2080     G     12      3      -      -      -      -    Xwayland       "), Some((2080, 12.0)));
    }
}
