//! UI for the extra tools: weekly scan toggle, install history, hardware knowledge, temperatures.
use crate::{ERR, MUTED, OK, TEXT, WARN, card, kv, status};
use eframe::egui::{self, RichText, Ui};
use flash::{extras, health::Report, hwkb};

fn open(url: &str) {
    flash::run_hidden("cmd", &["/c", "start", "", url]);
}

fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(14.0);
    ui.label(RichText::new(text).size(15.0).strong().color(TEXT));
    ui.add_space(4.0);
}

/// 1. Weekly background scan on/off. `on` is filled lazily (one schtasks query).
pub fn schedule(ui: &mut Ui, on: &mut Option<bool>, note: &mut String) {
    let current = *on.get_or_insert_with(extras::schedule_enabled);
    let mut want = current;
    ui.checkbox(&mut want, "Scan every week in the background")
        .on_hover_text("Sundays at noon. Flash only notifies you if it finds updates or device problems.");
    if want != current {
        match extras::set_schedule(want) {
            Ok(()) => *on = Some(want),
            Err(e) => *note = format!("Couldn't change the schedule: {e}"),
        }
    }
    let hidden = extras::load().hidden.len();
    if hidden > 0 && ui.small_button(format!("Show {hidden} hidden update(s) again")).clicked() {
        let mut state = extras::load();
        state.hidden.clear();
        extras::save(&state);
        *note = "Hidden updates will show again after the next scan.".into();
    }
    if !note.is_empty() {
        ui.label(RichText::new(note.as_str()).size(12.0).color(WARN));
    }
}

/// 2. Install history with undo. Returns the index to roll back, if clicked.
pub fn history(ui: &mut Ui, busy: bool) -> Option<usize> {
    let state = extras::load();
    if state.history.is_empty() {
        return None;
    }
    let mut undo = None;
    heading(ui, "Driver history");
    card(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::Grid::new("history").num_columns(4).spacing([18.0, 8.0]).show(ui, |ui| {
            for (i, c) in state.history.iter().enumerate().rev() {
                ui.label(RichText::new(extras::date(c.time)).color(MUTED));
                ui.label(RichText::new(&c.device).color(TEXT));
                ui.label(RichText::new(format!("{} → {}", c.old_version, c.new_version)).monospace().color(MUTED));
                if c.undone {
                    ui.label(RichText::new("Rolled back").color(MUTED));
                } else if !extras::is_oem(&c.new_inf) {
                    ui.label(RichText::new("Windows driver").color(MUTED)).on_hover_text("Built into Windows: use Device Manager's Roll Back Driver.");
                } else if ui.add_enabled(!busy, egui::Button::new("Undo")).on_hover_text("Removes the new driver so Windows goes back to the previous one.").clicked() {
                    undo = Some(i);
                }
                ui.end_row();
            }
        });
    });
    undo
}

/// 4. Temperatures and fans, with an overheating warning.
pub fn temps(ui: &mut Ui, temps: &[(String, f32)], fans: &[(String, f32)]) {
    heading(ui, "Temperatures");
    card(ui, |ui| {
        ui.set_width(ui.available_width());
        if temps.is_empty() {
            ui.label(RichText::new("This PC doesn't expose its temperature sensors to Windows. NVIDIA graphics cards are read automatically; for AMD or Intel graphics, use AMD Software or Intel Graphics Software.").color(MUTED));
            return;
        }
        let mut hot = false;
        for (name, t) in temps {
            let gpu = name.contains("GPU");
            let (warn, err) = if gpu { (83.0, 90.0) } else { (85.0, 95.0) };
            hot |= *t >= warn;
            status(ui, if *t >= err { ERR } else if *t >= warn { WARN } else { OK }, &format!("{name}: {t:.0} °C"));
        }
        for (name, f) in fans {
            ui.label(RichText::new(format!("{name}: {f:.0}%")).color(MUTED));
        }
        if hot {
            ui.label(RichText::new("Running hot. Check the fans and airflow, clean dust filters, and make sure the cooler is seated properly.").color(WARN));
        }
    });
}

/// 7 + 9 + knowledge base: CPU and GPU profiles, BIOS age and GPU driver status.
pub fn hardware(ui: &mut Ui, r: &Report) {
    let arch = hwkb::cpu_arch(&r.cpu_name());
    let cpu = hwkb::cpu_profile(arch);
    heading(ui, "Your hardware");
    card(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(cpu.name).strong().color(TEXT));
        for t in cpu.tips {
            ui.add(egui::Label::new(RichText::new(format!("•  {t}")).color(MUTED)).wrap());
        }
        ui.horizontal(|ui| {
            if ui.button(format!("Get the {}", cpu.tool.0)).clicked() {
                open(cpu.tool.1);
            }
            ui.label(RichText::new(format!("Memory profile in the BIOS: {}", arch.memory_profile())).size(12.5).color(MUTED));
        });

        // BIOS: show its age and where to get the newest one.
        let get = |k: &str| r.system.iter().find(|(key, _)| key == k).map_or(String::new(), |(_, v)| v.clone());
        let bios = get("BIOS");
        let board = get("Motherboard");
        ui.add_space(10.0);
        egui::Grid::new("bios").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
            kv(ui, "BIOS", &bios);
        });
        let bios_date = bios.rsplit(", ").next().unwrap_or("");
        let year_ago = extras::date(extras::now().saturating_sub(365 * 86400));
        if bios_date.len() == 10 && bios_date < year_ago.as_str() {
            ui.label(RichText::new("This BIOS is over a year old. Newer versions often fix stability, memory and security issues.").color(WARN));
        }
        let (maker, model) = board.split_once(' ').map_or((board.as_str(), ""), |(a, b)| (a, b));
        if !board.is_empty() && ui.button("Find the latest BIOS for this board").clicked() {
            open(&extras::bios_page(maker, model));
        }

        for (name, drv) in &r.gpus {
            let kind = hwkb::gpu_kind(name);
            let gpu = hwkb::gpu_profile(kind);
            ui.add_space(12.0);
            ui.label(RichText::new(format!("{} · {}", gpu.name, name)).strong().color(TEXT));
            if kind == hwkb::GpuKind::Nvidia {
                let installed = extras::nvidia_version(drv).unwrap_or_default();
                match &r.gpu_latest {
                    Some((latest, released, url)) if extras::newer(latest, &installed) => {
                        status(ui, WARN, &format!("Driver {installed} · Game Ready {latest} is available ({released})"));
                        if ui.button("Download the latest driver from NVIDIA").clicked() {
                            open(url);
                        }
                    }
                    Some(_) => status(ui, OK, &format!("Driver {installed} is the latest Game Ready driver")),
                    None => status(ui, MUTED, &format!("Driver {installed}")),
                }
            } else {
                ui.label(RichText::new(format!("Driver {drv}")).color(MUTED));
            }
            for t in gpu.tips {
                ui.add(egui::Label::new(RichText::new(format!("•  {t}")).color(MUTED)).wrap());
            }
            if kind != hwkb::GpuKind::Other && ui.button(format!("Open {}", gpu.tool.0)).clicked() {
                open(gpu.tool.1);
            }
        }
        if r.display_resets > 0 {
            ui.add_space(8.0);
            ui.label(RichText::new(format!("The graphics driver stopped responding {} time(s) in the last 30 days. See the tips above for your graphics card.", r.display_resets)).color(WARN));
        }
    });
}
