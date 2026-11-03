//! UI-independent completed-download data and platform actions.

use crate::model::{config_dir, DownloadItem};

fn read_item(id: u64) -> Option<DownloadItem> {
    let data = std::fs::read_to_string(config_dir()?.join("queue.json")).ok()?;
    let items: Vec<DownloadItem> = serde_json::from_str(&data).ok()?;
    items.into_iter().find(|item| item.id == id)
}

pub(crate) fn gpui_item(id: u64) -> Option<DownloadItem> {
    read_item(id)
}

#[cfg(windows)]
pub(crate) fn gpui_open_with(path: &std::path::Path) {
    let _ = crate::platform::silent_command("rundll32.exe")
        .arg("shell32.dll,OpenAs_RunDLL")
        .arg(path)
        .spawn();
}
