//! Closing Flash while it's working keeps it running instead of quitting. On X11 the window
//! hides to the tray (a StatusNotifierItem, the protocol AppIndicator uses); Wayland can't
//! hide windows, so there it minimizes. A notification says so, with Stop scan and Quit
//! Flash buttons, and another says when the job is done.
use eframe::egui;
use flash::{is_scanning, stop_scan};
use ksni::blocking::TrayMethods;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

static HIDDEN: AtomicBool = AtomicBool::new(false);
static CTX: OnceLock<egui::Context> = OnceLock::new();

pub fn hidden() -> bool {
    HIDDEN.load(SeqCst)
}

/// Brings the window back from the tray or the taskbar.
pub fn show_window() {
    HIDDEN.store(false, SeqCst);
    if let Some(ctx) = CTX.get() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }
}

fn quit() {
    std::process::exit(0);
}

/// Wayland sessions can minimize a window but not hide it.
fn wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Flash's icon as a file, for notifications (works for the AppImage and tarball too).
fn icon_path() -> String {
    let path = flash::extras::data_dir().join("flash.png");
    if !path.exists() {
        std::fs::create_dir_all(flash::extras::data_dir()).ok();
        std::fs::write(&path, include_bytes!("../assets/flash.png")).ok();
    }
    path.to_string_lossy().into_owned()
}

struct FlashTray;

impl ksni::Tray for FlashTray {
    fn id(&self) -> String {
        "flash".into()
    }
    fn title(&self) -> String {
        "Flash".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let Ok(png) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/flash.png")) else { return vec![] };
        // RGBA to ARGB, as the StatusNotifierItem spec wants.
        let data = png.rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
        vec![ksni::Icon { width: png.width as i32, height: png.height as i32, data }]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: if is_scanning() { "Flash · scanning".into() } else { "Flash · working".into() }, ..Default::default() }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        show_window();
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        let mut items: Vec<ksni::MenuItem<Self>> = vec![StandardItem { label: "Open Flash".into(), activate: Box::new(|_| show_window()), ..Default::default() }.into()];
        if is_scanning() {
            items.push(StandardItem { label: "Stop scan".into(), activate: Box::new(|_| stop_scan()), ..Default::default() }.into());
        }
        items.push(StandardItem { label: "Quit Flash".into(), activate: Box::new(|_| quit()), ..Default::default() }.into());
        items
    }
}

/// Shows a desktop notification with buttons; `on` gets the button's id ("default" for a click).
fn notify(title: String, body: String, buttons: Vec<(&'static str, &'static str)>, on: impl FnOnce(&str) + Send + 'static) {
    std::thread::spawn(move || {
        let mut n = notify_rust::Notification::new();
        n.appname("Flash").summary(&title).body(&body).icon(&icon_path()).hint(notify_rust::Hint::DesktopEntry("flash".into()));
        n.action("default", "Open Flash");
        for (id, label) in buttons {
            n.action(id, label);
        }
        if let Ok(handle) = n.show() {
            handle.wait_for_action(on);
        }
    });
}

pub struct Tray {
    handle: Option<ksni::blocking::Handle<FlashTray>>,
}

impl Tray {
    pub fn new() -> Tray {
        Tray { handle: None }
    }

    /// Hides (X11) or minimizes (Wayland) the window and says so in a notification.
    pub fn hide(&mut self, ctx: &egui::Context, _frame: &eframe::Frame) -> bool {
        CTX.get_or_init(|| ctx.clone());
        if self.handle.is_none() {
            self.handle = FlashTray.spawn().ok();
        }
        // Without a tray icon, only minimize, so the taskbar can always bring it back.
        let tray = self.handle.is_some();
        if wayland() || !tray {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        HIDDEN.store(true, SeqCst);
        let scanning = is_scanning();
        let title = if scanning { "Flash is still scanning" } else { "Flash is still working" };
        let body = if wayland() || !tray { "It's minimized: open it again from the taskbar or the tray icon." } else { "It's been hidden to the tray. Click the Flash icon there to open it." };
        let mut buttons = vec![];
        if scanning {
            buttons.push(("stop", "Stop scan"));
        }
        buttons.push(("quit", "Quit Flash"));
        notify(title.into(), body.into(), buttons, |action| match action {
            "stop" => stop_scan(),
            "quit" => quit(),
            "default" => show_window(),
            _ => {}
        });
        true
    }

    /// Removes the tray icon once the window is back (Wayland: once it's restored from the taskbar).
    pub fn tick(&mut self, ctx: &egui::Context) {
        if hidden() && ctx.input(|i| i.viewport().minimized == Some(false) && i.viewport().focused == Some(true)) {
            HIDDEN.store(false, SeqCst);
        }
        if !hidden() {
            if let Some(h) = self.handle.take() {
                h.shutdown();
            }
        }
    }
}

/// Tells the user a job finished while Flash was hidden. Clicking it opens Flash.
pub fn notify_done(message: &str) {
    notify("Flash finished".into(), message.into(), vec![], |action| {
        if action == "default" {
            show_window();
        }
    });
}

/// Result of the weekly background scan. Clicking it opens Flash.
pub fn notify_scheduled(message: &str) {
    notify("Flash found things to look at".into(), message.into(), vec![], |action| {
        if action == "default" {
            if let Ok(exe) = std::env::current_exe() {
                std::process::Command::new(exe).spawn().ok();
            }
            std::process::exit(0);
        }
    });
}
