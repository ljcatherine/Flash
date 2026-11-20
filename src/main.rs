// Flash: scans every PnP device, shows its driver, and installs newer drivers from
// Windows Update, the Microsoft Update Catalog, the Driver Store or a local folder.
#![windows_subsystem = "windows"]
use eframe::egui::{self, Align, Align2, Color32, CornerRadius, FontId, Id, Layout, Margin, RichText, Sense, Shape, Stroke, TextStyle, Ui, pos2, vec2};
use egui_extras::{Column, TableBuilder};
mod gpu_bench;
mod system_ui;
mod app_update_ui;

use flash::{Device, PURPOSES, Shared, Source, Update, backup, check_folder, install, problem_text, purpose, scan};
use std::collections::{HashMap, HashSet};

// Neutral dark palette: one accent colour, status colours only where they carry meaning.
const BG: Color32 = Color32::from_rgb(15, 17, 21);
const SIDEBAR: Color32 = Color32::from_rgb(19, 22, 28);
const SURFACE: Color32 = Color32::from_rgb(23, 27, 34);
const RAISED: Color32 = Color32::from_rgb(30, 35, 44);
const BORDER: Color32 = Color32::from_rgb(38, 44, 54);
const TEXT: Color32 = Color32::from_rgb(230, 232, 235);
const MUTED: Color32 = Color32::from_rgb(138, 147, 163);
const ACCENT: Color32 = Color32::from_rgb(76, 141, 255);
const WARN: Color32 = Color32::from_rgb(224, 164, 58);
const ERR: Color32 = Color32::from_rgb(229, 83, 75);
const OK: Color32 = Color32::from_rgb(63, 185, 80);

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Overview,
    Map,
    Updates,
    Devices,
    Problems,
    Health,
    Storage,
    Monitor,
    Activity,
}

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Name,
    Class,
    Version,
    Date,
    Provider,
    Status,
}

enum Action {
    Scan,
    Install(Vec<Update>, bool),
    Folder,
    Backup,
}

struct App {
    data: Shared,
    page: Page,
    page_at: f64,
    search: String,
    purpose: String, // "" = all purposes
    only_updates: bool,
    sort: (Sort, bool), // column, ascending
    device: Option<String>, // instance ID shown in the details panel
    picked: HashSet<String>, // update keys ticked for install
    seen_scans: u64,
    scanned_at: Option<f64>,
    restore_point: bool,
    confirm: bool,
    was_busy: bool,
    toast: Option<(String, f64)>,
    sys: system_ui::SysUi,
    app_update: app_update_ui::AppUpdateUi,
    verbose: bool, // sidebar status expanded into live details
}

impl App {
    fn run(&self, ctx: &egui::Context, job: impl FnOnce(&Shared) + Send + 'static) {
        let (d, ctx) = (self.data.clone(), ctx.clone());
        d.lock().unwrap().busy = true;
        std::thread::spawn(move || {
            job(&d);
            d.lock().unwrap().busy = false;
            ctx.request_repaint();
        });
    }

    fn go(&mut self, page: Page, now: f64) {
        if self.page != page {
            self.page = page;
            self.page_at = now;
        }
    }
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t.clamp(0.0, 1.0)).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// The app mark: the bolt from assets/flash.svg on a rounded tile, as two convex halves.
fn mark(ui: &mut Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, size * 0.22, ACCENT);
    let c = rect.center();
    let pt = |x: f32, y: f32| c + vec2(x - 128.0, y - 128.0) * (size / 256.0);
    p.add(Shape::convex_polygon(vec![pt(146.0, 28.0), pt(68.0, 140.0), pt(120.0, 140.0), pt(134.0, 108.0)], Color32::WHITE, Stroke::NONE));
    p.add(Shape::convex_polygon(vec![pt(134.0, 108.0), pt(120.0, 140.0), pt(104.0, 228.0), pt(188.0, 108.0)], Color32::WHITE, Stroke::NONE));
}

/// The only way the app draws an accent edge: a 3px bar on the left of an already-laid-out
/// shape, inset top and bottom and clipped to that shape, so it can never run past it.
fn accent_edge(painter: &egui::Painter, shape: egui::Rect, inset: f32, color: Color32) {
    let h = (shape.height() - 2.0 * inset).max(0.0);
    let bar = egui::Rect::from_min_size(pos2(shape.left(), shape.top() + inset), vec2(3.0, h));
    painter.with_clip_rect(shape.intersect(painter.clip_rect())).rect_filled(bar, 1.5, color);
}

/// Thin indeterminate progress bar.
fn progress(ui: &mut Ui, t: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 2.0), Sense::hover());
    let w = rect.width();
    let x = ((t * 0.6) % 1.4 - 0.4) * w;
    let seg = egui::Rect::from_min_max(pos2(rect.left() + x.max(0.0), rect.top()), pos2((rect.left() + x + 0.4 * w).min(rect.right()), rect.bottom()));
    ui.painter().rect_filled(rect, 1.0, BORDER);
    ui.painter().rect_filled(seg, 1.0, ACCENT);
}

/// Sidebar navigation item with an optional count on the right.
fn nav(ui: &mut Ui, label: &str, count: Option<(usize, Color32)>, active: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
    let h = ui.ctx().animate_bool_with_time(resp.id, resp.hovered() || active, 0.12);
    let p = ui.painter();
    if h > 0.0 {
        p.rect_filled(rect, 6.0, mix(SIDEBAR, RAISED, h));
    }
    if active {
        accent_edge(p, rect, 9.0, ACCENT);
    }
    p.text(rect.left_center() + vec2(14.0, 0.0), Align2::LEFT_CENTER, label, FontId::proportional(14.0), if active { TEXT } else { MUTED });
    if let Some((n, color)) = count.filter(|(n, _)| *n > 0) {
        p.text(rect.right_center() - vec2(12.0, 0.0), Align2::RIGHT_CENTER, n.to_string(), FontId::proportional(12.5), color);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Status line with a triangle that turns from pointing right (closed) to up (open).
fn disclosure(ui: &mut Ui, open: f32, text: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::click());
    let hover = resp.hovered();
    let c = rect.left_center() + vec2(5.0, 0.0);
    let rot = egui::emath::Rot2::from_angle(-std::f32::consts::FRAC_PI_2 * open);
    let tri: Vec<egui::Pos2> = [vec2(-2.5, -4.0), vec2(-2.5, 4.0), vec2(3.5, 0.0)].iter().map(|v| c + rot * *v).collect();
    let color = if hover { TEXT } else { MUTED };
    let p = ui.painter();
    p.add(Shape::convex_polygon(tri, color, Stroke::NONE));
    let galley = p.layout_no_wrap(text.to_string(), FontId::proportional(12.0), color);
    let clip = egui::Rect::from_min_max(pos2(rect.left() + 14.0, rect.top()), rect.max);
    p.with_clip_rect(clip).galley(pos2(clip.left(), rect.center().y - galley.size().y / 2.0), galley, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A dot plus label, used for every status in the app.
fn status(ui: &mut Ui, color: Color32, text: &str) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(8.0, 14.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 3.5, color);
        ui.label(RichText::new(text).color(TEXT));
    });
}

fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new().fill(SURFACE).stroke(Stroke::new(1.0_f32, BORDER)).corner_radius(8.0).inner_margin(16.0).show(ui, add).inner
}

/// Metric tile: counts up to its value; the border picks up the accent on hover.
fn metric(ui: &mut Ui, w: f32, value: Option<usize>, text: &str, label: &str, color: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 84.0), Sense::click());
    let ctx = ui.ctx();
    let h = ctx.animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let p = ui.painter();
    p.rect_filled(rect, 8.0, SURFACE);
    p.rect_stroke(rect, 8.0, Stroke::new(1.0_f32, mix(BORDER, ACCENT, h * 0.8)), egui::StrokeKind::Inside);
    let shown = match value {
        Some(v) => format!("{}", ctx.animate_value_with_time(resp.id.with("n"), v as f32, 0.6).round() as i64),
        None => text.to_string(),
    };
    p.text(rect.min + vec2(16.0, 14.0), Align2::LEFT_TOP, shown, FontId::proportional(26.0), color);
    p.text(rect.min + vec2(16.0, 54.0), Align2::LEFT_TOP, label, FontId::proportional(13.0), MUTED);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn page_header(ui: &mut Ui, title: &str, subtitle: &str, actions: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).size(21.0).strong().color(TEXT));
            ui.label(RichText::new(subtitle).color(MUTED));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), actions);
    });
    ui.add_space(16.0);
}

fn primary(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).color(Color32::WHITE).strong()).fill(ACCENT).corner_radius(6.0).min_size(vec2(0.0, 30.0))
}

fn header_cell(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(MUTED).strong());
}

fn sort_header(ui: &mut Ui, text: &str, col: Sort, sort: &mut (Sort, bool)) {
    let arrow = if sort.0 == col { if sort.1 { "  ^" } else { "  v" } } else { "" };
    let resp = ui.add(egui::Label::new(RichText::new(format!("{text}{arrow}")).size(12.0).color(if sort.0 == col { TEXT } else { MUTED }).strong()).sense(Sense::click()));
    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        *sort = if sort.0 == col { (col, !sort.1) } else { (col, true) };
    }
}

fn empty(ui: &mut Ui, title: &str, body: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(title).size(16.0).strong().color(TEXT));
        ui.label(RichText::new(body).color(MUTED));
    });
}

fn ago(secs: f64) -> String {
    match secs as u64 {
        0..=59 => "just now".into(),
        s @ 60..=3599 => format!("{} min ago", s / 60),
        s => format!("{} h ago", s / 3600),
    }
}

/// Device status: (colour, text, sort rank).
fn device_status(dev: &Device, n_updates: usize) -> (Color32, &'static str, u8) {
    if dev.problem != 0 {
        (if dev.problem == 22 { MUTED } else { ERR }, problem_text(dev.problem), 0)
    } else if n_updates > 0 {
        (WARN, "Update available", 1)
    } else {
        (OK, "Up to date", 2)
    }
}

fn kv(ui: &mut Ui, key: &str, value: &str) {
    ui.label(RichText::new(key).color(MUTED));
    ui.add(egui::Label::new(RichText::new(if value.is_empty() { "-" } else { value }).color(TEXT)).wrap());
    ui.end_row();
}

// ---------- Health map ----------

const MAP_CENTER: egui::Vec2 = egui::Vec2::new(200.0, 84.0);
const MAP_GROUP: egui::Vec2 = egui::Vec2::new(184.0, 54.0);
const MAP_LEAF: egui::Vec2 = egui::Vec2::new(250.0, 40.0);
const LEAF_GAP: f32 = 8.0;
const MAX_LEAVES: usize = 4;

struct Leaf {
    title: String,
    detail: String,
    color: Color32,
    device: Option<String>,
}

struct Branch {
    purpose: &'static str,
    color: Color32,
    devices: usize,
    attention: usize,
    leaves: Vec<Leaf>,
}

impl Branch {
    fn height(&self) -> f32 {
        MAP_GROUP.y.max(self.leaves.len() as f32 * (MAP_LEAF.y + LEAF_GAP) - LEAF_GAP)
    }
}

fn ease(x: f32) -> f32 {
    1.0 - (1.0 - x.clamp(0.0, 1.0)).powi(3)
}

/// Horizontal S-curve between two points.
fn curve(p: &egui::Painter, a: egui::Pos2, b: egui::Pos2, stroke: Stroke) {
    let dx = (b.x - a.x) * 0.5;
    p.add(egui::epaint::CubicBezierShape::from_points_stroke([a, a + vec2(dx, 0.0), b - vec2(dx, 0.0), b], false, Color32::TRANSPARENT, stroke));
}

/// Single line of text cut with an ellipsis at `max_w`.
fn line(p: &egui::Painter, pos: egui::Pos2, text: &str, size: f32, color: Color32, max_w: f32) {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), FontId::proportional(size), color);
    job.wrap = egui::text::TextWrapping { max_width: max_w, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    p.galley(pos, p.layout_job(job), color);
}

fn branches(devices: &[Device], by_dev: &HashMap<&str, Vec<&Update>>) -> Vec<Branch> {
    let mut out = Vec::new();
    for p in PURPOSES {
        let devs: Vec<&Device> = devices.iter().filter(|x| purpose(&x.class) == p).collect();
        if devs.is_empty() {
            continue;
        }
        let ups = |x: &Device| by_dev.get(x.instance.as_str()).and_then(|v| v.first().copied());
        let mut att: Vec<&Device> = devs.iter().copied().filter(|x| x.problem != 0 || ups(x).is_some()).collect();
        att.sort_by_key(|x| match x.problem {
            0 => 1,
            22 => 2,
            _ => 0,
        });
        let broken = att.iter().any(|x| x.problem != 0 && x.problem != 22);
        let color = if broken { ERR } else if att.iter().any(|x| ups(x).is_some()) { WARN } else { OK };
        let mut leaves: Vec<Leaf> = att
            .iter()
            .take(MAX_LEAVES)
            .map(|x| {
                let (color, detail) = match (x.problem, ups(x)) {
                    (0, Some(u)) => (WARN, format!("{} → {}", if x.version.is_empty() { "none" } else { &x.version }, u.version)),
                    (22, _) => (MUTED, problem_text(22).to_string()),
                    (code, _) => (ERR, problem_text(code).to_string()),
                };
                Leaf { title: x.name.clone(), detail, color, device: Some(x.instance.clone()) }
            })
            .collect();
        if att.len() > MAX_LEAVES {
            leaves.push(Leaf { title: format!("{} more need attention", att.len() - MAX_LEAVES), detail: "Open the group to see them".into(), color: WARN, device: None });
        }
        let ok = devs.len() - att.len();
        if ok > 0 {
            leaves.push(Leaf { title: format!("{ok} up to date"), detail: if att.is_empty() { "Nothing to do".into() } else { "Working normally".into() }, color: OK, device: None });
        }
        out.push(Branch { purpose: p, color, devices: devs.len(), attention: att.len(), leaves });
    }
    out
}

/// Draws the mind map and returns what was clicked: a purpose, and maybe a device in it.
fn health_map(ui: &mut Ui, devices: &[Device], by_dev: &HashMap<&str, Vec<&Update>>, age: f32, n_updates: usize, n_problems: usize) -> Option<(&'static str, Option<String>)> {
    let bs = branches(devices, by_dev);
    // Split branches between the two sides, keeping both columns about the same height.
    let (mut left, mut right): (Vec<usize>, Vec<usize>) = (vec![], vec![]);
    let (mut lh, mut rh) = (0.0, 0.0);
    for (i, b) in bs.iter().enumerate() {
        if rh <= lh {
            right.push(i);
            rh += b.height() + 22.0;
        } else {
            left.push(i);
            lh += b.height() + 22.0;
        }
    }
    let side_w = MAP_LEAF.x + 36.0 + MAP_GROUP.x + 56.0;
    let avail = ui.available_size();
    let size = vec2(avail.x.max(2.0 * side_w + MAP_CENTER.x + 24.0), avail.y.max(lh.max(rh) + 40.0));
    let mut clicked = None;

    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        let c = rect.center();
        let p = ui.painter().clone();
        let center = egui::Rect::from_center_size(c, MAP_CENTER);

        for (side, idx, total) in [(1.0_f32, &right, rh - 22.0), (-1.0, &left, lh - 22.0)] {
            let gx = c.x + side * (MAP_CENTER.x / 2.0 + 56.0 + MAP_GROUP.x / 2.0);
            let mut top = c.y - total / 2.0;
            for &i in idx.iter() {
                let b = &bs[i];
                let bh = b.height();
                let gy = top + bh / 2.0;
                top += bh + 22.0;
                // Branches grow out from the centre one after another; leaves follow.
                let a = ease((age - 0.05 * (i as f32)) / 0.35);
                let la = ease((age - 0.05 * (i as f32) - 0.15) / 0.35);
                let gpos = c + (egui::pos2(gx, gy) - c) * a;
                let grect = egui::Rect::from_center_size(gpos, MAP_GROUP);
                curve(&p, egui::pos2(c.x + side * MAP_CENTER.x / 2.0, c.y), egui::pos2(gpos.x - side * MAP_GROUP.x / 2.0, gpos.y), Stroke::new(1.5_f32, b.color.gamma_multiply(0.55 * a)));

                let resp = ui.interact(grect, ui.id().with(("branch", b.purpose)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                let h = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
                if resp.clicked() {
                    clicked = Some((b.purpose, None));
                }
                let ring = if b.attention > 0 { b.color.gamma_multiply(0.6) } else { BORDER };
                p.rect_filled(grect, 8.0, SURFACE.gamma_multiply(a));
                p.rect_stroke(grect, 8.0, Stroke::new(1.0_f32, mix(ring, ACCENT, h).gamma_multiply(a)), egui::StrokeKind::Inside);
                accent_edge(&p, grect, 12.0, b.color.gamma_multiply(a));
                line(&p, grect.min + vec2(14.0, 9.0), b.purpose, 14.0, TEXT.gamma_multiply(a), MAP_GROUP.x - 22.0);
                let sub = if b.attention > 0 { format!("{} devices · {} need attention", b.devices, b.attention) } else { format!("{} devices", b.devices) };
                line(&p, grect.min + vec2(14.0, 29.0), &sub, 11.5, MUTED.gamma_multiply(a), MAP_GROUP.x - 22.0);

                let leaves_h = b.leaves.len() as f32 * (MAP_LEAF.y + LEAF_GAP) - LEAF_GAP;
                let mut ly = gy - leaves_h / 2.0 + MAP_LEAF.y / 2.0;
                for (j, leaf) in b.leaves.iter().enumerate() {
                    let lx = gx + side * (MAP_GROUP.x / 2.0 + 36.0 + MAP_LEAF.x / 2.0);
                    let lpos = gpos + (egui::pos2(lx, ly) - gpos) * la;
                    ly += MAP_LEAF.y + LEAF_GAP;
                    let lrect = egui::Rect::from_center_size(lpos, MAP_LEAF);
                    curve(&p, egui::pos2(gpos.x + side * MAP_GROUP.x / 2.0, gpos.y), egui::pos2(lpos.x - side * MAP_LEAF.x / 2.0, lpos.y), Stroke::new(1.0_f32, leaf.color.gamma_multiply(0.4 * la)));
                    let resp = ui.interact(lrect, ui.id().with(("leaf", b.purpose, j)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{}\n{}", leaf.title, leaf.detail));
                    let h = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
                    if resp.clicked() {
                        clicked = Some((b.purpose, leaf.device.clone()));
                    }
                    p.rect_filled(lrect, 6.0, mix(BG, SURFACE, 0.6 + 0.4 * h).gamma_multiply(la));
                    p.rect_stroke(lrect, 6.0, Stroke::new(1.0_f32, mix(BORDER, ACCENT, h).gamma_multiply(la)), egui::StrokeKind::Inside);
                    p.circle_filled(lrect.left_center() + vec2(12.0, 0.0), 3.5, leaf.color.gamma_multiply(la));
                    line(&p, lrect.min + vec2(22.0, 4.0), &leaf.title, 12.5, TEXT.gamma_multiply(la), MAP_LEAF.x - 30.0);
                    line(&p, lrect.min + vec2(22.0, 21.0), &leaf.detail, 11.0, leaf.color.gamma_multiply(0.9 * la), MAP_LEAF.x - 30.0);
                }
            }
        }

        // Centre node last so curves tuck under it.
        p.rect_filled(center, 10.0, RAISED);
        p.rect_stroke(center, 10.0, Stroke::new(1.0_f32, BORDER), egui::StrokeKind::Inside);
        p.text(center.center_top() + vec2(0.0, 14.0), Align2::CENTER_TOP, "This PC", FontId::proportional(16.0), TEXT);
        p.text(center.center_top() + vec2(0.0, 36.0), Align2::CENTER_TOP, format!("{} devices", devices.len()), FontId::proportional(12.0), MUTED);
        let (summary, color) = match (n_problems, n_updates) {
            (0, 0) => ("Everything is up to date".to_string(), OK),
            (0, u) => (format!("{u} update{} available", if u == 1 { "" } else { "s" }), WARN),
            (b, _) => (format!("{b} device{} need fixing", if b == 1 { "" } else { "s" }), ERR),
        };
        p.text(center.center_top() + vec2(0.0, 54.0), Align2::CENTER_TOP, summary, FontId::proportional(12.0), color);
    });
    clicked
}

fn style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.window_stroke = Stroke::new(1.0_f32, BORDER);
    v.extreme_bg_color = Color32::from_rgb(12, 14, 18);
    v.faint_bg_color = Color32::from_rgb(20, 23, 29);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    v.hyperlink_color = ACCENT;
    v.override_text_color = None;
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = CornerRadius::same(4); // 6+ turns 14px checkboxes into circles
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    v.widgets.inactive.weak_bg_fill = RAISED;
    v.widgets.inactive.bg_fill = RAISED;
    v.widgets.hovered.weak_bg_fill = mix(RAISED, BORDER, 0.8);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, mix(BORDER, ACCENT, 0.5));
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.text_styles.insert(TextStyle::Body, FontId::proportional(14.0));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(14.0));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
        s.text_styles.insert(TextStyle::Monospace, FontId::monospace(13.0));
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(12.0, 5.0);
        s.spacing.interact_size.y = 28.0;
        s.animation_time = 0.15;
    });
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.sys.gpu_tick(frame.gl(), ctx);
        self.app_update.poll();
        let data = self.data.clone();
        let mut d = data.lock().unwrap();
        let now = ctx.input(|i| i.time);
        let t = now as f32;

        if d.scans != self.seen_scans {
            self.seen_scans = d.scans;
            self.scanned_at = Some(now);
            // Pre-tick real upgrades; vendor drivers that replace Windows' own stay opt-in.
            self.picked = d.updates.iter().filter(|u| !u.vendor_swap).map(|u| u.key.clone()).collect();
        }
        if self.was_busy && !d.busy {
            self.toast = Some((d.status.clone(), now));
        }
        self.was_busy = d.busy;
        let fade = ((now - self.page_at) / 0.18).clamp(0.0, 1.0) as f32;
        let map_anim = self.page == Page::Map && now - self.page_at < 1.5;
        if d.busy || fade < 1.0 || map_anim || self.toast.is_some() {
            ctx.request_repaint(); // animate only while something moves
        }

        let mut actions: Vec<Action> = Vec::new();
        let mut enabled = d.enabled;
        let mut clear_folder = false;
        {
            let d = &*d;
            let mut by_dev: HashMap<&str, Vec<&Update>> = HashMap::new();
            for u in &d.updates {
                if let Some(dev) = &u.device {
                    by_dev.entry(dev.as_str()).or_default().push(u);
                }
            }
            let n_upd = |dev: &Device| by_dev.get(dev.instance.as_str()).map_or(0, |v| v.len());
            let problems: Vec<&Device> = d.devices.iter().filter(|x| x.problem != 0).collect();
            let first_scan = d.scans == 0;

            // ---------- Sidebar ----------
            egui::SidePanel::left("nav").exact_width(216.0).resizable(false).frame(egui::Frame::new().fill(SIDEBAR).inner_margin(Margin::symmetric(12, 16))).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    mark(ui, 26.0);
                    ui.add_space(2.0);
                    ui.label(RichText::new("Flash").size(17.0).strong().color(TEXT));
                });
                ui.add_space(22.0);
                ui.spacing_mut().item_spacing.y = 2.0;
                let monitoring = self.sys.monitor_running();
                let items = [
                    (Some("DRIVERS"), Page::Overview, "Overview", None),
                    (None, Page::Map, "Health map", None),
                    (None, Page::Updates, "Updates", Some((d.updates.len(), WARN))),
                    (None, Page::Devices, "Devices", Some((d.devices.len(), MUTED))),
                    (None, Page::Problems, "Problems", Some((problems.len(), ERR))),
                    (Some("SYSTEM"), Page::Health, "Device health", None),
                    (None, Page::Storage, "Storage & cleanup", None),
                    (None, Page::Monitor, if monitoring { "System monitor  ●" } else { "System monitor" }, None),
                    (Some(""), Page::Activity, "Activity", None),
                ];
                for (heading, p, label, count) in items {
                    if let Some(h) = heading {
                        ui.add_space(if h.is_empty() { 10.0 } else { 14.0 });
                        if !h.is_empty() {
                            ui.label(RichText::new(h).size(11.0).strong().color(MUTED));
                            ui.add_space(2.0);
                        }
                    }
                    if nav(ui, label, count, self.page == p).clicked() {
                        self.go(p, now);
                    }
                }
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    self.app_update.sidebar(ui);
                    let label = if d.busy { "Scanning..." } else { "Scan for updates" };
                    if ui.add_enabled(!d.busy, primary(label).min_size(vec2(ui.available_width(), 34.0))).clicked() {
                        actions.push(Action::Scan);
                    }
                    if d.busy {
                        progress(ui, t);
                    }
                    let line = if d.busy {
                        d.status.clone()
                    } else {
                        self.scanned_at.map(|at| format!("Last checked {}", ago(now - at))).unwrap_or_default()
                    };
                    let open = ui.ctx().animate_bool_with_time(Id::new("verbose"), self.verbose, 0.15);
                    if disclosure(ui, open, &line).on_hover_text(if self.verbose { "Hide details" } else { "Show what Flash is doing" }).clicked() {
                        self.verbose = !self.verbose;
                    }
                    if open > 0.0 {
                        // Bottom-up layout: this lands above the status line it expands from.
                        ui.multiply_opacity(open);
                        egui::Frame::new().fill(RAISED).stroke(Stroke::new(1.0_f32, BORDER)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                                ui.spacing_mut().item_spacing.y = 3.0;
                                for src in Source::ALL {
                                    let s = &d.source_status[src as usize];
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(src.name()).size(11.5).color(TEXT));
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            ui.add(egui::Label::new(RichText::new(if s.is_empty() { "-" } else { s }).size(11.5).color(MUTED)).truncate());
                                        });
                                    });
                                }
                                ui.separator();
                                egui::ScrollArea::vertical().max_height(150.0 * open).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                                    let lines: Vec<&str> = d.log.lines().collect();
                                    for l in &lines[lines.len().saturating_sub(40)..] {
                                        ui.add(egui::Label::new(RichText::new(*l).size(11.0).monospace().color(MUTED)).wrap());
                                    }
                                });
                            });
                        });
                    }
                });
            });

            // ---------- Device details ----------
            let selected = self.device.as_ref().and_then(|id| d.devices.iter().find(|x| &x.instance == id));
            if self.page == Page::Devices || self.page == Page::Problems {
                if let Some(dev) = selected {
                    egui::SidePanel::right("details")
                        .default_width(360.0)
                        .width_range(300.0..=520.0)
                        .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(Margin::same(18)).stroke(Stroke::new(1.0_f32, BORDER)))
                        .show(ctx, |ui| {
                            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.add(egui::Label::new(RichText::new(&dev.name).size(17.0).strong().color(TEXT)).wrap());
                                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                                        if ui.small_button("Close").clicked() {
                                            self.device = None;
                                        }
                                    });
                                });
                                ui.label(RichText::new(format!("{} · {}", dev.class, dev.manufacturer)).color(MUTED));
                                ui.add_space(6.0);
                                let ups = by_dev.get(dev.instance.as_str()).cloned().unwrap_or_default();
                                let (c, s, _) = device_status(dev, ups.len());
                                status(ui, c, s);
                                ui.add_space(14.0);

                                ui.label(RichText::new("DRIVER").size(11.5).strong().color(MUTED));
                                egui::Grid::new("drv").num_columns(2).spacing(vec2(16.0, 6.0)).show(ui, |ui| {
                                    kv(ui, "Provider", &dev.provider);
                                    kv(ui, "Version", &dev.version);
                                    kv(ui, "Date", &dev.date);
                                    kv(ui, "INF file", &dev.inf);
                                    kv(ui, "Signed by", &dev.signer);
                                });

                                ui.add_space(14.0);
                                ui.label(RichText::new("AVAILABLE UPDATES").size(11.5).strong().color(MUTED));
                                if ups.is_empty() {
                                    ui.label(RichText::new("None found by the enabled sources.").color(MUTED));
                                }
                                for u in ups {
                                    card(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        let mut on = self.picked.contains(&u.key);
                                        if ui.checkbox(&mut on, RichText::new(format!("{}  ({})", u.version, u.date)).strong().color(TEXT)).changed() {
                                            if on { self.picked.insert(u.key.clone()); } else { self.picked.remove(&u.key); }
                                        }
                                        ui.add(egui::Label::new(RichText::new(&u.title).color(MUTED)).wrap());
                                        let tag = if u.vendor_swap { format!("{} · replaces the Windows built-in driver", u.source.name()) } else { u.source.name().to_string() };
                                        ui.label(RichText::new(tag).size(12.0).color(if u.vendor_swap { WARN } else { MUTED }));
                                    });
                                }

                                ui.add_space(14.0);
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("HARDWARE IDS").size(11.5).strong().color(MUTED));
                                    if ui.small_button("Copy").clicked() {
                                        ui.ctx().copy_text(dev.hwids.join("\r\n"));
                                    }
                                });
                                for id in &dev.hwids {
                                    ui.add(egui::Label::new(RichText::new(id).monospace().color(TEXT)).wrap());
                                }
                                if !dev.compat.is_empty() {
                                    egui::CollapsingHeader::new(RichText::new(format!("Compatible IDs ({})", dev.compat.len())).color(MUTED)).show(ui, |ui| {
                                        for id in &dev.compat {
                                            ui.add(egui::Label::new(RichText::new(id).monospace().color(MUTED)).wrap());
                                        }
                                    });
                                }
                                ui.add_space(8.0);
                                ui.label(RichText::new("INSTANCE").size(11.5).strong().color(MUTED));
                                ui.add(egui::Label::new(RichText::new(&dev.instance).monospace().color(MUTED)).wrap());
                                ui.add_space(14.0);
                                if ui.button("Open Device Manager").clicked() {
                                    flash::run_hidden("cmd", &["/c", "start", "", "devmgmt.msc"]);
                                }
                            });
                        });
                }
            }

            // ---------- Main page ----------
            egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(28, 24))).show(ctx, |ui| {
                ui.multiply_opacity(fade);
                match self.page {
                    Page::Overview => {
                        page_header(ui, "Overview", "Driver health for this PC.", |_| {});
                        let gap = 12.0;
                        let w = ((ui.available_width() - 3.0 * gap) / 4.0).max(150.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = gap;
                            let last = self.scanned_at.map(|a| ago(now - a)).unwrap_or_else(|| "Scanning".into());
                            if metric(ui, w, Some(d.devices.len()), "", "Devices", TEXT).clicked() {
                                self.go(Page::Devices, now);
                            }
                            let ups = (!first_scan).then_some(d.updates.len());
                            if metric(ui, w, ups, "-", "Updates available", if d.updates.is_empty() { TEXT } else { WARN }).clicked() {
                                self.go(Page::Updates, now);
                            }
                            if metric(ui, w, Some(problems.len()), "", "Devices with problems", if problems.is_empty() { TEXT } else { ERR }).clicked() {
                                self.go(Page::Problems, now);
                            }
                            if metric(ui, w, None, &last, "Last checked", TEXT).clicked() {
                                self.go(Page::Activity, now);
                            }
                        });
                        ui.add_space(20.0);

                        ui.label(RichText::new("Update sources").size(15.0).strong().color(TEXT));
                        ui.label(RichText::new("Flash checks every enabled source on each scan. Turn one off to skip it next time.").color(MUTED));
                        ui.add_space(8.0);
                        card(ui, |ui| {
                            ui.set_width(ui.available_width());
                            for (row, src) in Source::ALL.into_iter().enumerate() {
                                if row > 0 {
                                    ui.add_space(4.0);
                                    ui.separator();
                                    ui.add_space(4.0);
                                }
                                let i = src as usize;
                                ui.horizontal(|ui| {
                                    ui.checkbox(&mut enabled[i], "");
                                    let w = (ui.available_width() - 230.0).max(200.0);
                                    ui.allocate_ui_with_layout(vec2(w, 0.0), Layout::top_down(Align::Min), |ui| {
                                        ui.set_width(w);
                                        ui.label(RichText::new(src.name()).strong().color(TEXT));
                                        ui.label(RichText::new(src.about()).color(MUTED));
                                        if src == Source::Folder {
                                            ui.horizontal(|ui| {
                                                if ui.add_enabled(!d.busy, egui::Button::new("Choose folder...")).clicked() {
                                                    actions.push(Action::Folder);
                                                }
                                                if let Some(f) = &d.folder {
                                                    ui.label(RichText::new(f).monospace().color(MUTED));
                                                    if ui.small_button("Clear").clicked() {
                                                        clear_folder = true;
                                                    }
                                                }
                                            });
                                        }
                                    });
                                    let s = &d.source_status[i];
                                    let color = if s.starts_with("Couldn't") { ERR } else if s.starts_with("Checking") || s.starts_with("Reading") || s.starts_with("Waiting") { ACCENT } else { MUTED };
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.label(RichText::new(if s.is_empty() { "-" } else { s }).color(color));
                                    });
                                });
                            }
                        });
                        ui.add_space(20.0);

                        ui.label(RichText::new("Tools").size(15.0).strong().color(TEXT));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.add_enabled(!d.busy, egui::Button::new("Back up drivers...")).on_hover_text("Exports every third-party driver to a folder, so you can reinstall them later.").clicked() {
                                actions.push(Action::Backup);
                            }
                            if ui.button("Open Device Manager").clicked() {
                                flash::run_hidden("cmd", &["/c", "start", "", "devmgmt.msc"]);
                            }
                            if ui.button("System Restore").on_hover_text("Roll the PC back to an earlier restore point.").clicked() {
                                flash::run_hidden("cmd", &["/c", "start", "", "rstrui.exe"]);
                            }
                        });
                    }

                    Page::Map => {
                        page_header(ui, "Health map", "Each branch is a kind of device. Click a branch or device to open it.", |ui| {
                            status(ui, ERR, "Needs fixing");
                            status(ui, WARN, "Update available");
                            status(ui, OK, "Up to date");
                        });
                        if d.devices.is_empty() {
                            empty(ui, "Reading devices...", "The map appears once the first scan has read your devices.");
                        } else if let Some((p, dev)) = health_map(ui, &d.devices, &by_dev, (now - self.page_at) as f32, d.updates.len(), problems.len()) {
                            self.purpose = p.to_string();
                            self.search.clear();
                            self.only_updates = false;
                            self.device = dev;
                            self.go(Page::Devices, now);
                        }
                    }

                    Page::Updates => {
                        let sub = format!("{} found across {} source(s). Pick what to install.", d.updates.len(), enabled.iter().filter(|e| **e).count());
                        page_header(ui, "Updates", &sub, |ui| {
                            let n = d.updates.iter().filter(|u| self.picked.contains(&u.key)).count();
                            if ui.add_enabled(!d.busy && n > 0, primary(&format!("Install {n} selected"))).clicked() {
                                self.confirm = true;
                            }
                            if ui.button("Select none").clicked() {
                                self.picked.clear();
                            }
                            if ui.button("Select all").clicked() {
                                self.picked = d.updates.iter().map(|u| u.key.clone()).collect();
                            }
                        });
                        if d.updates.is_empty() {
                            if first_scan {
                                empty(ui, "Checking for updates...", "This takes about a minute the first time.");
                            } else {
                                empty(ui, "Your drivers are up to date", "None of the enabled sources has a newer driver for this PC.");
                            }
                        } else {
                            let mut open: Option<String> = None;
                            TableBuilder::new(ui)
                                .id_salt("updates")
                                .striped(true)
                                .resizable(true)
                                .sense(Sense::click())
                                .cell_layout(Layout::left_to_right(Align::Center))
                                .column(Column::exact(28.0))
                                .column(Column::remainder().at_least(180.0).clip(true))
                                .column(Column::initial(120.0).clip(true))
                                .column(Column::initial(120.0).clip(true))
                                .column(Column::initial(90.0))
                                .column(Column::initial(120.0).clip(true))
                                .header(30.0, |mut h| {
                                    h.col(|_| {});
                                    for c in ["DEVICE", "INSTALLED", "AVAILABLE", "RELEASED", "SOURCE"] {
                                        h.col(|ui| header_cell(ui, c));
                                    }
                                })
                                .body(|body| {
                                    body.rows(30.0, d.updates.len(), |mut row| {
                                        let u = &d.updates[row.index()];
                                        let dev = u.device.as_ref().and_then(|id| d.devices.iter().find(|x| &x.instance == id));
                                        row.col(|ui| {
                                            let mut on = self.picked.contains(&u.key);
                                            if ui.checkbox(&mut on, "").changed() {
                                                if on { self.picked.insert(u.key.clone()); } else { self.picked.remove(&u.key); }
                                            }
                                        });
                                        row.col(|ui| {
                                            match dev {
                                                Some(x) => ui.label(RichText::new(&x.name).color(TEXT)),
                                                None => ui.label(RichText::new("Device not connected").color(MUTED)),
                                            }
                                            .on_hover_text(format!("{}\n{}  {}", u.title, u.hwid, u.size));
                                            if u.vendor_swap {
                                                ui.label(RichText::new("vendor driver").size(12.0).color(WARN)).on_hover_text("Replaces the Windows built-in driver with the hardware maker's driver. Not ticked by default.");
                                            }
                                        });
                                        row.col(|ui| {
                                            let v = dev.map_or("", |x| x.version.as_str());
                                            ui.label(RichText::new(if v.is_empty() { "None" } else { v }).monospace().color(MUTED));
                                        });
                                        row.col(|ui| {
                                            ui.label(RichText::new(if u.version.is_empty() { "-" } else { &u.version }).monospace().color(TEXT));
                                        });
                                        row.col(|ui| {
                                            ui.label(RichText::new(&u.date).color(MUTED));
                                        });
                                        row.col(|ui| {
                                            ui.label(RichText::new(u.source.name()).color(MUTED));
                                        });
                                        if row.response().double_clicked() {
                                            open = u.device.clone();
                                        }
                                    });
                                });
                            if let Some(id) = open {
                                self.device = Some(id);
                                self.go(Page::Devices, now);
                            }
                        }
                    }

                    Page::Devices => {
                        page_header(ui, "Devices", "Every device Windows has a driver for, or should, grouped by what it's for.", |_| {});
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search name, provider, version or hardware ID").desired_width(320.0));
                            egui::ComboBox::from_id_salt("purpose")
                                .selected_text(if self.purpose.is_empty() { "All purposes" } else { &self.purpose })
                                .width(170.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.purpose, String::new(), "All purposes");
                                    for p in PURPOSES.iter().filter(|p| d.devices.iter().any(|x| purpose(&x.class) == **p)) {
                                        ui.selectable_value(&mut self.purpose, p.to_string(), *p);
                                    }
                                });
                            ui.checkbox(&mut self.only_updates, "Only with updates");
                        });
                        ui.add_space(10.0);

                        let q = self.search.to_lowercase();
                        let mut list: Vec<&Device> = d
                            .devices
                            .iter()
                            .filter(|x| self.purpose.is_empty() || purpose(&x.class) == self.purpose)
                            .filter(|x| !self.only_updates || n_upd(x) > 0)
                            .filter(|x| {
                                q.is_empty()
                                    || [&x.name, &x.provider, &x.version, &x.manufacturer, &x.class].iter().any(|f| f.to_lowercase().contains(&q))
                                    || x.hwids.iter().any(|h| h.to_lowercase().contains(&q))
                            })
                            .collect();
                        let (col, asc) = self.sort;
                        list.sort_by(|a, b| {
                            let o = match col {
                                Sort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                                Sort::Class => a.class.cmp(&b.class).then(a.name.cmp(&b.name)),
                                Sort::Version => flash::inf::version_parts(&a.version).cmp(&flash::inf::version_parts(&b.version)),
                                Sort::Date => a.date.cmp(&b.date),
                                Sort::Provider => a.provider.to_lowercase().cmp(&b.provider.to_lowercase()),
                                Sort::Status => device_status(a, n_upd(a)).2.cmp(&device_status(b, n_upd(b)).2),
                            };
                            if asc { o } else { o.reverse() }
                        });

                        if list.is_empty() {
                            empty(ui, if first_scan && d.busy { "Reading devices..." } else { "No matching devices" }, "Try a different search or purpose.");
                        } else {
                            let mut clicked: Option<String> = None;
                            let mut sort = self.sort;
                            // Fixed column widths so the shared header lines up with every group's table.
                            let gap = ui.spacing().item_spacing.x;
                            let indent = ui.spacing().indent;
                            let fixed = [120.0, 130.0, 90.0, 150.0, 150.0];
                            let name_w = (ui.available_width() - indent - 14.0 - fixed.iter().sum::<f32>() - 5.0 * gap).max(160.0);
                            let widths = [name_w, fixed[0], fixed[1], fixed[2], fixed[3], fixed[4]];
                            ui.horizontal(|ui| {
                                ui.add_space(indent);
                                for ((text, c), w) in [("DEVICE", Sort::Name), ("CLASS", Sort::Class), ("VERSION", Sort::Version), ("DATE", Sort::Date), ("PROVIDER", Sort::Provider), ("STATUS", Sort::Status)].into_iter().zip(widths) {
                                    ui.allocate_ui_with_layout(vec2(w, 24.0), Layout::left_to_right(Align::Center), |ui| {
                                        ui.set_min_width(w);
                                        sort_header(ui, text, c, &mut sort);
                                    });
                                }
                            });
                            ui.separator();
                            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                                for p in PURPOSES {
                                    let group: Vec<&Device> = list.iter().copied().filter(|x| purpose(&x.class) == p).collect();
                                    if group.is_empty() {
                                        continue;
                                    }
                                    let ups = group.iter().filter(|x| x.problem == 0 && n_upd(x) > 0).count();
                                    let bad = group.iter().filter(|x| x.problem != 0 && x.problem != 22).count();
                                    let plural = |n: usize, w: &str| if n == 1 { format!("{n} {w}") } else { format!("{n} {w}s") };
                                    let mut job = egui::text::LayoutJob::default();
                                    job.append(p, 0.0, egui::TextFormat::simple(FontId::proportional(14.5), TEXT));
                                    job.append(&format!("   {}", plural(group.len(), "device")), 0.0, egui::TextFormat::simple(FontId::proportional(13.0), MUTED));
                                    if ups > 0 {
                                        job.append(&format!("  ·  {}", plural(ups, "update")), 0.0, egui::TextFormat::simple(FontId::proportional(13.0), WARN));
                                    }
                                    if bad > 0 {
                                        job.append(&format!("  ·  {}", plural(bad, "problem")), 0.0, egui::TextFormat::simple(FontId::proportional(13.0), ERR));
                                    }
                                    egui::CollapsingHeader::new(job).id_salt(("group", p)).default_open(true).show(ui, |ui| {
                                        let mut t = TableBuilder::new(ui).id_salt(("devices", p)).vscroll(false).striped(true).sense(Sense::click()).cell_layout(Layout::left_to_right(Align::Center));
                                        for w in widths {
                                            t = t.column(Column::exact(w).clip(true));
                                        }
                                        t.body(|body| {
                                            body.rows(30.0, group.len(), |mut row| {
                                                let dev = group[row.index()];
                                                row.set_selected(self.device.as_deref() == Some(dev.instance.as_str()));
                                                row.col(|ui| {
                                                    ui.label(RichText::new(&dev.name).color(TEXT));
                                                });
                                                row.col(|ui| {
                                                    ui.label(RichText::new(&dev.class).color(MUTED));
                                                });
                                                row.col(|ui| {
                                                    ui.label(RichText::new(if dev.version.is_empty() { "None" } else { &dev.version }).monospace().color(MUTED));
                                                });
                                                row.col(|ui| {
                                                    ui.label(RichText::new(&dev.date).color(MUTED));
                                                });
                                                row.col(|ui| {
                                                    ui.label(RichText::new(&dev.provider).color(MUTED));
                                                });
                                                row.col(|ui| {
                                                    let (c, s, _) = device_status(dev, n_upd(dev));
                                                    status(ui, c, s);
                                                });
                                                if row.response().clicked() {
                                                    clicked = Some(dev.instance.clone());
                                                }
                                            });
                                        });
                                    });
                                    ui.add_space(4.0);
                                }
                            });
                            self.sort = sort;
                            if let Some(id) = clicked {
                                self.device = if self.device.as_ref() == Some(&id) { None } else { Some(id) };
                            }
                        }
                    }

                    Page::Problems => {
                        page_header(ui, "Problems", "Devices Device Manager flags with a warning, and whether Flash found a driver for them.", |_| {});
                        if problems.is_empty() {
                            empty(ui, "No device problems", "Every device has a working driver.");
                        } else {
                            let mut clicked: Option<String> = None;
                            TableBuilder::new(ui)
                                .id_salt("problems")
                                .striped(true)
                                .sense(Sense::click())
                                .cell_layout(Layout::left_to_right(Align::Center))
                                .column(Column::initial(320.0).clip(true))
                                .column(Column::initial(240.0).clip(true))
                                .column(Column::initial(70.0))
                                .column(Column::remainder().clip(true))
                                .header(30.0, |mut h| {
                                    for c in ["DEVICE", "PROBLEM", "CODE", "FIX"] {
                                        h.col(|ui| header_cell(ui, c));
                                    }
                                })
                                .body(|body| {
                                    body.rows(30.0, problems.len(), |mut row| {
                                        let dev = problems[row.index()];
                                        row.set_selected(self.device.as_deref() == Some(dev.instance.as_str()));
                                        row.col(|ui| {
                                            ui.label(RichText::new(&dev.name).color(TEXT));
                                        });
                                        row.col(|ui| status(ui, if dev.problem == 22 { MUTED } else { ERR }, problem_text(dev.problem)));
                                        row.col(|ui| {
                                            ui.label(RichText::new(dev.problem.to_string()).monospace().color(MUTED));
                                        });
                                        row.col(|ui| {
                                            let fix = match (n_upd(dev), dev.problem) {
                                                (n, _) if n > 0 => "Driver available on the Updates page",
                                                (_, 22) => "Enable it in Device Manager",
                                                (_, 14) => "Restart the PC",
                                                _ => "No driver found. Try a vendor folder or Device Manager",
                                            };
                                            ui.label(RichText::new(fix).color(MUTED));
                                        });
                                        if row.response().clicked() {
                                            clicked = Some(dev.instance.clone());
                                        }
                                    });
                                });
                            if let Some(id) = clicked {
                                self.device = Some(id);
                            }
                        }
                    }

                    Page::Health => self.sys.health_page(ui, ctx, &d.devices),
                    Page::Storage => self.sys.storage_page(ui, ctx),
                    Page::Monitor => self.sys.monitor_page(ui, ctx, now),

                    Page::Activity => {
                        page_header(ui, "Activity", "Everything Flash has done this session.", |ui| {
                            if ui.button("Copy log").clicked() {
                                ui.ctx().copy_text(d.log.clone());
                            }
                        });
                        card(ui, |ui| {
                            egui::ScrollArea::vertical().auto_shrink(false).stick_to_bottom(true).show(ui, |ui| {
                                ui.add(egui::Label::new(RichText::new(&d.log).monospace().color(MUTED)).wrap());
                            });
                        });
                    }
                }
            });

            // ---------- Install confirmation ----------
            self.sys.modals(ctx);
            if self.confirm {
                let chosen: Vec<&Update> = d.updates.iter().filter(|u| self.picked.contains(&u.key)).collect();
                let modal = egui::Modal::new(Id::new("confirm")).frame(egui::Frame::new().fill(SURFACE).stroke(Stroke::new(1.0_f32, BORDER)).corner_radius(10.0).inner_margin(22.0)).show(ctx, |ui| {
                    ui.set_width(420.0);
                    ui.label(RichText::new(format!("Install {} driver update(s)?", chosen.len())).size(17.0).strong().color(TEXT));
                    ui.add_space(8.0);
                    for src in Source::ALL {
                        let n = chosen.iter().filter(|u| u.source == src).count();
                        if n > 0 {
                            ui.label(RichText::new(format!("{n} from {}", src.name())).color(MUTED));
                        }
                    }
                    let swaps = chosen.iter().filter(|u| u.vendor_swap).count();
                    if swaps > 0 {
                        ui.label(RichText::new(format!("{swaps} will replace a Windows built-in driver with the vendor's.")).color(WARN));
                    }
                    ui.add_space(10.0);
                    ui.checkbox(&mut self.restore_point, "Create a restore point first (recommended)");
                    ui.label(RichText::new("Some drivers need a restart to finish installing.").size(12.5).color(MUTED));
                    ui.add_space(14.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(primary("Install")).clicked() {
                            actions.push(Action::Install(chosen.iter().map(|u| (*u).clone()).collect(), self.restore_point));
                            self.confirm = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm = false;
                        }
                    });
                });
                if modal.should_close() {
                    self.confirm = false;
                }
            }
        }

        d.enabled = enabled;
        if clear_folder {
            d.folder = None;
            d.updates.retain(|u| u.source != Source::Folder);
        }
        drop(d);

        // Toast: slides up and fades in, holds, then fades out.
        if let Some((msg, t0)) = self.toast.clone() {
            let e = (now - t0) as f32;
            if e > 4.0 || msg.is_empty() {
                self.toast = None;
            } else {
                let a = (e / 0.2).min(1.0).min((4.0 - e) / 0.4);
                let slide = (1.0 - (e / 0.2).min(1.0)) * 16.0;
                egui::Area::new(Id::new("toast")).anchor(Align2::RIGHT_BOTTOM, vec2(-24.0, -24.0 + slide)).interactable(false).show(ctx, |ui| {
                    ui.multiply_opacity(a);
                    egui::Frame::new().fill(RAISED).stroke(Stroke::new(1.0_f32, BORDER)).corner_radius(8.0).inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
                        ui.label(RichText::new(msg).color(TEXT));
                    });
                });
            }
        }

        for a in actions {
            match a {
                Action::Scan => self.run(ctx, scan),
                Action::Install(ups, restore) => self.run(ctx, move |d| install(d, &ups, restore)),
                Action::Folder => self.run(ctx, check_folder),
                Action::Backup => self.run(ctx, backup),
            }
        }
        let busy = self.data.lock().unwrap().busy || self.sys.update_blocked();
        self.app_update.dialog(ctx, busy);
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 720.0])
            .with_min_inner_size([960.0, 560.0])
            .with_title("Flash")
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/flash.png")).expect("bad icon png")),
        ..Default::default()
    };
    eframe::run_native(
        "Flash",
        options,
        Box::new(|cc| {
            style(&cc.egui_ctx);
            let app = App {
                data: Shared::default(),
                page: Page::Overview,
                page_at: 0.0,
                search: String::new(),
                purpose: String::new(),
                only_updates: false,
                sort: (Sort::Status, true),
                device: None,
                picked: HashSet::new(),
                seen_scans: 0,
                scanned_at: None,
                restore_point: true,
                confirm: false,
                was_busy: false,
                toast: None,
                sys: system_ui::SysUi::new(),
                app_update: app_update_ui::AppUpdateUi::new(&cc.egui_ctx),
                verbose: false,
            };
            app.run(&cc.egui_ctx, scan); // scan right away on launch
            Ok(Box::new(app))
        }),
    )
}
