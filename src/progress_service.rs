//! UI-independent progress snapshots and commands shared by the GPUI views.
//!
//! This module reads the persisted queue/progress files and writes the same
//! command file consumed by the download manager. It deliberately contains no
//! windowing or rendering code.

use crate::model::{config_dir, DownloadItem};

fn queue_path() -> Option<std::path::PathBuf> {
    config_dir().map(|path| path.join("queue.json"))
}

fn progress_path() -> Option<std::path::PathBuf> {
    config_dir().map(|path| path.join("progress.json"))
}

fn cmd_path() -> Option<std::path::PathBuf> {
    config_dir().map(|path| path.join("cmd.json"))
}

fn read_queue() -> Vec<DownloadItem> {
    queue_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[derive(Debug, Default, Clone)]
struct Snap {
    downloaded: u64,
    total: Option<u64>,
    speed: f64,
    resumable: bool,
    live: bool,
    phase: String,
    resolution: String,
    parts: Vec<(u64, u64, u64)>,
    fragments: Option<(u64, u64)>,
}

fn read_snap(id: u64) -> Snap {
    let mut out = Snap::default();
    let data = progress_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    if data.is_empty() {
        return out;
    }
    let value: serde_json::Value =
        serde_json::from_str(&data).unwrap_or(serde_json::Value::Null);
    let id_key = id.to_string();
    let Some(entry) = value.get("items").and_then(|items| items.get(&id_key)) else {
        return out;
    };
    out.live = true;
    out.downloaded = entry.get("downloaded").and_then(|value| value.as_u64()).unwrap_or(0);
    out.total = entry.get("total").and_then(|value| value.as_u64());
    out.speed = entry.get("speed").and_then(|value| value.as_f64()).unwrap_or(0.0);
    out.resumable = entry.get("resumable").and_then(|value| value.as_bool()).unwrap_or(false);
    out.phase = entry.get("phase").and_then(|value| value.as_str()).unwrap_or("").to_string();
    out.resolution = entry.get("resolution").and_then(|value| value.as_str()).unwrap_or("").to_string();
    if let Some(parts) = entry.get("parts").and_then(|value| value.as_array()) {
        for part in parts.iter().take(32) {
            let number = |index: usize| part.get(index).and_then(|value| value.as_u64()).unwrap_or(0);
            out.parts.push((number(0), number(1), number(2)));
        }
    }
    if let Some(fragments) = entry.get("fragments").and_then(|value| value.as_array()) {
        let number = |index: usize| fragments.get(index).and_then(|value| value.as_u64()).unwrap_or(0);
        if number(1) > 0 {
            out.fragments = Some((number(0).min(number(1)), number(1)));
        }
    }
    out
}

#[derive(Clone, Default)]
pub(crate) struct GpuiProgressSnapshot {
    pub item: Option<DownloadItem>,
    pub live: bool,
    pub phase: String,
    pub resolution: String,
    pub resumable: bool,
    pub parts: Vec<(u64, u64, u64)>,
    pub fragments: Option<(u64, u64)>,
    pub log_lines: Vec<String>,
}

/// Read the queue, live progress, and optional job log for a GPUI progress view.
pub(crate) fn gpui_snapshot(id: u64, include_log: bool) -> GpuiProgressSnapshot {
    let mut item = read_queue().into_iter().find(|item| item.id == id);
    let snap = read_snap(id);
    if snap.live {
        if let Some(item) = item.as_mut() {
            if let Some(total) = snap.total.filter(|total| *total > 0) {
                item.total_bytes = Some(total);
            }
            item.downloaded_bytes = item.downloaded_bytes.max(snap.downloaded);
            if let Some(total) = item.total_bytes {
                item.downloaded_bytes = item.downloaded_bytes.min(total);
            }
            item.speed_bps = snap.speed;
        }
    }
    let log_lines = if include_log {
        crate::engine::job_log_path(&id.to_string())
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|text| text.lines().rev().take(13).map(str::to_string).collect::<Vec<_>>().into_iter().rev().collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    GpuiProgressSnapshot {
        item,
        live: snap.live,
        phase: snap.phase,
        resolution: snap.resolution,
        resumable: snap.resumable,
        parts: snap.parts,
        fragments: snap.fragments,
        log_lines,
    }
}

pub(crate) fn gpui_command(id: u64, action: &str) {
    send_cmd(id, action);
}

pub(crate) fn gpui_set_speed_limit(kbps: u64) {
    send_cmd_kv("limit", "kbps", serde_json::Value::from(kbps));
}

pub(crate) fn gpui_set_finish_action(action: &str) {
    send_cmd_kv("finish", "value", serde_json::Value::String(action.to_string()));
}

fn read_ui_prefs() -> (String, String) {
    let value: serde_json::Value = config_dir()
        .and_then(|path| std::fs::read_to_string(path.join("ui.json")).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null);
    let limit = value.get("speed_limit_kbps").and_then(|value| value.as_u64()).unwrap_or(0).to_string();
    let finish = value.get("finish_action").and_then(|value| value.as_str()).unwrap_or("nothing").to_string();
    (limit, finish)
}

pub(crate) fn gpui_settings() -> (u64, String) {
    let (limit, finish) = read_ui_prefs();
    (limit.parse().unwrap_or(0), finish)
}

fn send_cmd_kv(action: &str, key: &str, value: serde_json::Value) {
    let Some(path) = cmd_path() else { return };
    let _guard = crate::model::fs_lock("cmd");
    if !_guard.owned() { return; }
    let mut commands: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!([]));
    if let Some(commands) = commands.as_array_mut() {
        let mut command = serde_json::Map::new();
        command.insert("action".into(), serde_json::Value::String(action.into()));
        command.insert(key.into(), value);
        commands.push(serde_json::Value::Object(command));
        write_commands(&path, &commands);
    }
}

fn send_cmd(id: u64, action: &str) {
    let Some(path) = cmd_path() else { return };
    let _guard = crate::model::fs_lock("cmd");
    if !_guard.owned() { return; }
    let mut commands: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!([]));
    if let Some(commands) = commands.as_array_mut() {
        commands.retain(|command| command.get("id").and_then(|value| value.as_u64()) != Some(id));
        commands.push(serde_json::json!({ "id": id, "action": action }));
        write_commands(&path, &commands);
    }
}

fn write_commands(path: &std::path::Path, commands: &[serde_json::Value]) {
    let Ok(text) = serde_json::to_string_pretty(commands) else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let temporary = path.with_extension("tmp");
    if std::fs::write(&temporary, text).is_ok() {
        let _ = std::fs::rename(temporary, path);
    }
}
