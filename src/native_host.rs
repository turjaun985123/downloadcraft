//! Native-messaging host for the browser extension (`--native-host`).
//!
//! Protocol (Chrome/Edge/Firefox native messaging = 4-byte LE length + UTF-8 JSON):
//!   -> {"type":"ping"}                                  <- {"type":"pong","version":1}
//!   -> {"type":"download","url":"https://...","filename":"optional",
//!       "pageUrl":"https://page...","referer":"...","cookie":"...","mime":"...",
//!       "headers":{"origin":"...","authorization":"...","x-...":"..."}}
//!                                                      <- {"type":"ok","queued":true}
//!
//! Download requests are appended to an inbox file that the GUI polls and
//! imports as queued (auto-started) downloads.
//!
//! Persistent port (`runtime.connectNative`): every message may carry a
//! `rid`, echoed on its reply, so several requests can be in flight. After
//! `{"type":"hello"}` the host also *pushes* to the extension:
//!   <- {"type":"settings","settings":{...},"rules":{...}}   capture settings + site rules
//!   <- {"type":"event","event":"done"|"failed",...}        downloads the extension sent
//!   <- {"type":"pageFetch","id":"...","url":"...","page":"..."}  fetch inside the page
//! and accepts `{"type":"pageFetchChunk","fetchId":..,"chunk":b64,"done":bool}`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
struct Incoming {
    #[serde(rename = "type")]
    kind: String,
    /// "report": one line for the app's log (a site's layout changed, ...).
    #[serde(default)]
    note: Option<String>,
    url: Option<String>,
    filename: Option<String>,
    /// Stable per-capture id. Unlike `rid`, this survives a persistent-port
    /// timeout and the bridge's one-shot fallback.
    #[serde(default, alias = "captureId")]
    capture_id: Option<String>,
    /// Page the download was captured from (IDM-style referer fallback).
    #[serde(default, alias = "pageUrl")]
    page_url: Option<String>,
    #[serde(default)]
    referer: Option<String>,
    #[serde(default)]
    cookie: Option<String>,
    /// Origin / Authorization / x-* headers the player sent with this media.
    #[serde(default)]
    headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    mime: Option<String>,
    /// Browser-measured size bytes (HEAD/Range from the page session).
    /// The engine can't always re-probe media URLs (403 off-session), so
    /// the dialog prefers this over probing.
    #[serde(default)]
    size: Option<u64>,
    /// Adaptive pair audio URL (video-only + audio merge). The extension
    /// sends these in camelCase (`pairUrl`/`pairMime`) like `pageUrl`, so
    /// without the aliases the pair was silently dropped and the merge never
    /// happened.
    #[serde(default, alias = "pairUrl")]
    pub pair_url: Option<String>,
    #[serde(default, alias = "pairMime")]
    pub pair_mime: Option<String>,
    /// Batch presence check (`{"type":"query"}`): which of these URLs are
    /// already queued. Answered from queue.json without side effects.
    /// (Only read on Incoming; stored nowhere.)
    #[serde(default)]
    pub urls: Option<Vec<String>>,
    /// Preferred YouTube quality from the extension quality menu.
    #[serde(default)]
    pub quality: Option<String>,
    /// Media duration in seconds (page player / JSON, best-effort).
    /// Shown in the confirm dialog and checked against the completed file.
    #[serde(default)]
    pub dur: Option<u64>,
    #[serde(default, alias = "audioExpected")]
    pub audio_expected: bool,
    /// "Download all links" (`{"type":"links"}`): every link on a page.
    #[serde(default)]
    pub links: Option<Vec<PageLink>>,
    /// Page title for the links window.
    #[serde(default)]
    pub title: Option<String>,
    /// `{"type":"control"}`: which queue row, and what to do with it.
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub action: Option<String>,
    /// Request id on a persistent port; echoed on the reply.
    #[serde(default)]
    pub rid: Option<u64>,
    /// Stable per-operation id used by one-shot retries for side effects
    /// other than downloads (for example links, record, and control).
    #[serde(default, alias = "requestId")]
    request_id: Option<String>,
    /// The browser's side of the capture (method/body, request headers,
    /// server IP, proxy). Sanitized before it is stored.
    #[serde(default)]
    pub browser: Option<crate::model::BrowserContext>,
    /// `pageFetchChunk`: which request, a base64 piece, the last one, and
    /// the outcome.
    #[serde(default, alias = "fetchId")]
    pub fetch_id: Option<String>,
    #[serde(default)]
    pub chunk: Option<String>,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub error: Option<String>,
    /// `setPrefs`: capture settings changed in the extension's popup.
    #[serde(default)]
    pub prefs: Option<serde_json::Value>,
}

/// One link from a page ("Download all links").
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct PageLink {
    pub url: String,
    #[serde(default)]
    pub text: String,
}

/// What `--links <file>` reads: written by the host, deleted by the window.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct LinksBatch {
    pub page_url: String,
    #[serde(default)]
    pub title: String,
    /// The page's cookies; the window applies them to same-host links only.
    #[serde(default)]
    pub cookie: String,
    pub links: Vec<PageLink>,
}

/// Validate a page's links and keep the downloadable ones (http/https,
/// sane lengths, no duplicates), capped like the extension's collector.
pub fn links_batch(page_url: &str, title: &str, cookie: &str, links: &[PageLink]) -> LinksBatch {
    let mut seen = std::collections::HashSet::new();
    let kept = links
        .iter()
        .filter(|l| crate::model::is_download_url(&l.url) && l.url.len() <= 4096)
        .filter(|l| seen.insert(l.url.clone()))
        .take(5000)
        .map(|l| PageLink {
            url: l.url.clone(),
            text: l.text.chars().filter(|c| !c.is_control()).take(200).collect(),
        })
        .collect();
    LinksBatch {
        page_url: page_url.to_string(),
        title: title.chars().filter(|c| !c.is_control()).take(200).collect(),
        cookie: cookie.to_string(),
        links: kept,
    }
}

fn spawn_links(msg: &Incoming) -> Result<(), String> {
    open_links_window(
        &msg.page_url.clone().unwrap_or_default(),
        msg.title.as_deref().unwrap_or(""),
        msg.cookie.as_deref().unwrap_or(""),
        msg.links.as_deref().unwrap_or(&[]),
    )
}

/// The "pick links" window for any list of links (download all links, batch
/// patterns, dropped lists).
pub fn open_links_window(page: &str, title: &str, cookie: &str, links: &[PageLink]) -> Result<(), String> {
    let batch = links_batch(page, title, cookie, links);
    if batch.links.is_empty() {
        return Err("no downloadable links on this page".into());
    }
    let dir = crate::model::config_dir().ok_or_else(|| "no config dir".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("links_{}.json", chrono::Local::now().timestamp_millis()));
    let json = serde_json::to_string(&batch).map_err(|e| e.to_string())?;
    crate::model::atomic_write(&path, json.as_bytes()).map_err(|e| e.to_string())?;
    let exe = crate::platform::launch_path().map_err(|e| e.to_string())?;
    let mut cmd = crate::platform::silent_command(exe);
    let app_state = crate::model::app_lifecycle_state();
    if !matches!(app_state.as_str(), "starting" | "running") {
        cmd.arg("--links").arg(&path);
    }
    if spawn_detached(cmd) {
        Ok(())
    } else {
        Err("could not start the links window".into())
    }
}

/// Started by the browser straight from the manifest, without the launcher
/// script (`--native-host`). Chromium passes the caller's origin first,
/// Firefox the manifest path and then the extension's id. Used when the
/// program sits under a non-ASCII path, which a `.bat` cannot name (issue #5).
pub fn launched_directly(args: &[String]) -> bool {
    let ours = |a: &str| {
        a.strip_prefix("chrome-extension://").is_some_and(|rest| {
            let id = rest.trim_end_matches('/');
            id == crate::integration::EXTENSION_ID || crate::integration::STORE_EXTENSION_IDS.contains(&id)
        }) || a.starts_with("moz-extension://") || a == "send-to-download-craft@example"
    };
    let launch_args: Vec<&str> = args.iter().skip(1).take(3).map(String::as_str).collect();
    let has_origin = launch_args.iter().any(|arg| ours(arg));
    // Firefox passes the registered host manifest followed by the add-on id.
    // Requiring both avoids mistaking a normal CLI download of `*.json` for a
    // native-host launch.
    let has_manifest = launch_args.iter().take(2).any(|arg| arg.to_ascii_lowercase().ends_with(".json"));
    let has_firefox_id = launch_args.iter().any(|arg| *arg == "send-to-download-craft@example");
    has_origin || (has_manifest && has_firefox_id)
}

/// Spawn a window that outlives this short-lived host.
fn spawn_detached(mut cmd: std::process::Command) -> bool {
    // Never hand our stdio to the window: an inherited stdout keeps the
    // browser's pipe open after this host exits.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        // DETACHED_PROCESS so the window outlives this short-lived host, and
        // BREAKAWAY_FROM_JOB because Firefox runs native hosts inside a job
        // object it kills when the host exits: children inherit that job, so a
        // plain detached dialog was being torn down the moment Firefox closed
        // our stdin (verified live — Chrome does not use such a job, which is
        // why only Firefox looked like it did nothing). A job that forbids
        // breaking away makes the spawn fail, and a plain detached window
        // still beats none at all.
        cmd.creation_flags(DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
        if cmd.spawn().is_ok() {
            return true;
        }
        cmd.creation_flags(DETACHED_PROCESS);
        return cmd.spawn().is_ok();
    }
    // Linux/macOS: out of the host's process group, which the browser may
    // signal as a whole when it closes the host.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        cmd.spawn().is_ok()
    }
}

#[derive(Debug, Serialize)]
struct Reply {
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queued: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    urls: Option<Vec<String>>,
    /// The app's theme ("light" | "dark" | "system") on pong, so the
    /// extension's in-page controls match it.
    #[serde(skip_serializing_if = "Option::is_none")]
    theme: Option<String>,
    /// `{"type":"list"}`: the app's active and recent downloads.
    #[serde(skip_serializing_if = "Option::is_none")]
    items: Option<Vec<serde_json::Value>>,
    /// Main GUI liveness is separate from native-host responsiveness.
    #[serde(skip_serializing_if = "Option::is_none")]
    app_state: Option<String>,
}

impl Reply {
    fn ok() -> Self {
        Reply { kind: "ok".into(), version: None, queued: Some(true), error: None, urls: None, theme: None, items: None, app_state: None }
    }
    fn err(e: impl Into<String>) -> Self {
        Reply { kind: "error".into(), version: None, queued: None, error: Some(e.into()), urls: None, theme: None, items: None, app_state: None }
    }
}

/// The popup's Downloads tab: every active row, then the newest finished or
/// failed ones, with live bytes and speed from the app's progress snapshot.
fn app_downloads() -> Vec<serde_json::Value> {
    use crate::model::{DownloadItem, Status};
    let dir = match crate::model::config_dir() {
        Some(d) => d,
        None => return Vec::new(),
    };
    let items: Vec<DownloadItem> = std::fs::read_to_string(dir.join("queue.json"))
        .ok()
        .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default();
    let live: serde_json::Value = std::fs::read_to_string(dir.join("progress.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let mut rows: Vec<&DownloadItem> = items.iter().collect();
    rows.sort_by(|a, b| b.id.cmp(&a.id));
    let active = |d: &&DownloadItem| matches!(d.status, Status::Downloading | Status::Queued | Status::Paused);
    let mut out: Vec<&DownloadItem> = rows.iter().copied().filter(active).collect();
    out.extend(rows.iter().copied().filter(|d| !active(d)).take(15));
    out.into_iter()
        .map(|d| {
            let snap = live.get("items").and_then(|m| m.get(d.id.to_string()));
            let num = |k: &str| snap.and_then(|s| s.get(k)).and_then(|x| x.as_f64());
            let (state, error) = match &d.status {
                Status::Downloading => ("downloading", String::new()),
                Status::Queued => ("queued", String::new()),
                Status::Paused => ("paused", String::new()),
                Status::Completed => ("done", String::new()),
                Status::Failed(e) => ("failed", e.clone()),
            };
            serde_json::json!({
                "id": d.id,
                "name": d.filename,
                "state": state,
                "error": error,
                "total": num("total").map(|t| t as u64).or(d.total_bytes),
                "done": num("downloaded").map(|t| t as u64).unwrap_or(d.downloaded_bytes),
                "speed": num("speed").unwrap_or(0.0),
                "url": d.url,
            })
        })
        .collect()
}

/// A file the extension saved itself ("Download in browser"): list it in the
/// app as finished. `filename` is the saved file's full path; it must exist.
fn record_browser_download(msg: &Incoming) -> Result<(), String> {
    let url = msg.url.clone().filter(|u| u.starts_with("http")).unwrap_or_default();
    let path = std::path::PathBuf::from(msg.filename.clone().unwrap_or_default());
    if !path.is_absolute() || !path.is_file() {
        return Err("the saved file was not found".into());
    }
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).ok_or("no file name")?;
    let dir = path.parent().map(|d| d.to_string_lossy().to_string()).ok_or("no folder")?;
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut item = crate::model::DownloadItem::new(0, if url.is_empty() { format!("file:///{}", path.display()) } else { url }, name, dir, 1);
    item.total_bytes = Some(len);
    item.downloaded_bytes = len;
    item.referer = msg.page_url.clone().unwrap_or_default();
    item.description = "Downloaded in the browser".into();
    match crate::model::record_finished(&mut item) {
        crate::model::AppendResult::Error(e) => Err(e),
        _ => Ok(()),
    }
}

/// Queue a pause/resume/cancel order for the app (the same cmd.json the
/// progress windows use).
fn app_control(id: Option<u64>, action: &str) -> Result<(), String> {
    let allowed = ["pause", "resume", "cancel", "pause_all", "resume_all"];
    if !allowed.contains(&action) {
        return Err("unknown action".into());
    }
    let p = crate::model::config_dir().ok_or("no config dir")?.join("cmd.json");
    let _guard = crate::model::fs_lock("cmd");
    if !_guard.owned() {
        return Err("the app is busy; retry".into());
    }
    let mut v: serde_json::Value = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!([]));
    let arr = v.as_array_mut().ok_or("cmd file unreadable")?;
    let mut o = serde_json::json!({ "action": action });
    if let Some(id) = id {
        o["id"] = serde_json::json!(id);
    }
    arr.push(o);
    let json = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    crate::model::atomic_write(&p, json.as_bytes()).map_err(|e| e.to_string())
}

/// The app theme as the extension understands it.
fn app_theme() -> String {
    match crate::model::ui_pref_str("theme", "system").as_str() {
        "light" => "light".to_string(),
        "dark" | "black" => "dark".to_string(),
        _ => "system".to_string(),
    }
}

/// Which of `urls` are already in the shared queue (read-only, locked).
fn queued_urls(urls: &[String]) -> Vec<String> {
    if urls.is_empty() {
        return Vec::new();
    }
    let _guard = crate::model::fs_lock("queue");
    if !_guard.owned() { return Vec::new(); }
    let items: Vec<crate::model::DownloadItem> = crate::model::config_dir()
        .and_then(|d| std::fs::read_to_string(d.join("queue.json")).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    urls.iter()
        .filter(|u| items.iter().any(|d| &d.url == *u))
        .cloned()
        .collect()
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InboxItem {
    pub url: String,
    pub filename: Option<String>,
    pub received_at: String,
    /// Stable browser capture id, retained only in the private inbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_id: Option<String>,
    /// Page URL the media was captured from (used as Referer fallback).
    #[serde(default)]
    pub page_url: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub cookie: Option<String>,
    /// Captured request headers (credentials: kept in the private inbox,
    /// never on a command line).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub pair_url: Option<String>,
    #[serde(default)]
    pub pair_mime: Option<String>,
    /// Batch presence check (`{"type":"query"}`): which of these URLs are
    /// already queued. Answered from queue.json without side effects.
    /// (Only read on Incoming; stored nowhere.)
    #[serde(default)]
    pub urls: Option<Vec<String>>,
    /// Preferred YouTube quality from the extension quality menu.
    #[serde(default)]
    pub quality: Option<String>,
    /// Media duration in seconds (page player / JSON, best-effort).
    /// Shown in the confirm dialog and checked against the completed file.
    #[serde(default)]
    pub dur: Option<u64>,
    #[serde(default, alias = "audioExpected")]
    pub audio_expected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<crate::model::BrowserContext>,
}

/// Lines the browser extension asked to show in the app's log.
pub fn notes_path() -> Option<std::path::PathBuf> {
    crate::model::config_dir().map(|p| p.join("extension_notes.txt"))
}

/// Append one note (single line, bounded); the file is capped so a page
/// that reports in a loop cannot grow it.
fn append_extension_note(text: &str) {
    let Some(path) = notes_path() else { return };
    let line: String = text.chars().filter(|c| !c.is_control()).take(300).collect();
    if line.trim().is_empty() {
        return;
    }
    let _guard = crate::model::fs_lock("extension_notes");
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default();
    if lines.iter().any(|l| l == &line) {
        return;
    }
    lines.push(line);
    let keep = lines.len().saturating_sub(50);
    let _ = std::fs::write(&path, lines[keep..].join("
") + "
");
}

/// Take every pending note (the app shows them in its log).
pub fn take_extension_notes() -> Vec<String> {
    let Some(path) = notes_path() else { return Vec::new() };
    let _guard = crate::model::fs_lock("extension_notes");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if text.is_empty() {
        return Vec::new();
    }
    let _ = std::fs::remove_file(&path);
    text.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect()
}

pub fn inbox_path() -> Option<std::path::PathBuf> {
    crate::model::config_dir().map(|p| p.join("native_inbox.json"))
}

/// Read a stable inbox snapshot for the main GPUI process. Reading never
/// consumes a capture: the caller acknowledges it only after the user commits
/// the download or explicitly cancels its confirmation, so a process exit
/// leaves the request available for recovery.
pub fn pending_captures() -> Result<Vec<InboxItem>, String> {
    let path = inbox_path().ok_or_else(|| "no config dir".to_string())?;
    let _guard = crate::model::fs_lock("inbox");
    if !_guard.owned() {
        return Err("download inbox is busy; retry shortly".into());
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|_| "download inbox unreadable; kept for recovery".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("could not read download inbox: {error}")),
    }
}

/// Acknowledge one inbox item after its in-process confirmation is resolved.
/// The identity matches a stable URL row (and its capture id when available),
/// not an unrelated queued capture. Returns false if another owner already
/// claimed the item (for example, the standalone fallback dialog).
pub fn acknowledge_capture(target: &InboxItem) -> Result<bool, String> {
    let path = inbox_path().ok_or_else(|| "no config dir".to_string())?;
    let _guard = crate::model::fs_lock("inbox");
    if !_guard.owned() {
        return Err("download inbox is busy; retry shortly".into());
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("could not read download inbox: {error}")),
    };
    let mut items: Vec<InboxItem> = serde_json::from_str(&text)
        .map_err(|_| "download inbox unreadable; kept for recovery".to_string())?;
    let index = items.iter().position(|item| {
        if let Some(id) = target.capture_id.as_deref() {
            item.url == target.url && item.capture_id.as_deref() == Some(id)
        } else {
            item.url == target.url
                && item.received_at == target.received_at
                && item.capture_id.is_none()
        }
    });
    let Some(index) = index else { return Ok(false) };
    if let Some(id) = target.capture_id.as_deref() {
        let receipt_path = receipt_path().ok_or_else(|| "no config dir".to_string())?;
        let now = chrono::Utc::now().timestamp();
        let mut receipts = load_receipts(&receipt_path, now)?;
        if let Some(receipt) = receipts.iter_mut().find(|receipt| receipt.capture_id == id) {
            // The user resolved this confirmation. A late one-shot retry for
            // its old capture id must not reopen the dialog after the inbox
            // row is acknowledged.
            receipt.dialog_pending = false;
            save_receipts(&receipt_path, &receipts)?;
        }
    }
    items.remove(index);
    if items.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("could not acknowledge browser capture: {error}")),
        }
    } else {
        let bytes = serde_json::to_vec_pretty(&items).map_err(|error| error.to_string())?;
        crate::model::atomic_write(&path, &bytes).map_err(|error| error.to_string())?;
    }
    Ok(true)
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct CaptureReceipt {
    capture_id: String,
    accepted_at: i64,
    /// Retried requests may retry opening their confirmation until the host
    /// has successfully spawned it. A completed receipt has no side effects.
    #[serde(default)]
    dialog_pending: bool,
}

const RECEIPT_TTL_SECS: i64 = 30 * 24 * 60 * 60;
const MAX_CAPTURE_RECEIPTS: usize = 4096;

fn receipt_path() -> Option<std::path::PathBuf> {
    crate::model::config_dir().map(|p| p.join("native_capture_receipts.json"))
}

pub(crate) fn valid_capture_id(id: &str) -> bool {
    (8..=96).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Stable per-URL lock name shared by the host and standalone confirm UI.
pub(crate) fn confirm_window_name(url: &str) -> String {
    use sha2::Digest;
    format!("confirm_{:x}", sha2::Sha256::digest(url.as_bytes()))
}

fn load_receipts(path: &std::path::Path, now: i64) -> Result<Vec<CaptureReceipt>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("could not read capture receipts: {error}")),
    };
    let mut receipts: Vec<CaptureReceipt> = serde_json::from_str(&text)
        .map_err(|_| "capture receipt file unreadable; kept for recovery".to_string())?;
    receipts.retain(|r| now.saturating_sub(r.accepted_at) <= RECEIPT_TTL_SECS);
    if receipts.len() > MAX_CAPTURE_RECEIPTS {
        let excess = receipts.len() - MAX_CAPTURE_RECEIPTS;
        receipts.drain(..excess);
    }
    Ok(receipts)
}

fn save_receipts(path: &std::path::Path, receipts: &[CaptureReceipt]) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(receipts).map_err(|e| e.to_string())?;
    crate::model::atomic_write(path, &bytes).map_err(|e| e.to_string())
}

#[derive(Debug, Serialize, Deserialize)]
struct NativeRequestReceipt {
    request_id: String,
    kind: String,
    accepted_at: i64,
}

const REQUEST_RECEIPT_TTL_SECS: i64 = 30 * 24 * 60 * 60;
const MAX_REQUEST_RECEIPTS: usize = 4096;

/// Run retryable side effects at most once across native-host processes.
/// The receipt is persisted before the effect: if the host exits after the
/// effect but before its reply reaches the extension, a retry cannot repeat
/// the command or open another Links window.
fn with_request_receipt(
    request_id: Option<&str>,
    kind: &str,
    action: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let Some(request_id) = request_id.filter(|id| valid_capture_id(id)) else {
        return action();
    };
    let path = crate::model::config_dir()
        .ok_or_else(|| "no config dir".to_string())?
        .join("native_request_receipts.json");
    let _guard = crate::model::fs_lock("native-requests");
    if !_guard.owned() {
        return Err("native request is busy; retry shortly".into());
    }
    let now = chrono::Utc::now().timestamp();
    let mut receipts: Vec<NativeRequestReceipt> = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|_| "native request receipts are unreadable".to_string())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.to_string()),
    };
    receipts.retain(|entry| now.saturating_sub(entry.accepted_at) <= REQUEST_RECEIPT_TTL_SECS);
    if let Some(existing) = receipts.iter().find(|entry| entry.request_id == request_id) {
        return if existing.kind == kind {
            Ok(())
        } else {
            Err("request id was already used for a different operation".into())
        };
    }

    receipts.push(NativeRequestReceipt {
        request_id: request_id.to_string(),
        kind: kind.to_string(),
        accepted_at: now,
    });
    if receipts.len() > MAX_REQUEST_RECEIPTS {
        let excess = receipts.len() - MAX_REQUEST_RECEIPTS;
        receipts.drain(..excess);
    }
    let persist = |entries: &[NativeRequestReceipt]| -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(entries).map_err(|error| error.to_string())?;
        crate::model::atomic_write(&path, &bytes).map_err(|error| error.to_string())
    };
    persist(&receipts)?;
    match action() {
        Ok(()) => Ok(()),
        Err(error) => {
            receipts.retain(|entry| entry.request_id != request_id);
            persist(&receipts)?;
            Err(error)
        }
    }
}

/// Called with the inbox lock held so concurrent native-host processes cannot
/// each accept the same capture ID before its receipt is persisted.
fn record_capture_locked(
    receipts: &mut Vec<CaptureReceipt>,
    capture_id: &str,
    pending: bool,
    now: i64,
) -> Result<(), String> {
    if let Some(receipt) = receipts.iter_mut().find(|r| r.capture_id == capture_id) {
        receipt.dialog_pending = pending;
        return Ok(());
    }
    receipts.push(CaptureReceipt {
        capture_id: capture_id.to_string(),
        accepted_at: now,
        dialog_pending: pending,
    });
    if receipts.len() > MAX_CAPTURE_RECEIPTS {
        let excess = receipts.len() - MAX_CAPTURE_RECEIPTS;
        receipts.drain(..excess);
    }
    Ok(())
}

/// One stdin frame: clean EOF (browser gone → exit the host) vs a broken
/// frame (reply with an error and keep serving — one bad message must not
/// kill the host and abandon everything behind it).
enum HostFrame {
    Eof,
    Bad(String),
    Msg(Incoming),
}

fn read_message() -> HostFrame {
    use std::io::Read;
    let mut len_buf = [0u8; 4];
    match std::io::stdin().read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return HostFrame::Eof,
        Err(_) => return HostFrame::Eof, // pipe broken: nothing more to serve
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 {
        return HostFrame::Eof; // clean close
    }
    if len > 4 * 1024 * 1024 {
        // Bigger than any real request. The length itself is valid framing,
        // so skip exactly that many bytes and keep serving: exiting dropped
        // every later capture on this port. Only a stream that ends early
        // (a corrupt prefix) closes the host.
        let skipped = std::io::copy(&mut std::io::stdin().take(len as u64), &mut std::io::sink());
        return match skipped {
            Ok(n) if n == len as u64 => HostFrame::Bad(format!("message too large ({len} bytes)")),
            _ => HostFrame::Eof,
        };
    }
    let mut buf = vec![0u8; len];
    if std::io::stdin().read_exact(&mut buf).is_err() {
        return HostFrame::Eof;
    }
    match serde_json::from_slice::<Incoming>(&buf) {
        Ok(m) => HostFrame::Msg(m),
        Err(e) => HostFrame::Bad(format!("bad message: {e}")),
    }
}

/// Request id of the message being answered (one at a time: replies are
/// written by the reading loop).
static CURRENT_RID: std::sync::Mutex<Option<u64>> = std::sync::Mutex::new(None);
/// Replies and pushes share stdout: one frame at a time.
static OUT: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn write_message(reply: &Reply) {
    let Ok(mut v) = serde_json::to_value(reply) else { return };
    if let Some(rid) = *CURRENT_RID.lock().unwrap_or_else(|e| e.into_inner()) {
        v["rid"] = serde_json::json!(rid);
    }
    write_value(&v);
}

fn write_value(v: &serde_json::Value) {
    if let Ok(data) = serde_json::to_vec(v) {
        let len = (data.len() as u32).to_le_bytes();
        let _g = OUT.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = std::io::stdout();
        use std::io::Write;
        let _ = out.write_all(&len);
        let _ = out.write_all(&data);
        let _ = out.flush();
    }
}

// ---- push channel (persistent port) ----------------------------------------

/// Capture settings the user set in the app (only keys present in ui.json,
/// so the extension keeps its own choices for the rest).
pub fn capture_settings() -> serde_json::Value {
    let ui: serde_json::Value = crate::model::config_dir()
        .and_then(|d| std::fs::read_to_string(d.join("ui.json")).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let mut out = serde_json::Map::new();
    for (key, ext) in [
        ("capture_auto", "autoCapture"),
        ("capture_types", "captureTypes"),
        ("capture_skip_sites", "blockHosts"),
        ("capture_min_kb", "minSizeKB"),
        ("notify_done", "notify"),
        ("browser_fallback", "browserFallback"),
        ("capture_skip_key", "skipKey"),
    ] {
        if let Some(v) = ui.get(key) {
            out.insert(ext.to_string(), v.clone());
        }
    }
    serde_json::Value::Object(out)
}

/// Capture settings changed in the extension popup land in the app's
/// settings too, so the two never disagree.
fn set_prefs(prefs: &serde_json::Value) -> Result<(), String> {
    let mut changes: Vec<(&str, serde_json::Value)> = Vec::new();
    if let Some(b) = prefs.get("autoCapture").and_then(|v| v.as_bool()) {
        changes.push(("capture_auto", b.into()));
    }
    if let Some(n) = prefs.get("minSizeKB").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))) {
        changes.push(("capture_min_kb", n.min(10_000_000).into()));
    }
    if let Some(h) = prefs.get("blockHosts").and_then(|v| v.as_str()) {
        let h: String = h.chars().filter(|c| !c.is_control()).take(2000).collect();
        changes.push(("capture_skip_sites", h.into()));
    }
    if changes.is_empty() {
        return Ok(());
    }
    let path = crate::model::config_dir().ok_or("no config dir")?.join("ui.json");
    let _guard = crate::model::fs_lock("ui-settings");
    if !_guard.owned() {
        return Err("settings are busy; retry".into());
    }
    let mut v: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let obj = v.as_object_mut().ok_or("settings unreadable")?;
    for (k, val) in changes {
        obj.insert(k.to_string(), val);
    }
    let json = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    crate::model::atomic_write(&path, json.as_bytes()).map_err(|e| e.to_string())
}

/// Built-in site rules (also published at the repository root, where the
/// app looks for newer ones).
pub const DEFAULT_SITE_RULES: &str = include_str!("../site_rules.json");

/// Where the app fetches newer site rules (at most once a day).
const SITE_RULES_URL: &str = "https://raw.githubusercontent.com/hungry-detective/downloadcraft/master/site_rules.json";

/// Refresh `site_rules.json` from the project when the copy is a day old.
/// A rules file only replaces ours when it parses and is newer.
pub fn refresh_site_rules() {
    let Some(dir) = crate::model::config_dir() else { return };
    let path = dir.join("site_rules.json");
    let version = |v: &serde_json::Value| v.get("version").and_then(|x| x.as_u64()).unwrap_or(0);
    let builtin: serde_json::Value = serde_json::from_str(DEFAULT_SITE_RULES).unwrap_or_default();
    let current: Option<serde_json::Value> = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok());
    if current.as_ref().map_or(true, |c| version(c) < version(&builtin)) {
        let _ = std::fs::create_dir_all(&dir);
        let _ = crate::model::atomic_write(&path, DEFAULT_SITE_RULES.as_bytes());
    }
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < std::time::Duration::from_secs(24 * 3600));
    if fresh && current.is_some() {
        return;
    }
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
    let got = rt.block_on(async {
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().ok()?;
        let r = client.get(SITE_RULES_URL).send().await.ok()?.error_for_status().ok()?;
        r.text().await.ok()
    });
    let mine = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()).unwrap_or_default();
    match got.and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok().map(|v| (t, v))) {
        Some((text, v)) if v.is_object() && version(&v) > version(&mine) => {
            let _ = crate::model::atomic_write(&path, text.as_bytes());
        }
        // Nothing newer: touch the file so the next check waits a day.
        _ => {
            let _ = crate::model::atomic_write(&path, serde_json::to_string_pretty(&mine).unwrap_or_default().as_bytes());
        }
    }
}

/// Site rules the app keeps (`site_rules.json`, refreshed by the app).
pub fn site_rules() -> serde_json::Value {
    crate::model::config_dir()
        .and_then(|d| std::fs::read_to_string(d.join("site_rules.json")).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(|v: &serde_json::Value| v.is_object())
        // Before the app has written its copy: the rules it was built with.
        .or_else(|| serde_json::from_str(DEFAULT_SITE_RULES).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// Folder for page-fetch requests (app -> extension) and their results.
pub fn page_fetch_dir() -> Option<std::path::PathBuf> {
    crate::model::config_dir().map(|d| d.join("pagefetch"))
}

fn safe_fetch_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// One page-fetch answer may carry this much decoded data at most.
const MAX_PAGE_FETCH_BYTES: u64 = 64 * 1024 * 1024;
/// Requests older than this are dead: the app stopped waiting long ago.
const FETCH_REQUEST_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// Results and partial answers are swept after a day.
const FETCH_FILE_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
/// `asked` entries are forgotten once their request is gone; this is the
/// hard cap for a page that keeps creating new ones.
const MAX_ASKED: usize = 512;

fn is_regular_file(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.is_file() && !m.file_type().is_symlink())
        .unwrap_or(false)
}

fn older_than(path: &std::path::Path, age: std::time::Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|elapsed| elapsed > age)
}

/// Remove page-fetch files nobody will read anymore.
fn cleanup_page_fetch_dir(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.ends_with(".req.json") || name.ends_with(".part") || name.ends_with(".res") || name.ends_with(".err")) {
            continue;
        }
        let path = e.path();
        if older_than(&path, FETCH_FILE_TTL) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// One queue row as an event for the extension.
fn row_event(d: &crate::model::DownloadItem, event: &str, error: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "event", "event": event, "id": d.id, "url": d.url, "name": d.filename,
        "error": error, "page": d.referer, "quality": d.quality_pref,
        "path": d.dest_path().to_string_lossy(),
    })
}

/// Pushes settings, finished/failed downloads and page-fetch requests to the
/// extension until the browser closes the port (the process then exits).
fn push_loop() {
    use crate::model::{DownloadItem, Status};
    let mut last_settings = serde_json::Value::Null;
    let mut known: std::collections::HashMap<u64, u8> = std::collections::HashMap::new();
    let mut first = true;
    let mut asked: std::collections::HashMap<String, std::time::Instant> = std::collections::HashMap::new();
    let mut last_cleanup = std::time::Instant::now();
    loop {
        let settings = serde_json::json!({ "type": "settings", "settings": capture_settings(), "rules": site_rules() });
        if settings != last_settings {
            write_value(&settings);
            last_settings = settings;
        }
        // Rows the extension sent: tell it when they finish or fail.
        let items: Vec<DownloadItem> = crate::model::config_dir()
            .and_then(|d| std::fs::read_to_string(d.join("queue.json")).ok())
            .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default();
        for d in &items {
            if !d.browser.as_ref().is_some_and(|b| b.from_extension) {
                continue;
            }
            let state = match &d.status {
                Status::Completed => 2,
                Status::Failed(_) => 3,
                _ => 1,
            };
            let before = known.insert(d.id, state);
            if first || before == Some(state) || state == 1 {
                continue;
            }
            match &d.status {
                Status::Completed => write_value(&row_event(d, "done", "")),
                Status::Failed(e) => write_value(&row_event(d, "failed", e)),
                _ => {}
            }
        }
        first = false;
        if last_cleanup.elapsed() >= std::time::Duration::from_secs(60) {
            if let Some(dir) = page_fetch_dir() {
                cleanup_page_fetch_dir(&dir);
            }
            last_cleanup = std::time::Instant::now();
        }
        // Page-fetch requests the app is waiting on.
        let now = std::time::Instant::now();
        asked.retain(|_, requested_at| now.duration_since(*requested_at) < FETCH_REQUEST_TTL);
        if let Some(dir) = page_fetch_dir() {
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    let Some(id) = name.strip_suffix(".req.json") else { continue };
                    if !safe_fetch_id(id) || asked.contains_key(id) {
                        continue;
                    }
                    if asked.len() >= MAX_ASKED {
                        if let Some(oldest) = asked.iter().min_by_key(|(_, at)| **at).map(|(key, _)| key.clone()) {
                            asked.remove(&oldest);
                        }
                    }
                    asked.insert(id.to_string(), now);
                    if let Some(req) = std::fs::read_to_string(e.path()).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
                        write_value(&serde_json::json!({
                            "type": "pageFetch", "id": id,
                            "url": req.get("url"), "page": req.get("page"),
                        }));
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(700));
    }
}

/// Store one piece of a page-fetch answer; the last piece publishes it.
fn page_fetch_chunk(msg: &Incoming) -> Result<(), String> {
    let id = msg.fetch_id.clone().unwrap_or_default();
    if !safe_fetch_id(&id) {
        return Err("bad fetch id".into());
    }
    let dir = page_fetch_dir().ok_or("no config dir")?;
    if !dir.join(format!("{id}.req.json")).exists() {
        return Err("no such fetch request".into());
    }
    let part = dir.join(format!("{id}.part"));
    if let Some(err) = msg.error.clone().filter(|e| !e.is_empty()) {
        let _ = std::fs::remove_file(&part);
        let body = serde_json::json!({ "error": err, "status": msg.status });
        return std::fs::write(dir.join(format!("{id}.err")), body.to_string()).map_err(|e| e.to_string());
    }
    if let Some(c) = msg.chunk.as_deref().filter(|c| !c.is_empty()) {
        let bytes = crate::model::base64_decode(c).ok_or("bad chunk")?;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&part).map_err(|e| e.to_string())?;
        f.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    if msg.done {
        if !part.exists() {
            std::fs::write(&part, b"").map_err(|e| e.to_string())?;
        }
        std::fs::rename(&part, dir.join(format!("{id}.res"))).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Just the name of a suggested filename. Browsers report a download's whole
/// save path (Linux: "/home/me/Downloads/x.zip"), and cleaning that as one
/// name gave "_home_me_Downloads_x.zip" (issue #6).
fn file_part(name: &str) -> Option<String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if last.is_empty()
        || matches!(last, "." | "..")
        || last.contains(':') // Includes Windows alternate data stream syntax.
        || last.chars().any(char::is_control)
    {
        return None;
    }
    Some(last.to_string())
}

fn preferred_referer(page_url: Option<&str>, referer: Option<&str>) -> Option<String> {
    let page = page_url.filter(|s| !s.trim().is_empty());
    let captured = referer.filter(|s| !s.trim().is_empty());
    let has_path = |u: &str| {
        reqwest::Url::parse(u)
            .map(|parsed| parsed.path() != "/" && !parsed.path().is_empty())
            .unwrap_or(false)
    };
    match (page, captured) {
        (Some(page), Some(captured)) if captured != page && has_path(captured) => Some(captured.to_string()),
        (Some(page), _) => Some(page.to_string()),
        (None, Some(captured)) => Some(captured.to_string()),
        (None, None) => None,
    }
}

/// Keep only headers the engine may replay, with sane sizes. The extension
/// already filters; this is the trust boundary, so filter again.
fn replayable_headers(
    headers: Option<&std::collections::BTreeMap<String, String>>,
) -> Option<std::collections::BTreeMap<String, String>> {
    let kept: std::collections::BTreeMap<String, String> = headers?
        .iter()
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .filter(|(k, v)| {
            crate::engine::replayable_header(k)
                && k.len() <= 64
                && !v.is_empty()
                && v.len() <= 4096
                && !v.contains(['\r', '\n'])
        })
        .take(20)
        .collect();
    (!kept.is_empty()).then_some(kept)
}

fn queue_download(msg: &Incoming) -> Result<bool, String> {
    const MAX_INBOX: usize = 200;
    let url = msg.url.clone().unwrap_or_default();
    let path = inbox_path().ok_or_else(|| "no config dir".to_string())?;
    let receipts_path = receipt_path().ok_or_else(|| "no config dir".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _guard = crate::model::fs_lock("inbox");
    if !_guard.owned() { return Err("Download inbox is busy; retry shortly".into()); }
    let now = chrono::Utc::now().timestamp();
    let mut receipts = load_receipts(&receipts_path, now)?;
    let capture_id = msg.capture_id.as_deref().filter(|id| valid_capture_id(id));
    let window_name = confirm_window_name(&url);
    let dialog_active = crate::model::window_is_claimed(&window_name);
    if let Some(id) = capture_id {
        if let Some(receipt) = receipts.iter_mut().find(|r| r.capture_id == id) {
            if dialog_active {
                receipt.dialog_pending = false;
            }
            let should_retry_dialog = receipt.dialog_pending && !dialog_active;
            save_receipts(&receipts_path, &receipts)?;
            return Ok(should_retry_dialog);
        }
    }
    // Never destroy pending captures on an unreadable inbox.
    let mut items: Vec<InboxItem> = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("could not read download inbox: {e}")),
        Ok(s) => serde_json::from_str(&s)
            .map_err(|_| "inbox unreadable — kept for the next poll".to_string())?,
    };
    if let Some(id) = capture_id {
        if items.iter().any(|item| item.capture_id.as_deref() == Some(id)) {
            record_capture_locked(&mut receipts, id, !dialog_active, now)?;
            save_receipts(&receipts_path, &receipts)?;
            return Ok(!dialog_active);
        }
    }
    let referer = preferred_referer(msg.page_url.as_deref(), msg.referer.as_deref());
    let fresh = InboxItem {
        url: url.clone(),
        filename: msg.filename.as_deref().and_then(file_part),
        received_at: chrono::Local::now().to_rfc3339(),
        capture_id: capture_id.map(str::to_string),
        page_url: msg.page_url.clone(),
        referer,
        cookie: msg.cookie.clone().filter(|s| !s.trim().is_empty()),
        headers: replayable_headers(msg.headers.as_ref()),
        mime: msg.mime.clone().filter(|s| !s.trim().is_empty()),
        size: msg.size.filter(|s| *s > 0),
        pair_url: msg.pair_url.clone().filter(|s| s.starts_with("http")),
        pair_mime: msg.pair_mime.clone().filter(|s| !s.trim().is_empty()),
        urls: None,
        quality: msg.quality.clone().filter(|s| !s.trim().is_empty()),
        dur: msg.dur.filter(|s| *s > 0),
        audio_expected: msg.audio_expected,
        browser: msg.browser.clone().and_then(|b| b.sanitized()),
    };
    // Same URL re-captured: refresh metadata instead of ignoring it (the
    // filename/referer/cookie may be corrected this time. Keep the inbox
    // identity stable so the GPUI manager can refresh its existing dialog
    // instead of opening a second one for a new timestamp.
    let is_new = match items.iter_mut().find(|i| i.url == url) {
        Some(existing) => {
            let previous_capture_id = existing.capture_id.clone();
            let previous_received_at = existing.received_at.clone();
            *existing = fresh;
            existing.received_at = previous_received_at;
            if previous_capture_id.is_some() {
                existing.capture_id = previous_capture_id;
            }
            false
        }
        None => {
            items.push(fresh);
            true
        }
    };
    if dialog_active && is_new {
        // Do not leave a second same-URL row behind. If a row is still
        // pending, the merge above refreshes its metadata for the open dialog.
        items.pop();
        if let Some(id) = capture_id {
            record_capture_locked(&mut receipts, id, false, now)?;
            save_receipts(&receipts_path, &receipts)?;
        }
        return Ok(false);
    }
    if items.len() > MAX_INBOX {
        items.sort_by(|a, b| b.received_at.cmp(&a.received_at));
        items.truncate(MAX_INBOX);
    }
    let json = serde_json::to_vec_pretty(&items).map_err(|e| e.to_string())?;
    crate::model::atomic_write(&path, &json).map_err(|e| e.to_string())?;
    if let Some(id) = capture_id {
        record_capture_locked(&mut receipts, id, is_new, now)?;
        save_receipts(&receipts_path, &receipts)?;
    }
    Ok(is_new)
}

fn mark_capture_dialog_spawned(capture_id: &str) {
    if !valid_capture_id(capture_id) {
        return;
    }
    let Some(path) = receipt_path() else { return };
    let _guard = crate::model::fs_lock("inbox");
    if !_guard.owned() {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let Ok(mut receipts) = load_receipts(&path, now) else { return };
    if let Some(receipt) = receipts.iter_mut().find(|r| r.capture_id == capture_id) {
        receipt.dialog_pending = false;
        let _ = save_receipts(&path, &receipts);
    }
}

/// Fire-and-forget the standalone Download File Info dialog (detached, so
/// it outlives this short-lived native-host process).
fn spawn_confirm(msg: &Incoming) -> bool {
    let url = msg.url.clone().unwrap_or_default();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return false;
    }
    let Ok(exe) = crate::platform::launch_path() else {
        return false;
    };
    let mut cmd = crate::platform::silent_command(exe);
    cmd.arg("--confirm-download").arg("--url").arg(&url);
    if let Some(id) = msg.capture_id.as_deref().filter(|id| valid_capture_id(id)) {
        cmd.arg("--capture-id").arg(id);
    }
    if let Some(f) = msg.filename.as_deref().and_then(file_part) {
        cmd.arg("--filename").arg(f);
    }
    // The engine reads this as both the Referer and "the page the media came
    // from" (YouTube fresh-link retry, social page fallback), so the page
    // wins over an origin-only Referer ("https://www.youtube.com/"). A
    // Referer with a real path is an embedded player's page, which the CDN
    // checks (sending the article URL instead got 403s): that one wins.
    if let Some(r) = preferred_referer(msg.page_url.as_deref(), msg.referer.as_deref()) {
        cmd.arg("--referer").arg(r);
    }
    // The dialog claims credentials from the owner-only inbox.
    if let Some(m) = msg.mime.clone().filter(|s| !s.trim().is_empty()) {
        cmd.arg("--mime").arg(m);
    }
    if let Some(s) = msg.size.filter(|s| *s > 0) {
        cmd.arg("--size").arg(s.to_string());
    }
    if let Some(p) = msg.pair_url.clone().filter(|s| s.starts_with("http")) {
        cmd.arg("--pair-url").arg(p);
        if let Some(m) = msg.pair_mime.clone().filter(|s| !s.trim().is_empty()) {
            cmd.arg("--pair-mime").arg(m);
        }
    }
    if let Some(q) = msg.quality.clone().filter(|s| !s.trim().is_empty()) {
        cmd.arg("--quality").arg(q);
    }
    if msg.audio_expected { cmd.arg("--expect-audio"); }
    if let Some(d) = msg.dur.filter(|s| *s > 0) {
        cmd.arg("--dur").arg(d.to_string());
    }
    spawn_detached(cmd)
}

/// The extension just talked to the app: the "Get ready" window shows it as
/// connected from now on.
fn note_extension_seen() {
    if let Some(p) = crate::model::config_dir().map(|d| d.join("extension_seen.json")) {
        let origin = std::env::args().find(|a| a.contains("-extension://")).unwrap_or_default();
        let v = serde_json::json!({ "at": chrono::Local::now().to_rfc3339(), "origin": origin });
        let _ = std::fs::write(p, v.to_string());
    }
}

/// Entry point when launched with `--native-host`. Never returns normally.
pub fn run() {
    // Keep stderr quiet; any panic text would corrupt the pipe (stdout stays clean).
    loop {
        match read_message() {
            HostFrame::Eof => break,
            HostFrame::Bad(e) => {
                write_message(&Reply::err(e));
                continue;
            }
            HostFrame::Msg(msg) => {
            *CURRENT_RID.lock().unwrap_or_else(|e| e.into_inner()) = msg.rid;
            match msg.kind.as_str() {
            "hello" => {
                note_extension_seen();
                if !PUSHING.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    std::thread::spawn(push_loop);
                }
                write_message(&Reply {
                    kind: "pong".into(),
                    version: Some(2),
                    queued: None,
                    error: None,
                    urls: None,
                    theme: Some(app_theme()),
                    items: None,
                    app_state: Some(crate::model::app_lifecycle_state()),
                });
            }
            "setPrefs" => match msg.prefs.as_ref().map(set_prefs).unwrap_or(Ok(())) {
                Ok(()) => write_message(&Reply::ok()),
                Err(e) => write_message(&Reply::err(e)),
            },
            "pageFetchChunk" => match page_fetch_chunk(&msg) {
                Ok(()) => write_message(&Reply::ok()),
                Err(e) => write_message(&Reply::err(e)),
            },
            "report" => {
                if let Some(text) = msg.note.as_deref() {
                    append_extension_note(text);
                }
                write_message(&Reply::ok());
            }
            "ping" => write_message(&{ note_extension_seen(); Reply {
                kind: "pong".into(),
                version: Some(1),
                queued: None,
                error: None,
                urls: None,
                theme: Some(app_theme()),
                items: None,
                app_state: Some(crate::model::app_lifecycle_state()),
            } }),
            "query" => {
                // Presence check only: no queueing, no dialogs, no locks held
                // beyond the read.
                let urls = msg.urls.clone().unwrap_or_default();
                let hits = queued_urls(&urls);
                write_message(&Reply {
                    kind: "queued".into(),
                    version: None,
                    queued: None,
                    error: None,
                    urls: Some(hits),
                    theme: None,
                    items: None,
                    app_state: None,
                });
            }
            "download" => {
                let url = msg.url.clone().unwrap_or_default();
                let ok = url.starts_with("http://") || url.starts_with("https://");
                if !ok {
                    write_message(&Reply::err("need http/https url"));
                    continue;
                }
                // Blob/media-source URLs are not directly downloadable by the
                // engine — reject loudly so the extension can fall back to the
                // page URL instead of queueing a dead link.
                if url.starts_with("blob:") {
                    write_message(&Reply::err(
                        "blob: URLs need the page URL — send the page link instead",
                    ));
                    continue;
                }
                match queue_download(&msg) {
                    // A row is waiting for a fresh link ("Refresh download
                    // address"): the app takes this capture, no dialog.
                    Ok(_) if crate::model::refresh_wait().is_some() => {
                        if let Some(id) = msg.capture_id.as_deref() {
                            mark_capture_dialog_spawned(id);
                        }
                        write_message(&Reply::ok());
                    }
                    Ok(should_prompt) => {
                        write_message(&Reply::ok());
                        let gui_accepts_captures = matches!(
                            crate::model::app_lifecycle_state().as_str(),
                            "starting" | "running"
                        );
                        if should_prompt && !gui_accepts_captures {
                            // IDM-style: one dialog for this URL. A request
                            // retry may spawn a second short-lived process,
                            // but the per-URL window lock admits only one.
                            // When the main GPUI process is accepting captures,
                            // it reads this durable inbox row and mounts the
                            // confirmation in its own window instead.
                            if spawn_confirm(&msg) {
                                if let Some(id) = msg.capture_id.as_deref() {
                                    mark_capture_dialog_spawned(id);
                                }
                            }
                        }
                    }
                    Err(e) => write_message(&Reply::err(e)),
                }
            }
            "list" => write_message(&Reply {
                kind: "list".into(),
                version: None,
                queued: None,
                error: None,
                urls: None,
                theme: None,
                items: Some(app_downloads()),
                app_state: None,
            }),
            "record" => match with_request_receipt(
                msg.request_id.as_deref(),
                "record",
                || record_browser_download(&msg),
            ) {
                Ok(()) => write_message(&Reply::ok()),
                Err(e) => write_message(&Reply::err(e)),
            },
            "control" => match with_request_receipt(msg.request_id.as_deref(), "control", || {
                app_control(msg.id, msg.action.as_deref().unwrap_or(""))
            }) {
                Ok(()) => write_message(&Reply::ok()),
                Err(e) => write_message(&Reply::err(e)),
            },
            "links" => match with_request_receipt(
                msg.request_id.as_deref(),
                "links",
                || spawn_links(&msg),
            ) {
                Ok(()) => write_message(&Reply::ok()),
                Err(e) => write_message(&Reply::err(e)),
            },
            _ => write_message(&Reply::err("unknown message")),
            }
            }
        }
    }
}

static PUSHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
mod media_tests {
    use super::*;
    #[test]
    fn capture_ids_are_read_from_the_extension_and_validated() {
        let msg: Incoming = serde_json::from_str(r#"{"type":"download","url":"https://e.test/a","captureId":"capture-1234"}"#).unwrap();
        assert_eq!(msg.capture_id.as_deref(), Some("capture-1234"));
        assert!(valid_capture_id("capture-1234"));
        assert!(!valid_capture_id("../capture"));
        assert!(!valid_capture_id("short"));
    }

    #[test]
    fn capture_receipts_are_idempotent_and_expire() {
        let now = chrono::Utc::now().timestamp();
        let mut receipts = Vec::new();
        record_capture_locked(&mut receipts, "capture-1234", true, now).unwrap();
        record_capture_locked(&mut receipts, "capture-1234", false, now + 1).unwrap();
        assert_eq!(receipts.len(), 1);
        assert!(!receipts[0].dialog_pending);

        let dir = std::env::temp_dir().join(format!("dc_receipt_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("receipts.json");
        save_receipts(&path, &receipts).unwrap();
        let loaded = load_receipts(&path, now + 2).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].capture_id, "capture-1234");

        let expired = load_receipts(&path, now + RECEIPT_TTL_SECS + 2).unwrap();
        assert!(expired.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn confirmation_slot_name_is_stable_per_url() {
        assert_eq!(confirm_window_name("https://e.test/a"), confirm_window_name("https://e.test/a"));
        assert_ne!(confirm_window_name("https://e.test/a"), confirm_window_name("https://e.test/b"));
    }

    #[test]
    fn player_expectations_survive_browser_and_inbox_formats() {
        let msg: Incoming = serde_json::from_str(r#"{"type":"download","url":"https://cdn.example.org/video.mp4","dur":2700,"audioExpected":true}"#).unwrap();
        assert_eq!(msg.dur, Some(2700));
        assert!(msg.audio_expected);
        let stored: InboxItem = serde_json::from_str(r#"{"url":"https://cdn.example.org/video.mp4","filename":null,"received_at":"now","dur":2700,"audio_expected":true}"#).unwrap();
        assert_eq!(stored.dur, Some(2700));
        assert!(stored.audio_expected);
        let old: Incoming = serde_json::from_str(r#"{"type":"download","url":"https://cdn.example.org/video.mp4"}"#).unwrap();
        assert!(!old.audio_expected);
        assert!(old.headers.is_none());
    }

    #[test]
    fn captured_headers_keep_only_replayable_tokens() {
        let msg: Incoming = serde_json::from_str(r#"{"type":"download","url":"https://cdn.example.org/v",
            "headers":{"Origin":"https://site.example","authorization":"Bearer t","x-token":"1",
                       "x-client-data":"variations","Range":"bytes=0-","host":"evil","x-bad":"a\r\nb: c"}}"#).unwrap();
        let kept = replayable_headers(msg.headers.as_ref()).unwrap();
        assert_eq!(kept.keys().cloned().collect::<Vec<_>>(), ["authorization", "origin", "x-token"]);
        assert!(replayable_headers(None).is_none());
        let none = std::collections::BTreeMap::from([("cookie".to_string(), "a=b".to_string())]);
        assert!(replayable_headers(Some(&none)).is_none());
    }

    #[test]
    fn browser_launches_without_the_flag_are_recognised() {
        let ext = format!("chrome-extension://{}/", crate::integration::EXTENSION_ID);
        assert!(launched_directly(&["dc.exe".into(), ext, "--parent-window=0".into()]));
        assert!(launched_directly(&["dc.exe".into(), r"C:\m.json".into(), "send-to-download-craft@example".into()]));
        assert!(!launched_directly(&["dc.exe".into(), "chrome-extension://someoneelse/".into()]));
        assert!(!launched_directly(&["dc.exe".into(), "--settings".into()]));
    }

    #[test]
    fn suggested_paths_keep_only_the_file_name() {
        assert_eq!(file_part("/home/me/Downloads/a.zip").as_deref(), Some("a.zip"));
        assert_eq!(file_part(r"C:\Users\me\Downloads\b.exe").as_deref(), Some("b.exe"));
        assert_eq!(file_part("plain.txt").as_deref(), Some("plain.txt"));
        assert_eq!(file_part("/home/me/"), None);
        assert_eq!(file_part("../../safe.txt").as_deref(), Some("safe.txt"));
        assert_eq!(file_part("file.txt:stream"), None);
        assert_eq!(file_part("bad\0name.txt"), None);
        assert_eq!(file_part("bad\u{0007}name.txt"), None);
    }

    #[test]
    fn browser_context_is_sanitized_at_the_inbox() {
        let msg: Incoming = serde_json::from_str(r#"{"type":"download","url":"https://site.example/r","rid":7,
            "browser":{"method":"post","body":"YT0xJmI9Mg==","fromExtension":true,"serverIp":"203.0.113.9",
              "proxy":"socks5://127.0.0.1:9050","headers":{"User-Agent":"UA/1","Cookie":"a=b","Accept":"*/*","Range":"bytes=0-"}}}"#).unwrap();
        assert_eq!(msg.rid, Some(7));
        let b = msg.browser.clone().unwrap().sanitized().unwrap();
        assert!(b.is_post());
        assert_eq!(crate::model::base64_decode(&b.body).unwrap(), b"a=1&b=2");
        assert!(b.from_extension);
        assert_eq!(b.server_ip, "203.0.113.9");
        assert_eq!(b.proxy, "socks5://127.0.0.1:9050");
        assert_eq!(b.headers.keys().cloned().collect::<Vec<_>>(), ["accept", "user-agent"]);
        let bad: crate::model::BrowserContext = serde_json::from_str(r#"{"method":"DELETE","serverIp":"x","proxy":"file:///etc"}"#).unwrap();
        assert!(bad.sanitized().is_none());
        assert_eq!(crate::model::base64_encode(b"a=1&b=2"), "YT0xJmI9Mg==");
        assert!(safe_fetch_id("f1a2-3") && !safe_fetch_id("../x") && !safe_fetch_id(""));
    }

    #[test]
    fn built_in_site_rules_parse() {
        let v: serde_json::Value = serde_json::from_str(DEFAULT_SITE_RULES).unwrap();
        assert!(v.get("version").and_then(|x| x.as_u64()).unwrap() >= 1);
        assert!(v.get("youtube").and_then(|y| y.get("itagQuality")).is_some());
    }

    #[test]
    fn page_links_keep_only_unique_web_urls() {
        let msg: Incoming = serde_json::from_str(r#"{"type":"links","pageUrl":"https://site.example/dir/",
            "links":[{"url":"https://site.example/a.mp4","text":"A\u0007 video"},
                     {"url":"https://site.example/a.mp4","text":"again"},
                     {"url":"javascript:alert(1)"},{"url":"file:///c:/x"},
                     {"url":"http://other.example/b.zip"}]}"#).unwrap();
        let b = links_batch("https://site.example/dir/", "", "", msg.links.as_deref().unwrap());
        assert_eq!(b.links.len(), 2);
        assert_eq!(b.links[0].text, "A video");
        assert_eq!(b.links[1].url, "http://other.example/b.zip");
    }
}
