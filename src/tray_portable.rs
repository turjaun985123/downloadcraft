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
pub fn quitting() -> bool {
    QUITTING.load(Ordering::SeqCst)
}
pub fn take_quit() -> bool {
    QUIT.swap(false, Ordering::SeqCst)
}
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
/// Portable builds use the taskbar/Dock window instead of a tray icon, so a
/// normal close should close the application rather than leave a hidden
/// process with no way to restore it.
pub fn close_main_window() -> bool {
    true
}
pub fn show() {
    // The GPUI app consumes `take_show_request()` and activates its window.
}
pub fn request_quit() {
    QUITTING.store(true, Ordering::SeqCst);
    QUIT.store(true, Ordering::SeqCst);
    nudge_paint();
}
