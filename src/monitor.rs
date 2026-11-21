//! Live system monitor: CPU, per-core load, memory, GPU, disk and network, plus which
//! apps use them, sampled once a second while running.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{Networks, ProcessRefreshKind, ProcessesToUpdate, System};

pub const HISTORY: usize = 120; // seconds of graph

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    pub cpu: f32,  // %
    pub mem: f32,  // %
    pub gpu: f32,  // %
    pub disk: f64, // bytes/s read + write
    pub net: f64,  // bytes/s down + up
}

#[derive(Clone, Debug, Default)]
pub struct AppUse {
    pub name: String,
    pub count: usize, // processes with this name
    pub cpu: f32,     // % of the whole CPU
    pub mem: u64,
    pub gpu: f32,
    pub disk: f64, // bytes/s
}

#[derive(Default)]
pub struct Monitor {
    pub running: bool,
    pub started: Option<Instant>,
    pub history: VecDeque<Sample>,
    pub peak: Sample,
    pub cores: Vec<f32>,
    pub apps: Vec<AppUse>,
    pub mem_total: u64,
    pub mem_used: u64,
    pub disk_read: f64,
    pub disk_write: f64,
    pub net_down: f64,
    pub net_up: f64,
    pub gpu_seen: bool, // GPU counters answered at least once
    pub gpu_by_pid: HashMap<u32, f32>,
    pub gpu_total: f32,
    pub startup: Option<Vec<crate::extras::StartupApp>>,
    pub temps: Vec<(String, f32)>,   // sensor, deg C
    pub fans: Vec<(String, String)>, // fan, reading ("45%" or "1200 RPM")
    run_id: u64,
}

pub type SharedMonitor = Arc<Mutex<Monitor>>;

/// Parses one GPU line, `GPU<TAB>total<TAB>pid:percent,...`: total and per-process utilisation.
pub fn parse_gpu(line: &str) -> Option<(f32, HashMap<u32, f32>)> {
    let mut parts = line.trim_end().split('\t');
    if parts.next()? != "GPU" {
        return None;
    }
    let total = parts.next()?.parse().ok()?;
    let per = parts
        .next()
        .unwrap_or("")
        .split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once(':')?;
            Some((k.parse().ok()?, v.parse().ok()?))
        })
        .collect();
    Some((total, per))
}

pub(crate) fn alive(m: &SharedMonitor, id: u64) -> bool {
    let g = m.lock().unwrap();
    g.running && g.run_id == id
}

pub fn start(m: &SharedMonitor, repaint: impl Fn() + Send + Sync + 'static) {
    let id = {
        let mut g = m.lock().unwrap();
        if g.running {
            return;
        }
        g.running = true;
        g.run_id += 1;
        g.started = Some(Instant::now());
        g.history.clear();
        g.peak = Sample::default();
        g.run_id
    };
    let repaint = Arc::new(repaint);

    // GPU sampler: Windows' GPU engine counters, or DRM fdinfo, amdgpu and nvidia-smi on Linux.
    let gm = m.clone();
    std::thread::spawn(move || crate::platform::gpu_sampler(&gm, id));

    // Everything else, once a second.
    let sm = m.clone();
    std::thread::spawn(move || {
        let mut sys = System::new();
        let mut nets = Networks::new_with_refreshed_list();
        let kind = ProcessRefreshKind::nothing().with_cpu().with_memory().with_disk_usage();
        sys.refresh_cpu_usage();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        let mut last = Instant::now();
        let mut comps = sysinfo::Components::new_with_refreshed_list();
        let mut tick = 0u32;
        loop {
            std::thread::sleep(Duration::from_millis(1000));
            if !alive(&sm, id) {
                break;
            }
            sys.refresh_cpu_usage();
            sys.refresh_memory();
            sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
            nets.refresh(true);
            let dt = last.elapsed().as_secs_f64().max(0.2);
            last = Instant::now();

            let ncpu = sys.cpus().len().max(1) as f32;
            let gpu_by_pid = sm.lock().unwrap().gpu_by_pid.clone();
            let mut apps: HashMap<String, AppUse> = HashMap::new();
            let (mut read, mut write) = (0u64, 0u64);
            for p in sys.processes().values() {
                let name = p.name().to_string_lossy();
                let name = name.strip_suffix(".exe").unwrap_or(&name);
                if name.is_empty() || name == "System Idle Process" || name == "Idle" {
                    continue;
                }
                let du = p.disk_usage();
                read += du.read_bytes;
                write += du.written_bytes;
                let a = apps.entry(name.to_string()).or_insert_with(|| AppUse { name: name.to_string(), ..Default::default() });
                a.count += 1;
                a.cpu += p.cpu_usage() / ncpu;
                a.mem += p.memory();
                a.disk += (du.read_bytes + du.written_bytes) as f64 / dt;
                a.gpu += gpu_by_pid.get(&p.pid().as_u32()).copied().unwrap_or(0.0);
            }
            let (down, up) = nets.iter().fold((0u64, 0u64), |(d, u), (_, n)| (d + n.received(), u + n.transmitted()));

            // Temperatures every 3 s: every sensor the OS exposes, plus nvidia-smi on NVIDIA cards.
            tick += 1;
            let sensors = (tick % 3 == 1).then(|| crate::platform::sensors(&mut comps));
            let mut g = sm.lock().unwrap();
            if let Some((t, f)) = sensors {
                g.temps = t;
                g.fans = f;
            }
            g.cores = sys.cpus().iter().map(|c| c.cpu_usage()).collect();
            g.mem_total = sys.total_memory();
            g.mem_used = sys.used_memory();
            g.disk_read = read as f64 / dt;
            g.disk_write = write as f64 / dt;
            g.net_down = down as f64 / dt;
            g.net_up = up as f64 / dt;
            let s = Sample {
                cpu: sys.global_cpu_usage(),
                mem: if g.mem_total > 0 { g.mem_used as f32 * 100.0 / g.mem_total as f32 } else { 0.0 },
                gpu: g.gpu_total,
                disk: g.disk_read + g.disk_write,
                net: g.net_down + g.net_up,
            };
            g.peak = Sample { cpu: g.peak.cpu.max(s.cpu), mem: g.peak.mem.max(s.mem), gpu: g.peak.gpu.max(s.gpu), disk: g.peak.disk.max(s.disk), net: g.peak.net.max(s.net) };
            g.history.push_back(s);
            while g.history.len() > HISTORY {
                g.history.pop_front();
            }
            g.apps = apps.into_values().collect();
            drop(g);
            repaint();
        }
    });
}

pub fn stop(m: &SharedMonitor) {
    m.lock().unwrap().running = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gpu_lines() {
        let (total, per) = parse_gpu("GPU\t37.5\t1234:30.2,88:7.3\r\n").unwrap();
        assert_eq!(total, 37.5);
        assert_eq!(per.get(&1234), Some(&30.2));
        assert_eq!(per.len(), 2);
        assert_eq!(parse_gpu("GPU\t0\t").unwrap().1.len(), 0);
        assert!(parse_gpu("noise").is_none());
    }
}
