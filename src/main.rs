// Flash: scans every PnP device, shows its driver version, finds devices with
// missing/broken drivers, and installs driver updates from Windows Update.
#![windows_subsystem = "windows"]
use eframe::egui::{self, Color32, RichText};
use flash::{Shared, hw_match, install, scan};
use std::time::Duration;

struct App {
    data: Shared,
}

impl App {
    fn run(&self, ctx: &egui::Context, job: fn(&Shared)) {
        let (d, ctx) = (self.data.clone(), ctx.clone());
        d.lock().unwrap().busy = true;
        std::thread::spawn(move || {
            job(&d);
            d.lock().unwrap().busy = false;
            ctx.request_repaint();
        });
    }
}

fn header(ui: &mut egui::Ui, cols: &[&str]) {
    for c in cols {
        ui.label(RichText::new(*c).strong());
    }
    ui.end_row();
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let d = self.data.clone();
        let d = d.lock().unwrap();
        if d.busy {
            ctx.request_repaint_after(Duration::from_millis(250)); // keep the log live
        }
        let (mut do_scan, mut do_install) = (false, false);

        egui::TopBottomPanel::top("bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("⚡ Flash");
                ui.separator();
                do_scan = ui.add_enabled(!d.busy, egui::Button::new("Scan")).clicked();
                let label = format!("Install {} update(s)", d.updates.len());
                do_install = ui.add_enabled(!d.busy && !d.updates.is_empty(), egui::Button::new(label)).clicked();
                if d.busy {
                    ui.spinner();
                    ui.label(d.log.lines().last().unwrap_or(""));
                }
            });
        });

        egui::TopBottomPanel::bottom("log").resizable(true).default_height(120.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
                ui.monospace(&d.log);
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                egui::CollapsingHeader::new(format!("Driver updates ({})", d.updates.len())).default_open(true).show(ui, |ui| {
                    egui::Grid::new("updates").striped(true).show(ui, |ui| {
                        header(ui, &["Update", "Date", "For device", "Current"]);
                        for u in &d.updates {
                            ui.label(&u[0]);
                            ui.label(&u[1]);
                            if let Some(dev) = d.devices.iter().find(|x| hw_match(&x[4], &u[2])) {
                                ui.label(&dev[1]);
                                ui.label(&dev[2]);
                            } else if let Some(m) = d.missing.iter().find(|x| hw_match(&x[2], &u[2])) {
                                ui.label(&m[0]);
                                ui.label(RichText::new("MISSING").color(Color32::RED));
                            } else {
                                ui.label(&u[2]);
                                ui.label("");
                            }
                            ui.end_row();
                        }
                    });
                });

                egui::CollapsingHeader::new(format!("Missing or broken drivers ({})", d.missing.len())).default_open(true).show(ui, |ui| {
                    egui::Grid::new("missing").striped(true).show(ui, |ui| {
                        header(ui, &["Device", "Error", "Hardware ID"]);
                        for m in &d.missing {
                            ui.label(RichText::new(&m[0]).color(Color32::RED));
                            ui.label(&m[1]);
                            ui.label(&m[2]);
                            ui.end_row();
                        }
                    });
                });

                egui::CollapsingHeader::new(format!("All devices ({})", d.devices.len())).default_open(true).show(ui, |ui| {
                    egui::Grid::new("devices").striped(true).show(ui, |ui| {
                        header(ui, &["Class", "Device", "Version", "Provider", "Status"]);
                        for dev in &d.devices {
                            ui.label(&dev[0]);
                            ui.label(&dev[1]);
                            ui.label(&dev[2]);
                            ui.label(&dev[3]);
                            if d.updates.iter().any(|u| hw_match(&dev[4], &u[2])) {
                                ui.label(RichText::new("Update available").color(Color32::from_rgb(230, 160, 0)));
                            } else {
                                ui.label(RichText::new("Up to date").color(Color32::from_rgb(60, 170, 80)));
                            }
                            ui.end_row();
                        }
                    });
                });
            });
        });

        drop(d);
        if do_scan {
            self.run(ctx, scan);
        }
        if do_install {
            self.run(ctx, install);
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 750.0])
            .with_title("Flash")
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/flash.png")).expect("bad icon png")),
        ..Default::default()
    };
    eframe::run_native(
        "Flash",
        options,
        Box::new(|cc| {
            let app = App { data: Shared::default() };
            app.run(&cc.egui_ctx, scan); // scan right away on launch
            Ok(Box::new(app))
        }),
    )
}
