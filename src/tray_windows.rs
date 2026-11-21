//! Closing Flash while it's working hides it to the notification area instead of quitting,
//! with a Windows notification that can stop the scan or quit Flash.
use flash::{is_scanning, stop_scan};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::SeqCst};
use tauri_winrt_notification::Toast;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOW, SetForegroundWindow, ShowWindow};

/// Windows shows notifications under this app ID; register_app_id gives it a name and icon.
const APP_ID: &str = "ljcatherine.Flash";

static HWND: AtomicIsize = AtomicIsize::new(0);
static HIDDEN: AtomicBool = AtomicBool::new(false);

pub fn hidden() -> bool {
    HIDDEN.load(SeqCst)
}

/// Brings the window back. Uses Win32 directly: egui doesn't draw frames while hidden.
pub fn show_window() {
    let hwnd = HWND.load(SeqCst);
    if hwnd != 0 {
        // SAFETY: hwnd is Flash's own top-level window, stored by hide().
        unsafe {
            ShowWindow(hwnd as _, SW_SHOW);
            SetForegroundWindow(hwnd as _);
        }
    }
    HIDDEN.store(false, SeqCst);
}

fn quit() {
    std::process::exit(0);
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Unpackaged apps need an app ID registered with a display name and icon before
/// Windows will show their notifications.
fn register_app_id() {
    let Some(dir) = std::env::var_os("LOCALAPPDATA").map(|p| std::path::PathBuf::from(p).join("Flash")) else { return };
    let png = dir.join("flash.png");
    if std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&png, include_bytes!("../assets/flash.png"))).is_err() {
        return;
    }
    let key = wide(&format!(r"Software\Classes\AppUserModelId\{APP_ID}"));
    for (name, value) in [("DisplayName", "Flash".to_string()), ("IconUri", png.to_string_lossy().into_owned())] {
        let (name, value) = (wide(name), wide(&value));
        // SAFETY: NUL-terminated UTF-16 buffers that outlive the call; size is in bytes.
        unsafe {
            RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_SZ, value.as_ptr().cast(), (value.len() * 2) as u32);
        }
    }
}

fn build_icon() -> Option<TrayIcon> {
    let png = eframe::icon_data::from_png_bytes(include_bytes!("../assets/flash.png")).ok()?;
    let icon = Icon::from_rgba(png.rgba, png.width, png.height).ok()?;
    let menu = Menu::new();
    menu.append(&MenuItem::with_id("open", "Open Flash", true, None)).ok()?;
    if is_scanning() {
        menu.append(&MenuItem::with_id("stop", "Stop scan", true, None)).ok()?;
    }
    menu.append(&MenuItem::with_id("quit", "Quit Flash", true, None)).ok()?;
    let tooltip = if is_scanning() { "Flash · scanning" } else { "Flash · working" };
    TrayIconBuilder::new().with_icon(icon).with_tooltip(tooltip).with_menu(Box::new(menu)).with_menu_on_left_click(false).build().ok()
}

/// What a notification click or button does.
fn activated(action: Option<String>) -> tauri_winrt_notification::Result<()> {
    match action.as_deref() {
        Some("stop") => stop_scan(),
        Some("quit") => quit(),
        _ => show_window(),
    }
    Ok(())
}

/// Flash's own native window, for hiding and showing it outside egui.
fn window_handle(frame: &eframe::Frame) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
        _ => None,
    }
}

pub struct Tray {
    icon: Option<TrayIcon>,
}

impl Tray {
    pub fn new() -> Tray {
        // These run on the UI thread's message loop, even while egui isn't drawing.
        TrayIconEvent::set_event_handler(Some(|e| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                show_window();
            }
        }));
        MenuEvent::set_event_handler(Some(|e: MenuEvent| match e.id.0.as_str() {
            "open" => show_window(),
            "stop" => stop_scan(),
            "quit" => quit(),
            _ => {}
        }));
        register_app_id();
        Tray { icon: None }
    }

    /// Hides the window to the notification area and says so in a notification.
    /// Returns false (window left open) if the tray icon can't be created.
    pub fn hide(&mut self, _ctx: &eframe::egui::Context, frame: &eframe::Frame) -> bool {
        let Some(hwnd) = window_handle(frame) else { return false };
        HWND.store(hwnd, SeqCst);
        if self.icon.is_none() {
            self.icon = build_icon();
        }
        if self.icon.is_none() {
            return false; // never vanish without a way back
        }
        // SAFETY: hwnd is Flash's own top-level window, from eframe.
        unsafe { ShowWindow(hwnd as _, SW_HIDE) };
        HIDDEN.store(true, SeqCst);
        let scanning = is_scanning();
        let mut toast = Toast::new(APP_ID)
            .title(if scanning { "Flash is still scanning" } else { "Flash is still working" })
            .text1("It's been minimized to the notification area. Find it under hidden icons on the taskbar.");
        if scanning {
            toast = toast.add_button("Stop scan", "stop");
        }
        toast.add_button("Quit Flash", "quit").on_activated(activated).show().ok();
        true
    }

    /// Removes the tray icon once the window is back.
    pub fn tick(&mut self) {
        if !hidden() && self.icon.is_some() {
            self.icon = None;
        }
    }
}

/// Tells the user a job finished while Flash was hidden. Clicking it opens Flash.
pub fn notify_done(message: &str) {
    Toast::new(APP_ID).title("Flash finished").text1(message).on_activated(activated).show().ok();
}

/// Result of the weekly background scan. Clicking it opens Flash.
pub fn notify_scheduled(message: &str) {
    register_app_id();
    Toast::new(APP_ID)
        .title("Flash found things to look at")
        .text1(message)
        .on_activated(|_| {
            if let Ok(exe) = std::env::current_exe() {
                std::process::Command::new(exe).spawn().ok();
            }
            std::process::exit(0);
        })
        .show()
        .ok();
}
