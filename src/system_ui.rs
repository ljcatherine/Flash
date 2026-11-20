//! The SYSTEM pages: Device health (with benchmarks), Storage & cleanup, and System monitor.
use crate::{ACCENT, BG, BORDER, ERR, MUTED, OK, RAISED, SURFACE, TEXT, WARN, card, empty, header_cell, kv, metric, mix, page_header, primary, status};
use eframe::egui::{self, Align, Color32, Id, Layout, RichText, Sense, Shape, Stroke, Ui, pos2, vec2};
use egui_extras::{Column, TableBuilder};
use flash::bench::{self, Test};
use flash::health::{self, Report};
use flash::whea;
use flash::monitor::{self, SharedMonitor};
use flash::storage::{self, Breakdown, Drive, Junk, human};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct Sys {
    pub health: Option<Result<Report, String>>,
    pub health_busy: bool,
    pub bench: HashMap<Test, Result<f64, String>>,
    pub bench_running: Option<Test>,
    pub drives: Vec<Drive>,
    pub breakdown: Option<Breakdown>,
    pub breakdown_busy: bool,
    pub junk: Option<Vec<Junk>>,
    pub junk_gen: u64,
    pub junk_busy: bool,
    pub cleaning: Option<String>,
    pub clean_report: Vec<(bool, String)>,
}

type Shared = Arc<Mutex<Sys>>;

#[derive(Clone, Copy, PartialEq)]
enum AppSort {
    Cpu,
    Mem,
    Gpu,
    Disk,
    Name,
}

enum Act {
    Health,
    Drives,
    Breakdown,
    Junk,
    Bench(Vec<Test>),
    Clean,
    Start,
    Stop,
    Startup,
}

pub struct SysUi {
    sys: Shared,
    mon: SharedMonitor,
    picked: HashSet<String>,
    junk_gen_seen: u64,
    bench_drive: String,
    confirm_clean: bool,
    app_sort: AppSort,
    gpu_queue: std::collections::VecDeque<Test>,
    gpu_run: Option<crate::gpu_bench::GpuRun>,
    gpu_name: String,
}

const PART_COLORS: [Color32; 6] = [
    Color32::from_rgb(76, 141, 255),
    Color32::from_rgb(130, 110, 230),
    Color32::from_rgb(45, 170, 160),
    Color32::from_rgb(224, 164, 58),
    Color32::from_rgb(210, 110, 150),
    Color32::from_rgb(110, 118, 132),
];

fn section(ui: &mut Ui, title: &str, sub: &str) {
    ui.add_space(18.0);
    ui.label(RichText::new(title).size(15.0).strong().color(TEXT));
    if !sub.is_empty() {
        ui.label(RichText::new(sub).color(MUTED));
    }
    ui.add_space(6.0);
}

/// Horizontal fill bar.
fn bar(ui: &mut Ui, width: f32, frac: f32, color: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    ui.painter().rect_filled(r, 3.0, BORDER);
    let mut f = r;
    f.set_width(r.width() * frac.clamp(0.0, 1.0));
    ui.painter().rect_filled(f, 3.0, color);
}

/// A note with a coloured edge, for things that need attention.
fn note(ui: &mut Ui, color: Color32, title: &str, body: &str) {
    // Edge is drawn after layout, from the card's real rect.
    let card = egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(8.0)
        .inner_margin(egui::Margin { left: 16, right: 14, top: 10, bottom: 10 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong().color(TEXT));
            ui.add(egui::Label::new(RichText::new(body).color(MUTED)).wrap());
        })
        .response
        .rect;
    crate::accent_edge(ui.painter(), card, 8.0, color);
    ui.add_space(6.0);
}

/// Area graph of the last `HISTORY` samples, newest on the right.
fn sparkline(ui: &mut Ui, values: &[f32], max: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 56.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 4.0, BG);
    if values.len() < 2 || max <= 0.0 {
        return;
    }
    let step = rect.width() / (monitor::HISTORY - 1) as f32;
    let pts: Vec<egui::Pos2> = values
        .iter()
        .enumerate()
        .map(|(i, v)| pos2(rect.right() - (values.len() - 1 - i) as f32 * step, rect.bottom() - (v / max).clamp(0.0, 1.0) * (rect.height() - 4.0)))
        .collect();
    for w in pts.windows(2) {
        p.add(Shape::convex_polygon(vec![w[0], w[1], pos2(w[1].x, rect.bottom()), pos2(w[0].x, rect.bottom())], color.gamma_multiply(0.14), Stroke::NONE));
    }
    p.add(Shape::line(pts, Stroke::new(1.5_f32, color)));
}

fn rate(bytes_per_s: f64) -> String {
    format!("{}/s", human(bytes_per_s as u64))
}

impl SysUi {
    pub fn new() -> SysUi {
        SysUi { sys: Shared::default(), mon: SharedMonitor::default(), picked: HashSet::new(), junk_gen_seen: 0, bench_drive: String::new(), confirm_clean: false, app_sort: AppSort::Cpu, gpu_queue: Default::default(), gpu_run: None, gpu_name: String::new() }
    }

    pub fn monitor_running(&self) -> bool {
        self.mon.lock().unwrap().running
    }

    fn spawn(&self, ctx: &egui::Context, f: impl FnOnce(&Shared, &egui::Context) + Send + 'static) {
        let (s, c) = (self.sys.clone(), ctx.clone());
        std::thread::spawn(move || {
            f(&s, &c);
            c.request_repaint();
        });
    }

    fn act(&mut self, ctx: &egui::Context, acts: Vec<Act>) {
        for a in acts {
            match a {
                Act::Health => {
                    self.sys.lock().unwrap().health_busy = true;
                    self.spawn(ctx, |s, _| {
                        let r = health::read();
                        let mut g = s.lock().unwrap();
                        g.health = Some(r);
                        g.health_busy = false;
                    });
                }
                Act::Drives => {
                    let d = storage::drives();
                    if self.bench_drive.is_empty() {
                        self.bench_drive = d.iter().find(|x| x.mount.starts_with("C:")).or(d.first()).map(|x| x.mount.clone()).unwrap_or_default();
                    }
                    self.sys.lock().unwrap().drives = d;
                }
                Act::Breakdown => {
                    self.sys.lock().unwrap().breakdown_busy = true;
                    self.spawn(ctx, |s, _| {
                        let b = storage::breakdown();
                        let mut g = s.lock().unwrap();
                        g.breakdown = Some(b);
                        g.breakdown_busy = false;
                    });
                }
                Act::Junk => {
                    self.sys.lock().unwrap().junk_busy = true;
                    self.spawn(ctx, |s, _| {
                        let j = storage::find_junk();
                        let mut g = s.lock().unwrap();
                        g.junk = Some(j);
                        g.junk_gen += 1;
                        g.junk_busy = false;
                    });
                }
                Act::Bench(tests) => {
                    // GPU tests need the window's GL context, so they run from gpu_tick after the rest.
                    let (gpu, tests): (Vec<Test>, Vec<Test>) = tests.into_iter().partition(|t| t.is_gpu());
                    self.gpu_queue.extend(gpu);
                    if tests.is_empty() {
                        continue;
                    }
                    let drive = self.sys.lock().unwrap().drives.iter().find(|d| d.mount == self.bench_drive).cloned();
                    self.sys.lock().unwrap().bench_running = tests.first().copied();
                    self.spawn(ctx, move |s, c| {
                        let (dir, free) = drive.map_or((std::env::temp_dir(), 0), |d| (PathBuf::from(&d.mount), d.free));
                        let file = bench::bench_file(&dir);
                        for t in tests {
                            s.lock().unwrap().bench_running = Some(t);
                            c.request_repaint();
                            let need_file = || if file.exists() { Ok(0.0) } else { bench::disk_write(&file, free) };
                            let r = match t {
                                Test::CpuSingle => Ok(bench::cpu_single()),
                                Test::CpuMulti => Ok(bench::cpu_multi()),
                                Test::Memory => Ok(bench::memory()),
                                Test::DiskWrite => bench::disk_write(&file, free),
                                Test::DiskRead => need_file().and_then(|_| bench::disk_read(&file)),
                                Test::DiskRandom => need_file().and_then(|_| bench::disk_random(&file)),
                                Test::GpuCompute | Test::GpuFill => continue,
                            };
                            s.lock().unwrap().bench.insert(t, r);
                        }
                        std::fs::remove_file(&file).ok();
                        s.lock().unwrap().bench_running = None;
                    });
                }
                Act::Clean => {
                    let items: Vec<Junk> = self.sys.lock().unwrap().junk.iter().flatten().filter(|j| self.picked.contains(&j.id)).cloned().collect();
                    {
                        let mut g = self.sys.lock().unwrap();
                        g.cleaning = Some("Starting...".into());
                        g.clean_report.clear();
                    }
                    self.spawn(ctx, move |s, c| {
                        let mut total = 0;
                        for j in &items {
                            s.lock().unwrap().cleaning = Some(format!("Cleaning {}...", j.name));
                            c.request_repaint();
                            let line = match storage::clean(j) {
                                Ok(n) => {
                                    total += n;
                                    (true, format!("{}: freed {}", j.name, human(n)))
                                }
                                Err(e) => (false, format!("{}: {e}", j.name)),
                            };
                            s.lock().unwrap().clean_report.push(line);
                        }
                        s.lock().unwrap().clean_report.insert(0, (true, format!("Freed {} in total.", human(total))));
                        let fresh = storage::find_junk();
                        let mut g = s.lock().unwrap();
                        g.junk = Some(fresh);
                        g.junk_gen += 1;
                        g.cleaning = None;
                        g.drives = storage::drives();
                    });
                }
                Act::Start => {
                    let c = ctx.clone();
                    monitor::start(&self.mon, move || c.request_repaint());
                }
                Act::Stop => monitor::stop(&self.mon),
                Act::Startup => {
                    self.mon.lock().unwrap().startup = Some(vec![]);
                    let m = self.mon.clone();
                    let c = ctx.clone();
                    std::thread::spawn(move || {
                        let list = monitor::startup_apps();
                        m.lock().unwrap().startup = Some(list);
                        c.request_repaint();
                    });
                }
            }
        }
    }

    /// Drives GPU benchmarks. Called every frame with eframe's GL context, which is only current here.
    pub fn gpu_tick(&mut self, gl: Option<&std::sync::Arc<eframe::glow::Context>>, ctx: &egui::Context) {
        if let (true, Some(gl)) = (self.gpu_name.is_empty(), gl) {
            self.gpu_name = crate::gpu_bench::renderer(gl);
        }
        if self.gpu_queue.is_empty() && self.gpu_run.is_none() {
            return;
        }
        let Some(gl) = gl else {
            let mut g = self.sys.lock().unwrap();
            for t in self.gpu_queue.drain(..) {
                g.bench.insert(t, Err("OpenGL isn't available in this window".into()));
            }
            return;
        };
        if self.gpu_run.is_none() {
            if self.sys.lock().unwrap().bench_running.is_some() {
                return; // CPU, memory and disk tests finish first
            }
            let Some(t) = self.gpu_queue.pop_front() else { return };
            match crate::gpu_bench::GpuRun::new(gl, t) {
                Ok(r) => {
                    self.gpu_run = Some(r);
                    self.sys.lock().unwrap().bench_running = Some(t);
                }
                Err(e) => {
                    self.sys.lock().unwrap().bench.insert(t, Err(e));
                    ctx.request_repaint();
                    return;
                }
            }
        }
        if let Some(score) = self.gpu_run.as_mut().and_then(|r| r.step(gl)) {
            if let Some(run) = self.gpu_run.take() {
                let t = run.test;
                run.finish(gl);
                let mut g = self.sys.lock().unwrap();
                g.bench.insert(t, Ok(score));
                g.bench_running = None;
            }
        }
        ctx.request_repaint();
    }

    // ---------------- Device health ----------------

    pub fn health_page(&mut self, ui: &mut Ui, ctx: &egui::Context, devices: &[flash::Device]) {
        let mut acts = Vec::new();
        {
            let sys = self.sys.clone();
            let g = sys.lock().unwrap();
            if g.health.is_none() && !g.health_busy {
                acts.push(Act::Health);
            }
            if g.drives.is_empty() {
                acts.push(Act::Drives);
            }
            page_header(ui, "Device health", "How your hardware is doing, and how fast it is.", |ui| {
                if ui.add_enabled(!g.health_busy, egui::Button::new("Refresh")).clicked() {
                    acts.push(Act::Health);
                }
            });
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                match &g.health {
                    None => empty(ui, "Reading your hardware...", "This takes a few seconds."),
                    Some(Err(e)) => empty(ui, "Couldn't read hardware details", e),
                    Some(Ok(r)) => self.health_body(ui, r, devices),
                }
                self.bench_body(ui, &g, &mut acts);
            });
        }
        self.act(ctx, acts);
    }

    /// Hardware errors from the WHEA log, each with its explanation and fixes.
    fn whea_section(&self, ui: &mut Ui, r: &Report, devices: &[flash::Device]) {
        if r.whea.is_empty() {
            return;
        }
        let wctx = r.whea_context();
        section(ui, "Hardware errors", "From the last 90 days of Windows' hardware error log (WHEA), grouped, with what each one means and how to fix it.");
        for (i, g) in r.whea.iter().enumerate() {
            let a = whea::explain(g, &wctx);
            let device = (!g.device.is_empty())
                .then(|| devices.iter().find(|d| d.hwids.iter().any(|h| flash::hw_match(h, &g.device) || flash::hw_match(&g.device, h))))
                .flatten()
                .map(|d| d.name.clone());
            let color = if g.fatal { ERR } else { WARN };
            let rect = egui::Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0_f32, BORDER))
                .corner_radius(8.0)
                .inner_margin(egui::Margin { left: 16, right: 14, top: 10, bottom: 10 })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&a.title).strong().color(TEXT));
                        if let Some(name) = &device {
                            ui.label(RichText::new(format!("·  {name}")).color(TEXT));
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(format!("{} time{}", g.count, if g.count == 1 { "" } else { "s" })).strong().color(color));
                        });
                    });
                    let mut facts = vec![format!("Event {}", g.id), if g.fatal { "caused a crash".to_string() } else { "corrected, no crash".to_string() }];
                    facts.extend([&g.component, &g.source, &g.error_type].into_iter().filter(|s| !s.is_empty()).cloned());
                    if !g.bdf.is_empty() {
                        facts.push(format!("PCI {}", g.bdf));
                    }
                    facts.push(if g.first == g.last { format!("on {}", g.last) } else { format!("{} to {}", g.first, g.last) });
                    ui.add(egui::Label::new(RichText::new(facts.join("  ·  ")).size(12.5).color(MUTED)).wrap());
                    ui.add_space(2.0);
                    egui::CollapsingHeader::new(RichText::new("What it means and how to fix it").color(ACCENT)).id_salt(("whea", i)).default_open(i == 0).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(&a.meaning).color(TEXT)).wrap());
                        if let Some(n) = &a.note {
                            ui.add(egui::Label::new(RichText::new(n).color(WARN)).wrap());
                        }
                        ui.add_space(6.0);
                        ui.label(RichText::new("LIKELY CAUSES").size(11.5).strong().color(MUTED));
                        for c in &a.causes {
                            ui.add(egui::Label::new(RichText::new(format!("•  {c}")).color(TEXT)).wrap());
                        }
                        ui.add_space(6.0);
                        ui.label(RichText::new("HOW TO FIX IT, EASIEST FIRST").size(11.5).strong().color(MUTED));
                        for (n, f) in a.fixes.iter().enumerate() {
                            ui.add(egui::Label::new(RichText::new(format!("{}.  {f}", n + 1)).color(TEXT)).wrap());
                        }
                    });
                })
                .response
                .rect;
            crate::accent_edge(ui.painter(), rect, 8.0, color);
            ui.add_space(6.0);
        }
        ui.horizontal(|ui| {
            if ui.button("Open Event Viewer").clicked() {
                flash::run_hidden("cmd", &["/c", "start", "", "eventvwr.msc"]);
            }
            if ui.button("Windows Memory Diagnostic").on_hover_text("Tests your RAM at the next restart.").clicked() {
                std::process::Command::new("mdsched.exe").spawn().ok();
            }
        });
    }

    fn health_body(&self, ui: &mut Ui, r: &Report, devices: &[flash::Device]) {
        let gap = 12.0;
        let w = ((ui.available_width() - 3.0 * gap) / 4.0).max(150.0);
        let bad_disks = r.disks.iter().filter(|d| !health::disk_verdict(d).0).count();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            metric(ui, w, None, &format!("{}/{}", r.disks.len() - bad_disks, r.disks.len()), "Drives healthy", if bad_disks > 0 { ERR } else { TEXT });
            match &r.battery {
                Some(b) => metric(ui, w, None, &format!("{}%", b.health_pct()), "Battery capacity left", if b.health_pct() < 70 { WARN } else { TEXT }),
                None => metric(ui, w, None, "None", "Battery", MUTED),
            };
            metric(ui, w, Some(r.hardware_errors as usize), "", "Hardware errors (30 days)", if r.hardware_errors > 0 { WARN } else { TEXT });
            metric(ui, w, Some((r.crashes + r.unexpected_shutdowns) as usize), "", "Crashes & power losses (30 days)", if r.crashes > 0 { ERR } else if r.unexpected_shutdowns > 0 { WARN } else { TEXT });
        });

        ui.add_space(14.0);
        if let Some((rated, running)) = health::ram_slow(r) {
            note(ui, WARN, &format!("Memory is running at {running} MT/s, but it's rated for {rated}"), "Turn on XMP (Intel) or DOCP/EXPO (AMD) in your BIOS to run it at full speed. It's a free performance boost, often 10-20% in games.");
        }
        if r.crashes > 0 {
            note(ui, ERR, &format!("{} blue-screen crash{} in the last 30 days", r.crashes, if r.crashes == 1 { "" } else { "es" }), "Crashes are often caused by outdated chipset or graphics drivers. Check the Updates page, and keep an eye on the hardware errors count.");
        }
        if r.unexpected_shutdowns > 0 {
            note(ui, WARN, &format!("Windows shut down unexpectedly {} time{}", r.unexpected_shutdowns, if r.unexpected_shutdowns == 1 { "" } else { "s" }), "It lost power or stopped responding without shutting down properly. If you didn't hold the power button or lose power, this points to instability (power supply, overclock or overheating).");
        }
        self.whea_section(ui, r, devices);

        section(ui, "This PC", "");
        card(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::Grid::new("sysinfo").num_columns(2).spacing(vec2(24.0, 8.0)).show(ui, |ui| {
                for (k, v) in &r.system {
                    kv(ui, k, v);
                }
                for (name, drv) in &r.gpus {
                    kv(ui, "Graphics", &format!("{name} (driver {drv})"));
                }
            });
        });

        section(ui, "Drives", "Health as reported by each drive. Temperature and wear need the drive to report them.");
        card(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::Grid::new("disks").num_columns(7).striped(false).spacing(vec2(22.0, 10.0)).show(ui, |ui| {
                for h in ["DRIVE", "TYPE", "SIZE", "TEMPERATURE", "LIFE USED", "POWERED ON", "STATUS"] {
                    header_cell(ui, h);
                }
                ui.end_row();
                for d in &r.disks {
                    ui.label(RichText::new(&d.name).color(TEXT));
                    ui.label(RichText::new(format!("{} {}", d.bus, d.media)).color(MUTED));
                    ui.label(RichText::new(human(d.size)).color(MUTED));
                    ui.label(RichText::new(d.temp_c.map_or("-".into(), |t| format!("{t} °C"))).color(if d.temp_c.unwrap_or(0) >= 70 { WARN } else { MUTED }));
                    ui.label(RichText::new(d.wear_pct.map_or("-".into(), |w| format!("{w}%"))).color(if d.wear_pct.unwrap_or(0) >= 80 { WARN } else { MUTED }));
                    ui.label(RichText::new(d.hours.map_or("-".into(), |h| format!("{} h", h))).color(MUTED));
                    let (ok, text) = health::disk_verdict(d);
                    status(ui, if ok { OK } else { ERR }, &text);
                    ui.end_row();
                }
            });
        });

        if let Some(b) = &r.battery {
            section(ui, "Battery", "");
            card(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(format!("Holds {}% of its original charge", b.health_pct())).strong().color(TEXT));
                bar(ui, ui.available_width(), b.health_pct() as f32 / 100.0, if b.health_pct() < 70 { WARN } else { OK });
                ui.label(RichText::new(format!("{} mWh now, {} mWh when new. Currently {}% charged.", b.full_mwh, b.design_mwh, b.charge_pct)).color(MUTED));
            });
        }
    }

    fn bench_body(&mut self, ui: &mut Ui, g: &Sys, acts: &mut Vec<Act>) {
        section(ui, "Benchmarks", "Close games and big apps first for a fair result. Each test takes a few seconds; the window may stutter during GPU tests.");
        card(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("Disk tests run on").color(MUTED));
                egui::ComboBox::from_id_salt("bench_drive").selected_text(&self.bench_drive).width(90.0).show_ui(ui, |ui| {
                    for d in &g.drives {
                        ui.selectable_value(&mut self.bench_drive, d.mount.clone(), format!("{}  ({} free)", d.mount, human(d.free)));
                    }
                });
                if !self.gpu_name.is_empty() {
                    ui.label(RichText::new(format!("·  GPU tests run on {}", self.gpu_name.split('/').next().unwrap_or(&self.gpu_name))).color(MUTED));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let label = if g.bench_running.is_some() { "Running..." } else { "Run all" };
                    if ui.add_enabled(g.bench_running.is_none(), primary(label)).clicked() {
                        acts.push(Act::Bench(Test::ALL.to_vec()));
                    }
                });
            });
            ui.add_space(8.0);
            for (i, t) in Test::ALL.into_iter().enumerate() {
                if i > 0 {
                    ui.separator();
                }
                ui.horizontal(|ui| {
                    let left = (ui.available_width() - 420.0).max(220.0);
                    ui.allocate_ui_with_layout(vec2(left, 40.0), Layout::top_down(Align::Min), |ui| {
                        ui.set_width(left);
                        ui.label(RichText::new(t.name()).strong().color(TEXT));
                        ui.add(egui::Label::new(RichText::new(t.about()).size(12.5).color(MUTED)).wrap());
                    });
                    ui.allocate_ui_with_layout(vec2(320.0, 40.0), Layout::top_down(Align::Min), |ui| {
                        ui.set_width(320.0);
                        if g.bench_running == Some(t) {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(RichText::new("Running...").color(ACCENT));
                            });
                        } else {
                            match g.bench.get(&t) {
                                Some(Ok(v)) => {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(format!("{:.prec$}", v, prec = if *v < 100.0 { 1 } else { 0 })).size(17.0).strong().color(TEXT));
                                        ui.label(RichText::new(t.unit()).color(MUTED));
                                        ui.label(RichText::new(bench::rating(t, *v)).color(ACCENT));
                                    });
                                    bar(ui, 300.0, (*v / t.reference()) as f32, ACCENT);
                                }
                                Some(Err(e)) => {
                                    ui.add(egui::Label::new(RichText::new(format!("Failed: {e}")).color(ERR)).wrap());
                                }
                                None => {
                                    ui.label(RichText::new("Not run yet").color(MUTED));
                                }
                            }
                        }
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add_enabled(g.bench_running.is_none(), egui::Button::new("Run")).clicked() {
                            acts.push(Act::Bench(vec![t]));
                        }
                    });
                });
            }
        });
        ui.label(RichText::new("Bars compare against a fast current PC. Disk tests write a temporary file to the drive and delete it afterwards.").size(12.0).color(MUTED));
    }

    // ---------------- Storage ----------------

    pub fn storage_page(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let mut acts = Vec::new();
        {
            let sys = self.sys.clone();
            let g = sys.lock().unwrap();
            if g.drives.is_empty() {
                acts.push(Act::Drives);
            }
            if g.junk.is_none() && !g.junk_busy {
                acts.push(Act::Junk);
            }
            if g.junk_gen != self.junk_gen_seen {
                self.junk_gen_seen = g.junk_gen;
                self.picked = g.junk.iter().flatten().filter(|j| j.default_on && j.size > 0).map(|j| j.id.clone()).collect();
            }
            page_header(ui, "Storage & cleanup", "What's using your drives, and what you can safely remove.", |ui| {
                if ui.button("Refresh").clicked() {
                    acts.push(Act::Drives);
                }
            });
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                for d in &g.drives {
                    card(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let used = d.total - d.free;
                        let frac = used as f32 / d.total.max(1) as f32;
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(d.mount.trim_end_matches('\\')).size(16.0).strong().color(TEXT));
                            let label = if d.name.is_empty() { "Local disk" } else { &d.name };
                            ui.label(RichText::new(format!("{label} · {} · {}{}", d.fs, d.kind, if d.removable { " · removable" } else { "" })).color(MUTED));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new(format!("{} free of {}", human(d.free), human(d.total))).color(TEXT));
                            });
                        });
                        let color = if frac > 0.95 { ERR } else if frac > 0.85 { WARN } else { ACCENT };
                        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 10.0), Sense::hover());
                        ui.painter().rect_filled(r, 5.0, BORDER);
                        let mut f = r;
                        f.set_width(r.width() * frac);
                        ui.painter().rect_filled(f, 5.0, color);
                        ui.label(RichText::new(format!("{} used ({:.0}%)", human(used), frac * 100.0)).size(12.5).color(MUTED));
                    });
                    ui.add_space(8.0);
                }

                section(ui, "What's on your system drive", "Measures the main folders. Takes up to a minute on a full drive.");
                card(ui, |ui| {
                    ui.set_width(ui.available_width());
                    match &g.breakdown {
                        _ if g.breakdown_busy => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(RichText::new("Measuring folders...").color(MUTED));
                            });
                        }
                        None => {
                            if ui.add(primary("Analyze")).clicked() {
                                acts.push(Act::Breakdown);
                            }
                        }
                        Some(b) => {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("{} used on {}", human(b.used), b.drive)).strong().color(TEXT));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui.button("Analyze again").clicked() {
                                        acts.push(Act::Breakdown);
                                    }
                                });
                            });
                            // Stacked bar: one segment per part.
                            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
                            ui.painter().rect_filled(r, 5.0, BORDER);
                            let total = b.total.max(1) as f32;
                            let mut x = r.left();
                            for (i, (_, size)) in b.parts.iter().enumerate() {
                                let w = r.width() * (*size as f32 / total);
                                if w >= 1.0 {
                                    ui.painter().rect_filled(egui::Rect::from_min_size(pos2(x, r.top()), vec2(w, r.height())), 0.0, PART_COLORS[i % PART_COLORS.len()]);
                                }
                                x += w;
                            }
                            ui.add_space(8.0);
                            egui::Grid::new("parts").num_columns(4).spacing(vec2(18.0, 6.0)).show(ui, |ui| {
                                for (i, (name, size)) in b.parts.iter().enumerate() {
                                    let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                                    ui.painter().rect_filled(dot, 2.0, PART_COLORS[i % PART_COLORS.len()]);
                                    ui.label(RichText::new(*name).color(TEXT));
                                    ui.label(RichText::new(human(*size)).color(TEXT));
                                    ui.label(RichText::new(format!("{:.0}%", *size as f32 * 100.0 / b.used.max(1) as f32)).color(MUTED));
                                    ui.end_row();
                                }
                            });
                            if !b.largest.is_empty() {
                                ui.add_space(6.0);
                                egui::CollapsingHeader::new(RichText::new("Largest files in your folders").color(TEXT)).show(ui, |ui| {
                                    for (path, size) in &b.largest {
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(human(*size)).monospace().color(TEXT));
                                            ui.add(egui::Label::new(RichText::new(path.to_string_lossy()).color(MUTED)).truncate());
                                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                                if ui.small_button("Show").clicked() {
                                                    let arg = format!("/select,{}", path.to_string_lossy());
                                                    std::process::Command::new("explorer").arg(arg).spawn().ok();
                                                }
                                            });
                                        });
                                    }
                                });
                            }
                        }
                    }
                });

                section(ui, "Clean up", "Pick what to remove. Things that are in use are skipped automatically.");
                self.cleaner(ui, &g, &mut acts);
            });
        }
        self.act(ctx, acts);
    }

    fn cleaner(&mut self, ui: &mut Ui, g: &Sys, acts: &mut Vec<Act>) {
        card(ui, |ui| {
            ui.set_width(ui.available_width());
            let Some(junk) = &g.junk else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Looking for files to clean...").color(MUTED));
                });
                return;
            };
            let selected: u64 = junk.iter().filter(|j| self.picked.contains(&j.id)).map(|j| j.size).sum();
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} selected", human(selected))).size(16.0).strong().color(TEXT));
                if let Some(s) = &g.cleaning {
                    ui.spinner();
                    ui.label(RichText::new(s).color(MUTED));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add_enabled(g.cleaning.is_none() && selected > 0, primary("Clean selected")).clicked() {
                        self.confirm_clean = true;
                    }
                    if ui.add_enabled(g.cleaning.is_none() && !g.junk_busy, egui::Button::new("Rescan")).clicked() {
                        acts.push(Act::Junk);
                    }
                });
            });
            for (ok, line) in &g.clean_report {
                ui.label(RichText::new(line).size(12.5).color(if *ok { OK } else { WARN }));
            }
            let mut groups: Vec<&str> = junk.iter().map(|j| j.group).collect();
            groups.dedup();
            for grp in groups {
                ui.add_space(10.0);
                ui.label(RichText::new(grp.to_uppercase()).size(11.5).strong().color(MUTED));
                if grp == "Apps you may not need" {
                    ui.label(RichText::new("Only remove apps you don't use. They can be reinstalled from the Microsoft Store.").size(12.0).color(MUTED));
                }
                if grp == "Old driver versions" {
                    ui.label(RichText::new("Older copies of drivers that a newer version replaced. Removing them means you can't roll back to that version.").size(12.0).color(MUTED));
                }
                for j in junk.iter().filter(|j| j.group == grp) {
                    ui.horizontal(|ui| {
                        let mut on = self.picked.contains(&j.id);
                        if ui.add_enabled(j.size > 0 || matches!(j.kind, storage::Kind::App(_)), egui::Checkbox::without_text(&mut on)).changed() {
                            if on {
                                self.picked.insert(j.id.clone());
                            } else {
                                self.picked.remove(&j.id);
                            }
                        }
                        let w = (ui.available_width() - 110.0).max(200.0);
                        ui.allocate_ui_with_layout(vec2(w, 0.0), Layout::top_down(Align::Min), |ui| {
                            ui.set_width(w);
                            ui.label(RichText::new(&j.name).color(TEXT));
                            ui.add(egui::Label::new(RichText::new(&j.about).size(12.0).color(MUTED)).wrap());
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(if j.size > 0 { human(j.size) } else { "-".into() }).color(if j.size > 0 { TEXT } else { MUTED }));
                        });
                    });
                }
            }
        });
    }

    /// Clean confirmation; drawn outside the panels.
    pub fn modals(&mut self, ctx: &egui::Context) {
        if !self.confirm_clean {
            return;
        }
        let items: Vec<Junk> = self.sys.lock().unwrap().junk.iter().flatten().filter(|j| self.picked.contains(&j.id)).cloned().collect();
        let total: u64 = items.iter().map(|j| j.size).sum();
        let mut go = false;
        let modal = egui::Modal::new(Id::new("confirm_clean")).frame(egui::Frame::new().fill(SURFACE).stroke(Stroke::new(1.0_f32, BORDER)).corner_radius(10.0).inner_margin(22.0)).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.label(RichText::new(format!("Remove {} ({} items)?", human(total), items.len())).size(17.0).strong().color(TEXT));
            ui.add_space(6.0);
            for j in items.iter().take(12) {
                ui.label(RichText::new(format!("{}  ·  {}", j.name, human(j.size))).color(MUTED));
            }
            if items.len() > 12 {
                ui.label(RichText::new(format!("and {} more", items.len() - 12)).color(MUTED));
            }
            if items.iter().any(|j| matches!(j.kind, storage::Kind::RecycleBin)) {
                ui.label(RichText::new("The Recycle Bin is emptied permanently.").color(WARN));
            }
            if items.iter().any(|j| matches!(j.kind, storage::Kind::App(_))) {
                ui.label(RichText::new("Selected apps are uninstalled for your account.").color(WARN));
            }
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(primary("Clean")).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    self.confirm_clean = false;
                }
            });
        });
        if modal.should_close() {
            self.confirm_clean = false;
        }
        if go {
            self.confirm_clean = false;
            self.act(ctx, vec![Act::Clean]);
        }
    }

    // ---------------- Monitor ----------------

    pub fn monitor_page(&mut self, ui: &mut Ui, ctx: &egui::Context, now: f64) {
        let mut acts = Vec::new();
        {
            let mon = self.mon.clone();
            let m = mon.lock().unwrap();
            if m.startup.is_none() {
                acts.push(Act::Startup);
            }
            let elapsed = m.started.map(|s| s.elapsed().as_secs()).unwrap_or(0);
            let _ = now;
            page_header(ui, "System monitor", "See what's using your PC while you work or play. Start it, then switch to your game or app.", |ui| {
                if m.running {
                    if ui.add(egui::Button::new(RichText::new("Stop").color(TEXT).strong()).fill(RAISED).min_size(vec2(90.0, 30.0))).clicked() {
                        acts.push(Act::Stop);
                    }
                    ui.label(RichText::new(format!("Recording {}:{:02}", elapsed / 60, elapsed % 60)).color(ACCENT));
                } else if ui.add(primary("Start monitoring").min_size(vec2(150.0, 30.0))).clicked() {
                    acts.push(Act::Start);
                }
            });

            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                if m.history.is_empty() {
                    if m.running {
                        empty(ui, "Collecting the first samples...", "Graphs appear in a second or two.");
                    } else {
                        empty(ui, "Not running", "Press Start monitoring. Flash samples once a second, then you can see peaks and the apps responsible.");
                    }
                } else {
                    let last = *m.history.back().unwrap_or(&Default::default());
                    let col = |f: fn(&monitor::Sample) -> f32| m.history.iter().map(f).collect::<Vec<f32>>();
                    let gap = 12.0;
                    let w = ((ui.available_width() - 2.0 * gap) / 3.0).max(200.0);
                    let disk_max = m.history.iter().map(|s| s.disk).fold(1e6, f64::max) as f32;
                    let net_max = m.history.iter().map(|s| s.net).fold(1e5, f64::max) as f32;
                    let charts: [(&str, String, String, Vec<f32>, f32, Color32); 5] = [
                        ("CPU", format!("{:.0}%", last.cpu), format!("peak {:.0}%", m.peak.cpu), col(|s| s.cpu), 100.0, ACCENT),
                        ("Memory", format!("{:.0}%", last.mem), format!("{} of {}", human(m.mem_used), human(m.mem_total)), col(|s| s.mem), 100.0, Color32::from_rgb(130, 110, 230)),
                        ("GPU", if m.gpu_seen { format!("{:.0}%", last.gpu) } else { "-".into() }, if m.gpu_seen { format!("peak {:.0}%", m.peak.gpu) } else { "waiting for counters".into() }, col(|s| s.gpu), 100.0, OK),
                        ("Disk", rate(last.disk), format!("read {} · write {}", rate(m.disk_read), rate(m.disk_write)), col(|s| s.disk as f32), disk_max, WARN),
                        ("Network", rate(last.net), format!("down {} · up {}", rate(m.net_down), rate(m.net_up)), col(|s| s.net as f32), net_max, Color32::from_rgb(45, 170, 160)),
                    ];
                    for row in charts.chunks(3) {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = gap;
                            for (title, value, sub, values, max, color) in row {
                                ui.allocate_ui_with_layout(vec2(w, 140.0), Layout::top_down(Align::Min), |ui| {
                                    card(ui, |ui| {
                                        ui.set_width(w - 32.0);
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(*title).color(MUTED));
                                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                                ui.label(RichText::new(value).size(18.0).strong().color(TEXT));
                                            });
                                        });
                                        sparkline(ui, values, *max, *color);
                                        ui.add(egui::Label::new(RichText::new(sub).size(12.0).color(MUTED)).truncate());
                                    });
                                });
                            }
                        });
                        ui.add_space(gap);
                    }

                    // Per-thread load.
                    card(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(format!("CPU threads ({})", m.cores.len())).color(MUTED));
                        let n = m.cores.len().max(1) as f32;
                        let bw = ((ui.available_width() - (n - 1.0) * 4.0) / n).clamp(6.0, 40.0);
                        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::hover());
                        for (i, c) in m.cores.iter().enumerate() {
                            let x = r.left() + i as f32 * (bw + 4.0);
                            let col = egui::Rect::from_min_size(pos2(x, r.top()), vec2(bw, r.height()));
                            ui.painter().rect_filled(col, 2.0, BG);
                            let h = r.height() * (c / 100.0).clamp(0.0, 1.0);
                            ui.painter().rect_filled(egui::Rect::from_min_max(pos2(x, r.bottom() - h), col.max), 2.0, mix(ACCENT, ERR, (c - 70.0).max(0.0) / 30.0));
                        }
                    });

                    section(ui, "What's using your resources", "Grouped by app. Click a column to sort.");
                    let mut apps: Vec<&monitor::AppUse> = m.apps.iter().collect();
                    match self.app_sort {
                        AppSort::Cpu => apps.sort_by(|a, b| b.cpu.total_cmp(&a.cpu)),
                        AppSort::Mem => apps.sort_by(|a, b| b.mem.cmp(&a.mem)),
                        AppSort::Gpu => apps.sort_by(|a, b| b.gpu.total_cmp(&a.gpu)),
                        AppSort::Disk => apps.sort_by(|a, b| b.disk.total_cmp(&a.disk)),
                        AppSort::Name => apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                    }
                    apps.truncate(40);
                    let mut sort = self.app_sort;
                    TableBuilder::new(ui)
                        .id_salt("apps")
                        .striped(true)
                        .vscroll(false)
                        .cell_layout(Layout::left_to_right(Align::Center))
                        .column(Column::remainder().at_least(200.0).clip(true))
                        .column(Column::exact(110.0))
                        .column(Column::exact(120.0))
                        .column(Column::exact(90.0))
                        .column(Column::exact(120.0))
                        .header(28.0, |mut h| {
                            for (text, s) in [("APP", AppSort::Name), ("CPU", AppSort::Cpu), ("MEMORY", AppSort::Mem), ("GPU", AppSort::Gpu), ("DISK", AppSort::Disk)] {
                                h.col(|ui| {
                                    let t = RichText::new(text).size(12.0).strong().color(if sort == s { TEXT } else { MUTED });
                                    if ui.add(egui::Label::new(t).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                        sort = s;
                                    }
                                });
                            }
                        })
                        .body(|body| {
                            body.rows(28.0, apps.len(), |mut row| {
                                let a = apps[row.index()];
                                row.col(|ui| {
                                    ui.label(RichText::new(&a.name).color(TEXT));
                                    if a.count > 1 {
                                        ui.label(RichText::new(format!("×{}", a.count)).size(12.0).color(MUTED));
                                    }
                                });
                                let heat = |v: f32, warn: f32| if v >= warn { WARN } else { MUTED };
                                row.col(|ui| {
                                    ui.label(RichText::new(format!("{:.1}%", a.cpu)).monospace().color(heat(a.cpu, 25.0)));
                                });
                                row.col(|ui| {
                                    ui.label(RichText::new(human(a.mem)).monospace().color(heat(a.mem as f32 / m.mem_total.max(1) as f32 * 100.0, 15.0)));
                                });
                                row.col(|ui| {
                                    ui.label(RichText::new(if m.gpu_seen { format!("{:.0}%", a.gpu) } else { "-".into() }).monospace().color(heat(a.gpu, 40.0)));
                                });
                                row.col(|ui| {
                                    ui.label(RichText::new(rate(a.disk)).monospace().color(heat(a.disk as f32 / 1e6, 20.0)));
                                });
                            });
                        });
                    self.app_sort = sort;
                }

                section(ui, "Startup apps", "Programs that start with Windows. Fewer means a faster boot and less running in the background.");
                card(ui, |ui| {
                    ui.set_width(ui.available_width());
                    match &m.startup {
                        Some(list) if !list.is_empty() => {
                            for (name, cmd, place) in list {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(name).color(TEXT));
                                    ui.add(egui::Label::new(RichText::new(cmd).size(12.0).color(MUTED)).truncate());
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.label(RichText::new(place).size(12.0).color(MUTED));
                                    });
                                });
                            }
                        }
                        _ => {
                            ui.label(RichText::new("Reading startup apps...").color(MUTED));
                        }
                    }
                    ui.add_space(6.0);
                    if ui.button("Manage in Task Manager").clicked() {
                        std::process::Command::new("taskmgr").args(["/0", "/startup"]).spawn().ok();
                    }
                });
            });
        }
        self.act(ctx, acts);
    }
}
