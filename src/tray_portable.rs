//! Portable alternative to Windows' off-screen tray parking. The normal
//! taskbar/Dock window remains the primary surface on Linux and macOS.
use std::sync::atomic::{AtomicBool, Ordering};
static SHOW: AtomicBool = AtomicBool::new(false);
static QUIT: AtomicBool = AtomicBool::new(false);
static QUITTING: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
fn show_file() -> Option<std::path::PathBuf> {
    crate::model::config_dir().map(|d| d.join("show.request"))
}
/// Kept generic for compatibility with the legacy frontend's call site. The
/// GPUI frontend owns repaint and activation through its `WindowHandle`.
pub fn set_context<T>(_: &T) {}
pub fn claim_main_instance() -> Option<std::fs::File> {
    match crate::model::claim_window("main") {
        Some(f) => {
            start_keepalive();
            Some(f)
        }
        None => {
            if let Some(p) = show_file() {
                let _ = std::fs::write(p, b"show");
            }
            None
        }
    }
}
pub fn request_show() {
    SHOW.store(true, Ordering::SeqCst);
    nudge_paint();
}
pub fn take_show_request() -> bool {
    SHOW.swap(false, Ordering::SeqCst)
}
pub fn set_hwnd(_: isize) {}
pub fn hwnd() -> isize {
    0
}
pub fn is_parked() -> bool {
    false
}
pub fn quitting() -> bool {
    QUITTING.load(Ordering::SeqCst)
}
pub fn take_quit() -> bool {
    QUIT.swap(false, Ordering::SeqCst)
}
pub fn frame_tick() {}
/// GPUI polls show/quit requests from its app task, so there is no renderer
/// context for this portable compatibility hook to nudge.
pub fn nudge_paint() {}
pub fn start_keepalive() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        while !quitting() {
            if let Some(p) = show_file() {
                if p.exists() {
                    let _ = std::fs::remove_file(p);
                    request_show();
                }
            }
            nudge_paint();
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    });
}
pub fn park() {
    // Portable GPUI builds use the ordinary taskbar/Dock window and do not
    // claim that a window is parked when there is no system tray icon.
}
/// Portable builds use the taskbar/Dock window instead of a tray icon, so a
/// normal close should close the application rather than leave a hidden
/// process with no way to restore it.
pub fn close_main_window() -> bool {
    true
}
pub fn show() {
    // The GPUI app consumes `take_show_request()` and activates its window.
}
// Portable windows are never parked off screen. Called every frame.
pub fn restore() {}
pub fn request_quit() {
    QUITTING.store(true, Ordering::SeqCst);
    QUIT.store(true, Ordering::SeqCst);
    nudge_paint();
}
pub fn install_menu_handler() {}
pub fn tooltip() -> &'static str {
    "Download Craft"
}
pub fn set_titlebar_dark(_: bool) {}
pub fn set_titlebar_color(_: (u8, u8, u8)) {}
pub fn theme_caption(_: bool) {}
pub fn refresh_titlebar() {}
