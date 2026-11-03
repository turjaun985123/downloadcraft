//! Shared browser-download confirmation state and engine behavior.
//!
//! This module intentionally contains no windowing toolkit types.

use crate::capture_service::{launch_main_app, load_prefs, main_app_running, save_remember_category, wake_ids};
use crate::engine::{self, EngineOpts};
use crate::model::{atomic_write, category_of, filename_from_url, is_download_url, DownloadItem};
use std::sync::mpsc::Receiver;

/// Claim (remove) our URL from the native inbox so the main window does not
/// prompt for it too. Runs at dialog open; Cancel therefore discards.
fn claim_inbox(url: &str, capture_id: Option<&str>) -> Option<crate::native_host::InboxItem> {
    let path=crate::native_host::inbox_path()?;
    let guard=crate::model::fs_lock("inbox");
    if !guard.owned() {return None;}
    claim_inbox_at(&path,url,capture_id)
}
pub(crate) fn claim_inbox_at(path:&std::path::Path,url:&str,capture_id:Option<&str>) -> Option<crate::native_host::InboxItem> {
    let data=std::fs::read_to_string(path).ok()?;
    // Retain unreadable pending captures for recovery.
    let items:Vec<crate::native_host::InboxItem>=serde_json::from_str(&data).ok()?;
    let index=items.iter().position(|i| match capture_id {
        Some(id) => i.capture_id.as_deref() == Some(id),
        None => i.url == url,
    })?;
    let mut rest=items;
    let claimed=rest.remove(index);
    if rest.is_empty() {std::fs::remove_file(path).ok()?;}
    else {atomic_write(path,serde_json::to_string_pretty(&rest).ok()?.as_bytes()).ok()?;}
    Some(claimed)
}

pub(crate) const NOT_RUNNING: &str = "Download Craft isn't running — open it, then click Download again.";
pub(crate) const STARTING: &str = "Starting Download Craft…";
/// How long Download waits for the app it started before giving up.
pub(crate) const START_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The download the dialog starts while it is still open (IDM's way): by
/// the time Download is pressed, part of the file is already there.
pub(crate) struct PreStart {
    pub(crate) shared: std::sync::Arc<engine::SharedState>,
    pub(crate) handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    /// The temporary file it downloads to.
    pub(crate) dest: std::path::PathBuf,
}

pub(crate) struct ConfirmApp {
    pub(crate) pre: Option<PreStart>,
    pub(crate) pre_tried: bool,
    pub(crate) url: String,
    pub(crate) filename: String,
    pub(crate) category: String,
    pub(crate) remember_path: bool,
    pub(crate) save_base: String,
    pub(crate) description: String,
    pub(crate) referer: String,
    pub(crate) cookie: String,
    /// Captured request headers of this browser capture (from the inbox).
    pub(crate) headers: std::collections::BTreeMap<String, String>,
    /// The browser's side of the capture (method/body, headers, IP, proxy).
    pub(crate) browser: Option<crate::model::BrowserContext>,
    pub(crate) pair_url: String,
    pub(crate) pair_mime: String,
    /// Page/media MIME from the extension (first hint for naming when the
    /// URL carries no usable name and no probe has run yet).
    pub(crate) page_mime: String,
    /// Media duration in seconds from the extension (best-effort badge).
    pub(crate) duration_secs: u64,
    pub(crate) expects_audio: bool,
    pub(crate) quality_pref: String,
    pub(crate) connections: usize,
    pub(crate) user_agent: String,
    pub(crate) proxy_url: String,
    pub(crate) timeout_secs: u64,
    pub(crate) rename_template: String,
    pub(crate) folder_rules: Vec<crate::model::FolderRule>,
    pub(crate) probing: bool,
    pub(crate) probe_size: Option<u64>,
    /// Size the browser measured in its own session (`--size`): what a
    /// refused off-session re-probe falls back to instead of "—".
    pub(crate) browser_size: Option<u64>,
    /// The name the dialog made up itself (no `--filename`), while the user
    /// has not changed it: a server/title name may replace exactly that.
    pub(crate) auto_name: Option<String>,
    pub(crate) probe_rx: Option<Receiver<(Option<u64>, Option<String>)>>,
    pub(crate) rt: Option<tokio::runtime::Handle>,
    pub(crate) centered: bool,
    pub(crate) show_adv: bool,
    pub(crate) done: bool,
    /// Download was pressed while the app was closed: it was started at this
    /// moment and the download is handed over once it is live (issue #7).
    pub(crate) waiting_app: Option<std::time::Instant>,
    pub(crate) exited: bool,
    pub(crate) notice: String,
    /// State for controls the concept does not show (dropdown + extras).
    pub(crate) cat_open: bool,
    /// Path field's recent-folders menu (same pattern as the category combo).
    pub(crate) recent_open: bool,
    pub(crate) win_h: f32,
    pub(crate) adding_cat: bool,
    pub(crate) new_cat: String,
    pub(crate) custom_cats: Vec<String>,
}

pub(crate) const CATEGORIES: &[&str] = &[
    "Video",
    "Music",
    "Image",
    "Document",
    "Compressed",
    "Apps",
    "Other",
];

impl ConfirmApp {
    pub(crate) fn effective_dir(&self) -> String {
        if self.remember_path {
            // Ordered user rules first, category fallback second (mirrors app).
            if let Some(rule) = self
                .folder_rules
                .iter()
                .find(|r| r.matches(&self.filename))
            {
                if !rule.folder.trim().is_empty() {
                    let mut p = std::path::PathBuf::from(&self.save_base);
                    p.push(rule.folder.trim());
                    return p.to_string_lossy().to_string();
                }
            }
            let mut p = std::path::PathBuf::from(&self.save_base);
            p.push(self.category.clone());
            p.to_string_lossy().to_string()
        } else {
            self.save_base.clone()
        }
    }

    /// Recent folders for the path field's caret: the shell folders first,
    /// then whatever the field currently holds (mirrors the main window).
    pub(crate) fn recent_dirs(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for d in [
            dirs::download_dir(),
            dirs::desktop_dir(),
            dirs::document_dir(),
            dirs::picture_dir(),
            dirs::video_dir(),
            dirs::audio_dir(),
        ]
        .into_iter()
        .flatten()
        {
            let s = d.to_string_lossy().to_string();
            if !out.contains(&s) {
                out.push(s);
            }
        }
        let cur = self.save_base.trim().to_string();
        if !cur.is_empty() && !out.contains(&cur) {
            out.push(cur);
        }
        out
    }

    /// Extra one-line facts the concept has no place for; shown under the
    /// hidden extras panel only.
    pub(crate) fn extra_notes(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.pair_url.starts_with("http") {
            v.push("video-only + audio — merged on completion (needs ffmpeg)".to_string());
        }
        if !self.quality_pref.trim().is_empty() {
            v.push(format!("Quality: {}", self.quality_pref.trim()));
        }
        v
    }

    /// Start downloading in the background while the dialog is open, for a
    /// plain file whose size is known (not a video page, a stream or an
    /// audio/video pair: those the app assembles itself). Once only.
    pub(crate) fn maybe_prestart(&mut self) {
        if self.pre_tried || self.probing || self.done {
            return;
        }
        let url = self.url.trim().to_string();
        let plain = is_download_url(&url)
            && self.pair_url.is_empty()
            && self.quality_pref.trim().is_empty()
            && !self.page_mime.contains("text/html")
            && !engine::is_stream_manifest(&url, None, &self.page_mime)
            && !url.contains("youtube.com")
            && !url.contains("youtu.be");
        if !plain || self.probe_size.unwrap_or(0) == 0 {
            return;
        }
        self.pre_tried = true;
        let Some(rt) = self.rt.clone() else { return };
        let name = engine::sanitize_filename(if self.filename.trim().is_empty() { "download" } else { self.filename.trim() });
        let dir = std::env::temp_dir().join("download-craft-pre").join(format!(
            "{}-{}",
            std::process::id(),
            chrono::Local::now().timestamp_millis()
        ));
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let dest = dir.join(name);
        let opts = EngineOpts {
            user_agent: if self.user_agent.trim().is_empty() {
                format!("download_craft/{}", crate::model::APP_VERSION)
            } else {
                self.user_agent.clone()
            },
            timeout_secs: self.timeout_secs,
            proxy_url: self.proxy_url.clone(),
            referer: self.referer.clone(),
            cookie: self.cookie.clone(),
            cookie_scope: engine::CredentialScope::for_url(&url),
            captured_headers: engine::CapturedHeaders::for_url(&url, &self.headers),
            browser: self.browser.clone(),
            allow_rename: false,
            ..Default::default()
        };
        let shared = std::sync::Arc::new(engine::SharedState::new());
        let s2 = shared.clone();
        let d2 = dest.clone();
        let conns = self.connections.max(1);
        let handle = rt.spawn(async move { engine::download(url, d2, conns, opts, s2).await });
        self.pre = Some(PreStart { shared, handle, dest });
    }

    /// Stop the background download. `keep_as`: continue it as this file
    /// (its data is moved there; a file that already finished is moved
    /// whole and the return says so). Without it everything is deleted.
    pub(crate) fn stop_prestart(&mut self, keep_as: Option<&std::path::Path>) -> bool {
        let Some(pre) = self.pre.take() else { return false };
        pre.shared.cancel();
        let finished = pre.handle.is_finished() && engine::has_partial(&pre.dest) == false && pre.dest.is_file();
        pre.handle.abort();
        // Let the transfer write its journal and let go of the files.
        std::thread::sleep(std::time::Duration::from_millis(250));
        let mut whole = false;
        if let Some(to) = keep_as {
            if finished && !to.exists() {
                if let Some(dir) = to.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                whole = std::fs::rename(&pre.dest, to).is_ok()
                    || (std::fs::copy(&pre.dest, to).is_ok() && std::fs::remove_file(&pre.dest).is_ok());
            } else if !finished {
                engine::move_partial(&pre.dest, to);
            }
        }
        engine::remove_partial(&pre.dest);
        if let Some(dir) = pre.dest.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
        whole
    }

    pub(crate) fn start_probe(&mut self) {
        if !is_download_url(&self.url) {
            return;
        }
        let Some(rt) = self.rt.clone() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.probe_rx = Some(rx);
        self.probing = true;
        self.probe_size = None;
        let url = self.url.clone();
        let referer = self.referer.clone();
        // Quality choice feeds the page resolve so the dialog measures the
        // chosen stream's size, not just the page's (IDM shows size upfront).
        let quality_pref = self.quality_pref.trim().to_string();
        // Bare URL guess to correct with the MIME type when the server
        // sends no disposition (P1: `download_123.bin` + video/mp4).
        let guess_name = self.filename.clone();
        let page_mime = self.page_mime.clone();
        let opts = EngineOpts {
            user_agent: if self.user_agent.trim().is_empty() {
                format!("download_craft/{}", crate::model::APP_VERSION)
            } else {
                self.user_agent.clone()
            },
            timeout_secs: self.timeout_secs,
            proxy_url: self.proxy_url.clone(),
            referer,
            // The browser session claimed from the inbox: without it a
            // login-gated file probes as 403 or as the login page's size.
            cookie: self.cookie.clone(),
            cookie_scope: engine::CredentialScope::for_url(&url),
            quality_pref,
            captured_headers: engine::CapturedHeaders::for_url(&url, &self.headers),
            browser: self.browser.clone(),
            ..Default::default()
        };
        rt.spawn(async move {
            let out = match opts.build_client() {
                Ok(client) => {
                    let (mut total, _, ctype, mut disp) =
                        engine::probe(&client, &opts, &url).await;
                    // A playlist's length is its text, not the video: show
                    // the duration instead (as IDM does for streams).
                    if engine::is_stream_manifest(&url, ctype.as_deref(), &page_mime) {
                        total = None;
                    }
                    let is_page = ctype
                        .as_deref()
                        .map(|c| c.contains("text/html"))
                        .unwrap_or(false)
                        || url.contains("youtube.com")
                        || url.contains("youtu.be");
                    if total.is_none() && is_page {
                        let shared =
                            std::sync::Arc::new(engine::SharedState::new());
                        if let Some(hit) =
                            engine::resolve_media(&client, &opts, &url, &shared).await
                        {
                            let (t2, _, _, _) =
                                engine::probe(&client, &opts, &hit.url).await;
                            if total.is_none() {
                                total = t2;
                            }
                            if let Some(pair) = hit.pair_url.filter(|p| p.starts_with("http")) {
                                let (ta, _, _, _) =
                                    engine::probe(&client, &opts, &pair).await;
                                if let (Some(a), Some(b)) = (total, ta) {
                                    total = Some(a.saturating_add(b));
                                } else {
                                    total = total.or(ta);
                                }
                            }
                            if disp.is_none() && !hit.title.trim().is_empty() {
                                disp = Some(engine::sanitize_filename(&hit.title));
                            }
                        }
                    }
                    if disp.is_none()
                        && crate::model::is_fallback_filename(&guess_name)
                    {
                        // No disposition and no media title: at least fix the
                        // extension — probed MIME first, extension hint when
                        // the probe itself was refused off-session.
                        let ext = ctype
                            .as_deref()
                            .and_then(crate::model::ext_for_mime)
                            .or_else(|| crate::model::ext_for_mime(&page_mime));
                        if let Some(ext) = ext {
                            disp = Some(guess_name.trim_end_matches(".bin").to_string() + ext);
                        }
                    }
                    (total, disp)
                }
                Err(_) => (None, None),
            };
            let _ = tx.send(out);
        });
    }

    /// Queue the download. `start` = also wake the main app to begin it now
    /// (IDM "Start Download") vs merely queueing ("Download Later").
    pub(crate) fn commit(&mut self, start: bool) {
        // Download needs the main app, which owns the engine. When it is not
        // running, start it and queue nothing until it answers: a
        // queued-and-woken row used to sit silently and then start, with
        // every other one like it, the next time the app opened. `update`
        // commits once the app's heartbeat is live, or says it did not come.
        if start && !main_app_running() {
            if self.waiting_app.is_none() {
                self.waiting_app = Some(std::time::Instant::now());
                self.notice = crate::i18n::t(STARTING).to_string();
                std::thread::spawn(launch_main_app);
            }
            return;
        }
        self.waiting_app = None;
        if self.filename.trim().is_empty() {
            self.filename = filename_from_url(&self.url);
        }
        let mut item = DownloadItem::new(
            0, // id assigned atomically under the queue lock below
            self.url.trim().to_string(),
            engine::sanitize_filename(self.filename.trim()),
            self.effective_dir(),
            self.connections,
        );
        item.referer = self.referer.clone();
        item.cookie = self.cookie.clone();
        item.extra_headers = self.headers.clone();
        item.browser = self.browser.clone();
        item.description = self.description.trim().to_string();
        // Browser-measured size (extension HEAD/Range in the real session):
        // keep it so the engine and progress UI know the total even when a
        // server-side re-probe would 403 off-session.
        item.total_bytes = self.probe_size.filter(|s| *s > 0);
        if self.pair_url.starts_with("http") {
            item.pair_url = self.pair_url.clone();
            item.pair_mime = self.pair_mime.clone();
        }
        item.quality_pref = self.quality_pref.trim().to_string();
        item.duration_secs = self.duration_secs;
        item.expects_audio = self.expects_audio;
        // What the dialog already downloaded continues as this file. If it
        // finished meanwhile, the file is listed as finished.
        if self.pre.is_some() && !item.dest_path().exists() && !engine::has_partial(&item.dest_path()) {
            let dest = item.dest_path();
            // The progress window opens at what is already here, not at 0.
            item.downloaded_bytes = self.pre.as_ref().map_or(0, |p| p.shared.downloaded.load(std::sync::atomic::Ordering::SeqCst));
            if self.stop_prestart(Some(&dest)) {
                let len = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                item.total_bytes = Some(len);
                item.downloaded_bytes = len;
                let _ = crate::model::record_finished(&mut item);
                save_remember_category(self.remember_path);
                self.done = true;
                return;
            }
        } else {
            self.stop_prestart(None);
        }
        let id = match crate::model::append_to_queue(&mut item) {
            crate::model::AppendResult::Added(id) => {
                save_remember_category(self.remember_path);
                id
            }
            crate::model::AppendResult::AlreadyQueued(id) => {
                // Not an error. The URL is already in the list; if the user
                // asked to start it we wake that row, so this is never a dead
                // end. Stay open so the notice is actually read. The notice
                // names what really happens to the row.
                use crate::model::Status;
                self.notice = match crate::model::queued_status(id) {
                    Some(Status::Completed) => format!("Already downloaded (#{id}) — see Finished."),
                    Some(Status::Downloading) => format!("Already downloading (#{id})."),
                    // Download on a row that is waiting: start it and get out of
                    // the way, like any other Download (the progress window
                    // takes over). Only the cases with nothing to start stay
                    // open to explain themselves.
                    _ if start => {
                        self.wake_targets(&[id]);
                        self.done = true;
                        return;
                    }
                    _ => format!("Already in the queue (#{id})."),
                };
                return;
            }
            crate::model::AppendResult::Error(e) => {
                // Never close on a failed write: closing with a stale message
                // is exactly what made this look like "already in queue".
                self.notice = format!("Could not update the queue: {e}");
                return;
            }
        };
        save_remember_category(self.remember_path);
        if start {
            self.wake_targets(&[id]);
        }
        self.done = true;
    }

    pub(crate) fn wake_targets(&self, targets: &[u64]) {
        wake_ids(targets);
    }
}

/// Entry point for `--confirm-download`. Never returns normally
/// (runs the dialog window until it closes).
pub(crate) fn build_confirm_app() -> Option<(ConfirmApp, std::fs::File, std::sync::Arc<tokio::runtime::Runtime>)> {
    // Index-iterated flag parsing: a VALUE that looks like a flag (e.g. a
    // `Content-Disposition` filename of `--referer`) must not shift parsing.
    // Only the token immediately following a known flag is consumed.
    const KNOWN: &[&str] = &[
        "--url",
        "--filename",
        "--referer",
        "--cookie",
        "--mime",
        "--size",
        "--pair-url",
        "--pair-mime",
        "--quality",
        "--dur",
        "--capture-id",
    ];
    let args: Vec<String> = std::env::args().collect();
    let mut map: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(flag) = KNOWN.iter().find(|k| **k == args[i].as_str()) {
            if i + 1 < args.len() {
                map.insert(*flag, args[i + 1].clone());
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    let mut get = |name: &str| -> Option<String> { map.remove(name) };
    let url = get("--url").unwrap_or_default();
    if !is_download_url(&url) {
        eprintln!("--confirm-download needs --url <http(s)://...>");
        std::process::exit(2);
    }
    let capture_id = get("--capture-id").filter(|id| crate::native_host::valid_capture_id(id));
    // Every browser URL shares one OS-held slot. The host may launch multiple
    // short-lived processes after simultaneous retries, but only one dialog
    // for this URL can own the slot and reach the inbox claim below.
    let _confirm_slot = crate::model::claim_window(&crate::native_host::confirm_window_name(url.trim()));
    if _confirm_slot.is_none() {
        return None;
    }
    let mut page_mime = get("--mime").filter(|s| !s.trim().is_empty()).unwrap_or_default();
    // `get` consumes the flag, so read it once.
    let mut caller_name = get("--filename").filter(|f| !f.trim().is_empty());
    let mut referer = get("--referer").unwrap_or_default();
    let mut cookie = get("--cookie").unwrap_or_default();
    let mut pair_url = get("--pair-url").filter(|s| s.starts_with("http")).unwrap_or_default();
    let mut pair_mime = get("--pair-mime").unwrap_or_default();
    let mut quality_pref = get("--quality").filter(|s| !s.trim().is_empty()).unwrap_or_default();
    // Browser-measured size (menu HEAD/Range): trust it, skip probing.
    let mut known_size: Option<u64> = get("--size")
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        // An older extension sends a playlist's text length as its size.
        .filter(|_| !engine::is_stream_manifest(&url, None, &page_mime));
    // Browser-known media duration in seconds (best-effort badge).
    let mut duration_secs: u64 = get("--dur")
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0 && *s < 86400)
        .unwrap_or(0);

    // Claim the inbox entry first so the main window won't prompt too.
    let mut expects_audio = args.iter().any(|s| s == "--expect-audio");
    let mut headers = std::collections::BTreeMap::new();
    let mut browser = None;
    if let Some(item)=claim_inbox(url.trim(), capture_id.as_deref()) {
        caller_name = item.filename.clone().filter(|s| !s.trim().is_empty()).or(caller_name);
        if let Some(value) = item.referer.clone().filter(|s| !s.trim().is_empty()) { referer = value; }
        if let Some(value) = item.mime.clone().filter(|s| !s.trim().is_empty()) { page_mime = value; }
        if item.size.is_some() { known_size = item.size; }
        if let Some(value) = item.pair_url.clone().filter(|s| s.starts_with("http")) { pair_url = value; }
        if let Some(value) = item.pair_mime.clone() { pair_mime = value; }
        if let Some(value) = item.quality.clone() { quality_pref = value; }
        if let Some(value) = item.dur { duration_secs = value; }
        expects_audio |= item.audio_expected;
        browser = item.browser.clone();
        // The native host leaves credentials in the private inbox, not the
        // process command line where other local users can inspect them.
        if cookie.is_empty() {cookie=item.cookie.unwrap_or_default();}
        headers = item.headers.unwrap_or_default();
    }

    let named_by_caller = caller_name.is_some();
    let filename = caller_name.unwrap_or_else(|| {
        crate::model::filename_from_url_with_mime(&url, Some(page_mime.as_str()))
    });

    let prefs = load_prefs();
    let category = category_of(&filename).to_string();
    // A row that is still usable (queued/downloading/paused, or finished with
    // its file present) is reported up front. A dead row (failed, or finished
    // with the file gone) is refreshed by the capture instead, so the buttons
    // never dead-end on a stale "already in queue".
    let initial_notice = match crate::model::queued_entry(url.trim(), &quality_pref) {
        Some((id, false)) => format!("Already in the queue (#{id})."),
        _ => String::new(),
    };

    let rt = std::sync::Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime"),
    );
    let start_name = crate::model::apply_rename_template(&prefs.rename_template, &engine::sanitize_filename(&filename), url.trim());
    let mut app = ConfirmApp {
        auto_name: (!named_by_caller).then(|| start_name.clone()),
        browser_size: known_size,
        url: url.trim().to_string(),
        filename: start_name,
        category,
        remember_path: prefs.category_folders,
        save_base: prefs.save_dir,
        description: String::new(),
        referer,
        cookie,
        headers,
        browser,
        pair_url,
        pair_mime,
        page_mime,
        duration_secs,
        expects_audio,
        quality_pref,
        connections: prefs.connections,
        user_agent: prefs.user_agent,
        proxy_url: prefs.proxy_url,
        timeout_secs: prefs.timeout_secs,
        rename_template: prefs.rename_template,
        folder_rules: prefs.folder_rules,
        pre: None,
        pre_tried: false,
        probing: false,
        probe_size: known_size,
        probe_rx: None,
        rt: Some(rt.handle().clone()),
        centered: false,
        show_adv: false,
        done: false,
        waiting_app: None,
        exited: false,
        notice: initial_notice,
        cat_open: false,
        recent_open: false,
        win_h: 0.0,
        adding_cat: false,
        new_cat: String::new(),
        custom_cats: Vec::new(),
    };
    // Prefill filename from server when the extension sent none useful.
    // A browser-known size skips probing entirely (server re-probes of
    // media URLs often 403 off-session).
    if known_size.is_none() {
        app.start_probe();
    }
    Some((app, _confirm_slot.unwrap(), rt))
}
