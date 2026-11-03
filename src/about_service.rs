//! UI-independent update/About state shared by the GPUI and legacy windows.

use crate::model::config_dir;

/// Keep the About panel's release highlights synchronized with updater state.
pub(crate) fn save_release_info(v: &serde_json::Value, newer: bool) {
    let body = v.get("body").and_then(|b| b.as_str()).unwrap_or("");
    let highlights: Vec<String> = body
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("- "))
        .map(|line| {
            let line = line.trim_start_matches("- ").replace("**", "");
            let head = line.split(": ").next().unwrap_or(&line).trim().to_string();
            if head.chars().count() > 90 {
                head.chars().take(88).collect::<String>() + "…"
            } else {
                head
            }
        })
        .take(3)
        .collect();
    let info = serde_json::json!({
        "tag": v.get("tag_name").and_then(|tag| tag.as_str()).unwrap_or(""),
        "url": v.get("html_url").and_then(|url| url.as_str()).unwrap_or(""),
        "highlights": highlights,
        "newer": newer,
    });
    if let Some(path) = config_dir().map(|dir| dir.join("release_info.json")) {
        let _ = crate::model::atomic_write(&path, info.to_string().as_bytes());
    }
}

/// Append an updater action for the main app to consume.
pub(crate) fn send(action: &str) {
    let Some(path) = config_dir().map(|dir| dir.join("cmd.json")) else { return };
    let guard = crate::model::fs_lock("cmd");
    if !guard.owned() {
        return;
    }
    let mut value: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!([]));
    if let Some(actions) = value.as_array_mut() {
        actions.push(serde_json::json!({ "action": action }));
        let _ = crate::model::atomic_write(&path, value.to_string().as_bytes());
    }
}

#[derive(Default, Clone)]
pub(crate) struct UpdState {
    pub phase: String,
    pub status: String,
    pub size: u64,
    pub running: u64,
}

#[derive(Default, Clone)]
pub(crate) struct Release {
    pub tag: String,
    pub url: String,
    pub highlights: Vec<String>,
}

fn read_json(name: &str) -> Option<serde_json::Value> {
    let path = config_dir()?.join(name);
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

pub(crate) fn read_state() -> UpdState {
    let Some(value) = read_json("update_state.json") else { return UpdState::default() };
    UpdState {
        phase: value.get("phase").and_then(|item| item.as_str()).unwrap_or("idle").into(),
        status: value.get("status").and_then(|item| item.as_str()).unwrap_or("").into(),
        size: value.get("size").and_then(|item| item.as_u64()).unwrap_or(0),
        running: value.get("running").and_then(|item| item.as_u64()).unwrap_or(0),
    }
}

pub(crate) fn read_release() -> Release {
    let Some(value) = read_json("release_info.json") else { return Release::default() };
    Release {
        tag: value.get("tag").and_then(|item| item.as_str()).unwrap_or("").trim_start_matches('v').into(),
        url: value.get("url").and_then(|item| item.as_str()).unwrap_or("").into(),
        highlights: value
            .get("highlights")
            .and_then(|item| item.as_array())
            .map(|items| items.iter().filter_map(|item| item.as_str().map(String::from)).collect())
            .unwrap_or_default(),
    }
}

#[derive(Clone, Default)]
pub(crate) struct GpuiSnapshot {
    pub phase: String,
    pub status: String,
    pub size: u64,
    pub running: u64,
    pub tag: String,
    pub url: String,
    pub highlights: Vec<String>,
}

pub(crate) fn gpui_snapshot() -> GpuiSnapshot {
    let state = read_state();
    let release = read_release();
    GpuiSnapshot {
        phase: state.phase,
        status: state.status,
        size: state.size,
        running: state.running,
        tag: release.tag,
        url: release.url,
        highlights: release.highlights,
    }
}

pub(crate) fn gpui_request(action: &str) {
    send(action);
}
