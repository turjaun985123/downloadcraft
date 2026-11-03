//! UI independent queue and status operations for the Basket and Mini views.

use crate::model::{
    append_to_queue, apply_rename_template, category_of, config_dir, is_download_url, AppendResult,
    DownloadItem,
};

fn ui_prefs() -> (String, usize, String, bool) {
    let v: serde_json::Value = config_dir()
        .and_then(|p| std::fs::read_to_string(p.join("ui.json")).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let dir = v
        .get("save_dir")
        .and_then(|x| x.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            dirs::download_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| crate::model::default_download_dir().to_string_lossy().into_owned())
        });
    let conns = v
        .get("connections_default")
        .and_then(|x| x.as_u64())
        .map(|n| (n as usize).clamp(1, 32))
        .unwrap_or(8);
    let tpl = v
        .get("rename_template")
        .and_then(|x| x.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("{name}")
        .to_string();
    let cat = v
        .get("category_folders")
        .and_then(|x| x.as_bool())
        .unwrap_or(true);
    (dir, conns, tpl, cat)
}

pub(crate) fn queue_one(url: &str) -> &'static str {
    let url = url.trim().trim_matches(|c| c == '"' || c == '\'');
    if !is_download_url(url) || url.len() > 2048 {
        return "not a URL";
    }
    let (base, conns, tpl, cat) = ui_prefs();
    let mut name = apply_rename_template(&tpl, &crate::model::filename_from_url(url), url);
    if name.trim().is_empty() {
        name = crate::model::filename_from_url(url);
    }
    let mut dir = std::path::PathBuf::from(&base);
    if cat {
        dir.push(category_of(&name));
    }
    let mut item = DownloadItem::new(
        0,
        url.to_string(),
        crate::engine::sanitize_filename(&name),
        dir.to_string_lossy().to_string(),
        conns,
    );
    match append_to_queue(&mut item) {
        AppendResult::Added(_) => "queued",
        AppendResult::AlreadyQueued(_) => "already queued",
        AppendResult::Error(_) => "queue error",
    }
}

pub(crate) fn gpui_queue_one(url: &str) -> &'static str {
    queue_one(url)
}

pub(crate) fn gpui_queue_items() -> Vec<DownloadItem> {
    read_queue()
}

/// URLs inside a dropped file: `.url` (URL= line) or plain text (any token).
pub(crate) fn urls_from_dropped(path: &std::path::Path) -> Vec<String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    if ext == "url" {
        return data
            .lines()
            .filter_map(|l| {
                let t = l.trim();
                t.strip_prefix("URL=")
                    .or_else(|| t.strip_prefix("url="))
                    .map(|u| u.trim().to_string())
            })
            .collect();
    }
    if ext == "txt" || ext == "lnk" || ext.is_empty() {
        return data
            .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .filter(|t| is_download_url(t))
            .map(str::to_string)
            .collect();
    }
    Vec::new()
}

pub(crate) fn read_queue() -> Vec<DownloadItem> {
    config_dir()
        .and_then(|p| std::fs::read_to_string(p.join("queue.json")).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub(crate) fn send_cmd(action: &str) {
    let Some(p) = config_dir().map(|d| d.join("cmd.json")) else {
        return;
    };
    let _guard = crate::model::fs_lock("cmd");
    if !_guard.owned() { return; }
    let mut v: serde_json::Value = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!([]));
    if let Some(arr) = v.as_array_mut() {
        arr.push(serde_json::json!({ "action": action }));
        if let Ok(json) = serde_json::to_string_pretty(&v) {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let tmp = p.with_extension("tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
}

pub(crate) fn gpui_send_cmd(action: &str) {
    send_cmd(action);
}

pub(crate) fn launch_main() {
    let Ok(exe) = crate::platform::launch_path() else {
        return;
    };
    let mut cmd = crate::platform::silent_command(exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x00000008);
    }
    let _ = cmd.spawn();
}

pub(crate) fn gpui_launch_main() {
    launch_main();
}

#[derive(Clone, Copy, Default)]
pub(crate) struct MiniSnapshot {
    pub active: usize,
    pub queued: usize,
    pub speed: f64,
    pub frac: f32,
}

pub(crate) fn mini_snapshot() -> MiniSnapshot {
    let items = read_queue();
    let snap: serde_json::Value = config_dir()
        .and_then(|p| std::fs::read_to_string(p.join("progress.json")).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let mut active = 0;
    let mut queued = 0;
    let mut speed = 0.0;
    let mut dl = 0u64;
    let mut total = 0u64;
    let mut any_total = false;
    for d in &items {
        match d.status {
            crate::model::Status::Downloading => {
                active += 1;
                let id = d.id.to_string();
                let e = snap.get("items").and_then(|m| m.get(&id));
                let s = e
                    .and_then(|x| x.get("speed"))
                    .and_then(|x| x.as_f64())
                    .unwrap_or(d.speed_bps);
                speed += s;
                let dd = e
                    .and_then(|x| x.get("downloaded"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(d.downloaded_bytes);
                dl += dd;
                if let Some(t) = e
                    .and_then(|x| x.get("total"))
                    .and_then(|x| x.as_u64())
                    .or(d.total_bytes)
                {
                    total += t;
                    any_total = true;
                }
            }
            crate::model::Status::Queued | crate::model::Status::Paused => queued += 1,
            _ => {}
        }
    }
    MiniSnapshot {
        active,
        queued,
        speed,
        frac: if any_total && total > 0 {
            (dl as f32 / total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        },
    }
}
