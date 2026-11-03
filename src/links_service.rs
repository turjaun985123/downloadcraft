//! UI-independent data and queue operations for the GPUI All Links window.
//!
//! The browser host writes a validated batch under the application config
//! directory. This module reads it, prepares display rows, and commits user
//! selections to the shared queue without depending on the legacy UI toolkit.

use crate::engine;
use crate::model::{category_of, config_dir, filename_from_url, DownloadItem};
use crate::native_host::LinksBatch;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Video,
    Audio,
    Image,
    Doc,
    Zip,
    App,
    Page,
    Other,
}

impl Kind {
    fn of(name: &str, html: bool, image: bool) -> Self {
        if html { return Self::Page; }
        if image { return Self::Image; }
        match category_of(name) {
            "Video" => Self::Video,
            "Music" => Self::Audio,
            "Image" => Self::Image,
            "Document" => Self::Doc,
            "Compressed" => Self::Zip,
            "Apps" => Self::App,
            _ => Self::Other,
        }
    }
}

fn type_of(name: &str, content_type: Option<&str>) -> (String, bool, bool) {
    let mime = content_type
        .map(|value| value.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty() && value != "application/octet-stream");
    if let Some(mime) = mime.as_deref() {
        if mime == "text/html" || mime == "application/xhtml+xml" {
            return ("Web page".into(), true, false);
        }
        if let Some(image_type) = mime.strip_prefix("image/") {
            return (format!("{} image", image_type.to_ascii_uppercase()), false, true);
        }
    }
    let extension = name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default();
    match extension.as_str() {
        "htm" | "html" | "php" | "asp" | "aspx" | "jsp" | "cgi" | "shtml" => {
            return ("Web page".into(), true, false);
        }
        "" if mime.is_none() => return ("Web page".into(), true, false),
        _ => {}
    }
    let category = category_of(name);
    if category == "Image" {
        return (format!("{} image", extension.to_ascii_uppercase()), false, true);
    }
    if !extension.is_empty() && category != "Other" {
        return (format!("{} {}", extension.to_ascii_uppercase(), category.to_ascii_lowercase()), false, false);
    }
    match mime {
        Some(mime) => (mime, false, false),
        None => (format!("{} file", extension.to_ascii_uppercase()), false, false),
    }
}

fn page_name(url: &str, text: &str) -> String {
    let from_url = || {
        let parsed = reqwest::Url::parse(url).ok()?;
        let tail = parsed
            .query_pairs()
            .last()
            .map(|(_, value)| value.into_owned())
            .or_else(|| parsed.path_segments().and_then(|mut segments| segments.next_back().map(str::to_string)))
            .filter(|tail| !tail.trim().is_empty())
            .or_else(|| parsed.host_str().map(str::to_string))?;
        Some(tail.rsplit('/').find(|part| !part.trim().is_empty()).unwrap_or(&tail).to_string())
    };
    let text = text.trim();
    let base = if !text.is_empty() && text.chars().count() <= 80 && !text.contains('{') {
        text.to_string()
    } else {
        from_url().unwrap_or_else(|| "page".into())
    };
    engine::sanitize_filename(&format!("{base}.html"))
}

fn read_batch(path: &std::path::Path) -> Result<LinksBatch, String> {
    let dir = config_dir().ok_or("no config dir")?;
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
    let inside = path.parent().map(|parent| parent == dir).unwrap_or(false);
    if !inside || !name.starts_with("links_") || !name.ends_with(".json") {
        return Err("not a links file from the browser extension".into());
    }
    let data = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let _ = std::fs::remove_file(path);
    serde_json::from_str(&data).map_err(|error| error.to_string())
}

#[derive(Clone)]
pub struct GpuiLinkRow {
    pub url: String,
    pub text: String,
    pub name: String,
    pub checked: bool,
}

pub struct GpuiLinksBatch {
    pub page_url: String,
    pub title: String,
    cookie: String,
    pub rows: Vec<GpuiLinkRow>,
}

/// Browser/link requests waiting for the main GPUI process to claim them.
pub(crate) fn pending_batches() -> Vec<std::path::PathBuf> {
    let Some(dir) = config_dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("links_") && name.ends_with(".json"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

pub fn gpui_load(path: &std::path::Path) -> Result<GpuiLinksBatch, String> {
    let batch = read_batch(path)?;
    let rows = batch.links.into_iter().map(|link| {
        let mut name = engine::sanitize_filename(&filename_from_url(&link.url));
        if crate::model::is_fallback_filename(&name) {
            name = page_name(&link.url, &link.text);
        }
        let (_, html, image) = type_of(&name, None);
        let checked = !matches!(Kind::of(&name, html, image), Kind::Page | Kind::Other | Kind::Image);
        GpuiLinkRow { url: link.url, text: link.text, name, checked }
    }).collect();
    Ok(GpuiLinksBatch { page_url: batch.page_url, title: batch.title, cookie: batch.cookie, rows })
}

fn same_host(a: &str, b: &str) -> bool {
    let host = |url: &str| reqwest::Url::parse(url).ok().and_then(|url| url.host_str().map(str::to_ascii_lowercase));
    matches!((host(a), host(b)), (Some(a), Some(b)) if a == b)
}

pub fn gpui_commit(batch: &GpuiLinksBatch, selected: &[usize], start: bool) -> String {
    if selected.is_empty() { return "Check links to download first.".into(); }
    let prefs = crate::capture_service::load_prefs();
    let mut added = Vec::new();
    let mut already = 0usize;
    let mut failed = None;
    for &index in selected {
        let Some(row) = batch.rows.get(index) else { continue };
        let name = crate::model::apply_rename_template(
            &prefs.rename_template,
            &engine::sanitize_filename(row.name.trim()),
            &row.url,
        );
        let mut folder = std::path::PathBuf::from(&prefs.save_dir);
        match prefs.folder_rules.iter().find(|rule| rule.matches(&name)) {
            Some(rule) if !rule.folder.trim().is_empty() => folder.push(rule.folder.trim()),
            _ => folder.push(category_of(&name)),
        }
        let mut item = DownloadItem::new(
            0,
            row.url.clone(),
            engine::sanitize_filename(&name),
            folder.to_string_lossy().into_owned(),
            prefs.connections,
        );
        item.referer = batch.page_url.clone();
        if same_host(&row.url, &batch.page_url) { item.cookie = batch.cookie.clone(); }
        item.description = row.text.clone();
        match crate::model::append_to_queue(&mut item) {
            crate::model::AppendResult::Added(id) => added.push(id),
            crate::model::AppendResult::AlreadyQueued(_) => already += 1,
            crate::model::AppendResult::Error(error) => failed = Some(error),
        }
    }
    if !added.is_empty() {
        if start { crate::capture_service::wake_ids(&added); }
        crate::capture_service::gpui_wake_main_app();
    }
    if let Some(error) = failed { return format!("Queued {}; queue update failed: {error}", added.len()); }
    if added.is_empty() { return format!("All {already} selected link(s) are already queued."); }
    format!("Queued {} link(s).", added.len())
}
