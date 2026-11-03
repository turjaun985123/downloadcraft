//! UI-independent preferences and queue wake-up operations shared by capture
//! confirmation views.

use crate::model::{atomic_write, config_dir};

fn wake_path() -> Option<std::path::PathBuf> {
    config_dir().map(|path| path.join("wake.json"))
}

fn ui_path() -> Option<std::path::PathBuf> {
    config_dir().map(|path| path.join("ui.json"))
}

#[derive(Debug, Default)]
pub(crate) struct UiPrefs {
    pub(crate) save_dir: String,
    pub(crate) connections: usize,
    pub(crate) category_folders: bool,
    pub(crate) user_agent: String,
    pub(crate) proxy_url: String,
    pub(crate) timeout_secs: u64,
    pub(crate) rename_template: String,
    pub(crate) folder_rules: Vec<crate::model::FolderRule>,
}

pub(crate) fn load_prefs() -> UiPrefs {
    let mut prefs = UiPrefs {
        connections: 8,
        category_folders: true,
        timeout_secs: 60,
        ..Default::default()
    };
    let value: serde_json::Value = ui_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null);
    if let Some(value) = value.get("save_dir").and_then(|value| value.as_str()) {
        if !value.trim().is_empty() {
            prefs.save_dir = value.to_string();
        }
    }
    if prefs.save_dir.is_empty() {
        prefs.save_dir = dirs::download_dir()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| crate::model::default_download_dir().to_string_lossy().into_owned());
    }
    if let Some(value) = value.get("connections_default").and_then(|value| value.as_u64()) {
        prefs.connections = (value as usize).clamp(1, 32);
    }
    if let Some(value) = value.get("category_folders").and_then(|value| value.as_bool()) {
        prefs.category_folders = value;
    }
    if let Some(value) = value.get("user_agent").and_then(|value| value.as_str()) {
        prefs.user_agent = value.to_string();
    }
    if let Some(value) = value.get("proxy_url").and_then(|value| value.as_str()) {
        prefs.proxy_url = value.to_string();
    }
    if let Some(value) = value.get("timeout_secs").and_then(|value| value.as_u64()) {
        prefs.timeout_secs = value.clamp(5, 600);
    }
    if let Some(value) = value.get("rename_template").and_then(|value| value.as_str()) {
        if !value.trim().is_empty() {
            prefs.rename_template = value.to_string();
        }
    }
    if prefs.rename_template.trim().is_empty() {
        prefs.rename_template = "{name}".to_string();
    }
    if let Some(items) = value.get("folder_rules").and_then(|value| value.as_array()) {
        let rules: Vec<crate::model::FolderRule> = items
            .iter()
            .filter_map(|rule| {
                Some(crate::model::FolderRule {
                    exts: rule.get("exts")?.as_str()?.to_string(),
                    folder: rule.get("folder")?.as_str()?.to_string(),
                })
            })
            .take(8)
            .collect();
        if !rules.is_empty() {
            prefs.folder_rules = rules;
        }
    }
    if prefs.folder_rules.is_empty() {
        prefs.folder_rules = crate::model::default_folder_rules();
    }
    prefs
}

pub(crate) fn save_remember_category(value: bool) {
    let Some(path) = ui_path() else { return };
    let mut prefs: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(object) = prefs.as_object_mut() {
        object.insert("category_folders".to_string(), serde_json::Value::Bool(value));
        if let Ok(json) = serde_json::to_string_pretty(&prefs) {
            let _ = atomic_write(&path, json.as_bytes());
        }
    }
}

pub(crate) fn main_app_running() -> bool {
    crate::model::is_main_live()
}

pub(crate) fn launch_main_app() {
    let Ok(exe) = crate::platform::launch_path() else { return };
    let mut command = crate::platform::silent_command(exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0008);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let _ = command.spawn();
}

pub(crate) fn gpui_wake_main_app() {
    if !main_app_running() {
        launch_main_app();
    }
}

pub(crate) fn wake_ids(targets: &[u64]) {
    if let Some(path) = wake_path() {
        let _guard = crate::model::fs_lock("wake");
        if !_guard.owned() {
            return;
        }
        let mut ids: Vec<u64> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| value.get("start").cloned())
            .and_then(|value| serde_json::from_value::<Vec<u64>>(value).ok())
            .unwrap_or_default();
        for id in targets {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        if let Ok(json) = serde_json::to_string_pretty(&serde_json::json!({ "start": ids })) {
            let _ = atomic_write(&path, json.as_bytes());
        }
    }
    std::thread::spawn(|| {
        if !main_app_running() {
            launch_main_app();
        }
    });
}

/// Claim immediate-start requests written by confirmation or link-import
/// windows. Queue writes complete before these IDs are published, so the
/// manager can reload the queue and start each requested row exactly once.
pub(crate) fn take_wake_ids() -> Vec<u64> {
    let Some(path) = wake_path() else { return Vec::new() };
    let _guard = crate::model::fs_lock("wake");
    if !_guard.owned() {
        return Vec::new();
    }
    let ids = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("start").cloned())
        .and_then(|value| serde_json::from_value::<Vec<u64>>(value).ok())
        .unwrap_or_default();
    if !ids.is_empty() {
        let _ = std::fs::remove_file(path);
    }
    ids
}
