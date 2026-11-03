//! First GPUI migration slice for the main download queue.
//!
//! The desktop shell and companion views use GPUI. Transfer work and queue
//! files stay in the existing Rust model/engine; GPUI presents and controls it.

use crate::engine::{self, EngineOpts, SharedState};
use crate::model::{self, AppendResult, DownloadItem, Status};
use gpui::{
    div, prelude::*, rgb, size, px, App, Application, Bounds, Context, Entity, FocusHandle, KeyDownEvent, Render,
    AnyWindowHandle, SharedString, Timer, Window, WindowBounds, WindowHandle, WindowOptions, uniform_list,
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{atomic::Ordering, Arc, Mutex},
    time::{Duration, Instant},
};

struct ActiveDownload {
    shared: Arc<SharedState>,
    task: tokio::task::JoinHandle<()>,
}

fn capture_identity(item: &crate::native_host::InboxItem) -> String {
    match item.capture_id.as_deref() {
        Some(id) => format!("capture:{id}"),
        None => format!("url:{}\nreceived:{}", item.url, item.received_at),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QueueFilter {
    All,
    Active,
    Queued,
    Completed,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QueueSort {
    Added,
    Name,
    Size,
    Status,
}

impl QueueSort {
    const ALL: [Self; 4] = [Self::Added, Self::Name, Self::Size, Self::Status];

    fn label(self) -> &'static str {
        match self {
            Self::Added => "Date added",
            Self::Name => "Name",
            Self::Size => "Size",
            Self::Status => "Status",
        }
    }

    fn next(self) -> Self {
        let index = Self::ALL.iter().position(|candidate| *candidate == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }
}

impl QueueFilter {
    const ALL: [Self; 5] = [Self::All, Self::Active, Self::Queued, Self::Completed, Self::Failed];

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Active => "Downloading",
            Self::Queued => "Queued",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
        }
    }

    fn matches(self, status: &Status) -> bool {
        match self {
            Self::All => true,
            Self::Active => matches!(status, Status::Downloading),
            Self::Queued => matches!(status, Status::Queued | Status::Paused),
            Self::Completed => matches!(status, Status::Completed),
            Self::Failed => matches!(status, Status::Failed(_)),
        }
    }
}

pub(crate) struct DownloadManagerStore {
    runtime: tokio::runtime::Handle,
    updates: crate::update_service::UpdateService,
    speed_limit_live: Arc<std::sync::atomic::AtomicU64>,
    items: Vec<DownloadItem>,
    tasks: HashMap<u64, ActiveDownload>,
    removed_ids: HashSet<u64>,
    queue_running: bool,
    max_concurrent: usize,
    filter: QueueFilter,
    sort: QueueSort,
    search_query: String,
    pending_capture: Option<crate::native_host::InboxItem>,
    window_handles: HashMap<String, AnyWindowHandle>,
    message: String,
    last_progress_persist: Instant,
    grabber_open: bool,
    grab_url: String,
    grab_exts: String,
    grab_depth: usize,
    grab_stay_under: bool,
    grab_busy: bool,
    grab_results: Vec<crate::grabber::GrabLink>,
    grab_rx: Option<std::sync::mpsc::Receiver<Result<Vec<crate::grabber::GrabLink>, String>>>,
    grab_notice: String,
    site_rules_open: bool,
    site_rule_draft: String,
    site_rule_notice: String,
    shutdown_prepared: bool,
}

impl DownloadManagerStore {
    fn load(runtime: Arc<tokio::runtime::Runtime>) -> Self {
        let speed_limit_live = Arc::new(std::sync::atomic::AtomicU64::new(
            model::ui_pref_u64("speed_limit_kbps", 0).saturating_mul(1024),
        ));
        let mut manager = Self {
            runtime: runtime.handle().clone(),
            updates: crate::update_service::UpdateService::new(runtime.handle().clone()),
            speed_limit_live,
            items: Vec::new(),
            tasks: HashMap::new(),
            removed_ids: HashSet::new(),
            queue_running: false,
            max_concurrent: 3,
            filter: QueueFilter::All,
            sort: QueueSort::Added,
            search_query: String::new(),
            pending_capture: None,
            window_handles: HashMap::new(),
            message: "Download Craft is ready".into(),
            last_progress_persist: Instant::now(),
            grabber_open: false,
            grab_url: String::new(),
            grab_exts: crate::grabber::preset_exts("Mixed"),
            grab_depth: 1,
            grab_stay_under: true,
            grab_busy: false,
            grab_results: Vec::new(),
            grab_rx: None,
            grab_notice: String::new(),
            site_rules_open: false,
            site_rule_draft: String::new(),
            site_rule_notice: String::new(),
            shutdown_prepared: false,
        };
        manager.reload();
        if model::ui_pref_bool("update_check_on", true) {
            let _ = manager.updates.request("check_update");
        }
        manager
    }

    fn queue_path() -> Option<PathBuf> {
        model::config_dir().map(|dir| dir.join("queue.json"))
    }

    fn reload(&mut self) -> bool {
        let Some(path) = Self::queue_path() else { return false };
        match model::read_queue_items(&path) {
            Ok(disk) => {
                let mut changed = false;
                for mut item in disk {
                    if self.removed_ids.contains(&item.id) {
                        continue;
                    }
                    let disk_was_active = item.status.is_active();
                    if disk_was_active {
                        item.status = Status::Queued;
                    }
                    item.speed_bps = 0.0;
                    item.selected = false;
                    if let Some(position) = self.items.iter().position(|existing| existing.id == item.id) {
                        let local = &self.items[position];
                        let may_be_refreshed = !disk_was_active
                            && !self.tasks.contains_key(&item.id)
                            && match &local.status {
                                Status::Failed(_) => true,
                                Status::Completed => !local.dest_path().exists(),
                                Status::Paused => true,
                                Status::Queued => true,
                                Status::Downloading => false,
                            };
                        let disk_changed = local.url != item.url
                            || local.filename != item.filename
                            || local.status != item.status
                            || local.total_bytes != item.total_bytes
                            || local.downloaded_bytes != item.downloaded_bytes
                            || local.referer != item.referer
                            || local.cookie != item.cookie
                            || local.extra_headers != item.extra_headers
                            || local.username != item.username
                            || local.password != item.password
                            || local.pair_url != item.pair_url
                            || local.quality_pref != item.quality_pref;
                        if may_be_refreshed && disk_changed {
                            self.items[position] = item;
                            changed = true;
                        }
                        continue;
                    }
                    self.items.push(item);
                    changed = true;
                }
                if changed {
                    self.items.sort_by_key(|item| item.created_at);
                }
                changed
            }
            Err(error) => {
                let message = format!("Could not read queue: {error}");
                let changed = self.message != message;
                self.message = message;
                changed
            }
        }
    }

    fn persist(&mut self) {
        let _ = self.persist_with_lock_timeout(Duration::from_secs(2));
    }

    fn persist_with_lock_timeout(&mut self, timeout: Duration) -> bool {
        let Some(path) = Self::queue_path() else { return false };
        let guard = model::fs_lock_timeout("queue", timeout);
        if !guard.owned() {
            self.message = "Queue is busy; retry shortly".into();
            return false;
        }

        // Keep additions made by the extension or companion processes since
        // this window last refreshed. IDs explicitly removed here stay gone.
        let disk = match model::read_queue_items(&path) {
            Ok(items) => items,
            Err(error) => {
                self.message = format!("Could not read queue; refusing to overwrite it: {error}");
                return false;
            }
        };
        for item in disk {
            if !self.removed_ids.contains(&item.id)
                && !self.items.iter().any(|current| current.id == item.id)
            {
                self.items.push(item);
            }
        }

        let mut saved = self.items.clone();
        for item in &mut saved {
            item.speed_bps = 0.0;
            item.selected = false;
            if matches!(item.status, Status::Completed) {
                item.cookie.clear();
                item.extra_headers.clear();
            }
        }
        match serde_json::to_vec_pretty(&saved)
            .map_err(|error| error.to_string())
            .and_then(|bytes| model::atomic_write(&path, &bytes).map_err(|error| error.to_string()))
        {
            Ok(()) => true,
            Err(error) => {
                self.message = format!("Could not save queue: {error}");
                false
            }
        }
    }

    fn prepare_shutdown(&mut self) {
        if self.shutdown_prepared {
            return;
        }
        model::set_app_lifecycle("closing");
        for active in self.tasks.values() {
            active.shared.cancel();
        }
        for item in &mut self.items {
            if let Some(active) = self.tasks.get(&item.id) {
                item.downloaded_bytes = active.shared.downloaded.load(Ordering::SeqCst);
                if active.shared.has_total.load(Ordering::SeqCst) {
                    item.total_bytes = Some(active.shared.total.load(Ordering::SeqCst));
                }
                item.speed_bps = 0.0;
                if matches!(item.status, Status::Downloading) {
                    item.status = Status::Paused;
                }
            }
        }
        self.shutdown_prepared = self.persist_with_lock_timeout(Duration::from_millis(200));
    }

    fn add_url(&mut self, raw_url: &str) -> bool {
        let (url, mirrors) = model::split_mirrors(raw_url.trim());
        if !model::is_download_url(&url) {
            self.message = "Copy an http(s) download link first".into();
            return false;
        }
        let default_dir = dirs::download_dir()
            .unwrap_or_else(model::default_download_dir)
            .to_string_lossy()
            .into_owned();
        let save_dir = model::ui_pref_str("save_dir", &default_dir);
        let filename = model::filename_from_url(&url);
        let connections = model::ui_pref_u64("connections_default", 8).clamp(1, 64) as usize;
        let mut item = DownloadItem::new(0, url, filename, save_dir, connections);
        item.mirrors = mirrors;
        match model::append_to_queue(&mut item) {
            AppendResult::Added(id) => {
                item.id = id;
                self.items.retain(|existing| existing.id != id);
                self.items.push(item);
                self.items.sort_by_key(|row| row.created_at);
                self.message = format!("Queued #{id}");
                true
            }
            AppendResult::AlreadyQueued(id) => {
                self.message = format!("That link is already queued as #{id}");
                self.reload();
                false
            }
            AppendResult::Error(error) => {
                self.message = format!("Could not queue download: {error}");
                false
            }
        }
    }

    fn start_grab(&mut self) {
        if self.grab_busy {
            return;
        }
        let url = self.grab_url.trim().to_string();
        if !model::is_download_url(&url) {
            self.grab_notice = "Enter an http(s) page URL first.".into();
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.grab_rx = Some(rx);
        self.grab_busy = true;
        self.grab_results.clear();
        self.grab_notice = "Scanning this page and matching links…".into();
        let exts = crate::grabber::parse_ext_list(&self.grab_exts);
        let depth = self.grab_depth.clamp(1, 5);
        let stay_under = self.grab_stay_under;
        let configured_ua = model::ui_pref_str("user_agent", "");
        let (username, password) = model::login_for(&url).unwrap_or_default();
        let opts = EngineOpts {
            user_agent: if configured_ua.trim().is_empty() {
                format!("download_craft/{}", model::APP_VERSION)
            } else {
                configured_ua
            },
            timeout_secs: model::ui_pref_u64("timeout_secs", 60),
            proxy_url: model::ui_pref_str("proxy_url", ""),
            username,
            password,
            ..Default::default()
        };
        self.runtime.spawn(async move {
            let result = match opts.build_client() {
                Ok(client) => crate::grabber::grab_site(
                    &client,
                    &opts,
                    &url,
                    &exts,
                    depth,
                    stay_under,
                    crate::grabber::MAX_PAGES_DEFAULT * (depth * 2),
                )
                .await
                .map_err(|error| error.to_string()),
                Err(error) => Err(format!("{error:#}")),
            };
            let _ = tx.send(result);
        });
    }

    fn queue_grab_results(&mut self) {
        let results = std::mem::take(&mut self.grab_results);
        let mut added = 0usize;
        for link in results {
            if self.add_url(&link.url) {
                if let Some(item) = self.items.iter_mut().find(|item| item.url == link.url) {
                    item.filename = model::apply_rename_template(&model::ui_pref_str("rename_template", "{name}"), &link.name, &link.url);
                }
                added += 1;
            }
        }
        if added > 0 {
            self.persist();
            if self.queue_running { self.fill_queue(); }
        }
        self.grab_notice = format!("Queued {added} new link(s). Duplicate links were skipped.");
    }

    fn start(&mut self, id: u64) {
        if self.tasks.contains_key(&id) {
            return;
        }
        let Some(item) = self.items.iter().find(|item| item.id == id).cloned() else {
            return;
        };
        let shared = Arc::new(SharedState::new());
        shared.downloaded.store(item.downloaded_bytes, Ordering::SeqCst);
        if let Some(total) = item.total_bytes {
            shared.total.store(total, Ordering::SeqCst);
            shared.has_total.store(true, Ordering::SeqCst);
        }
        let options = EngineOpts {
            timeout_secs: item.eff_timeout(model::ui_pref_u64("timeout_secs", 60)),
            max_retries: item.eff_retries(model::ui_pref_u64("max_retries", 5) as u32),
            speed_limit_bps: 0,
            global_limit: Some(self.speed_limit_live.clone()),
            user_agent: {
                let configured = model::ui_pref_str("user_agent", "");
                if configured.trim().is_empty() {
                    format!("download_craft/{}", model::APP_VERSION)
                } else {
                    configured
                }
            },
            referer: item.referer.clone(),
            cookie: item.cookie.clone(),
            cookie_scope: engine::CredentialScope::for_url(&item.url),
            captured_headers: engine::CapturedHeaders::for_url(&item.url, &item.extra_headers),
            username: item.username.clone(),
            password: item.password.clone(),
            proxy_url: model::ui_pref_str("proxy_url", ""),
            per_host_conns: model::ui_pref_u64("per_host_conns", 0) as usize,
            expected_sha256: item.checksum_sha256.clone(),
            expected_duration_secs: item.duration_secs,
            require_audio: item.expects_audio || !item.pair_url.is_empty(),
            overwrite_existing: item.overwrite_existing,
            quality_pref: item.quality_pref.clone(),
            browser: item.browser.clone(),
            mirrors: item.mirrors.clone(),
            log_key: item.id.to_string(),
            ..EngineOpts::default()
        };
        let url = item.url.clone();
        let destination = item.dest_path();
        let pair_url = item.pair_url.clone();
        let filename = item.filename.clone();
        let connections = item.connections;
        let shared_task = shared.clone();
        let task = self.runtime.spawn(async move {
            let result = if pair_url.starts_with("http") {
                engine::download_adaptive(
                    url,
                    Some(pair_url),
                    destination,
                    connections,
                    options,
                    shared_task.clone(),
                )
                .await
            } else {
                engine::download(
                    url,
                    destination,
                    connections,
                    options,
                    shared_task.clone(),
                )
                .await
            };
            match result {
                Ok(()) => shared_task.succeeded.store(true, Ordering::SeqCst),
                Err(error) => shared_task.set_error(format!("{error:#}")),
            }
        });
        if let Some(row) = self.items.iter_mut().find(|row| row.id == id) {
            row.status = Status::Downloading;
            row.speed_bps = 0.0;
        }
        self.tasks.insert(id, ActiveDownload { shared, task });
        self.message = format!("Started {filename}");
        self.persist();
    }

    fn start_queue(&mut self) {
        self.queue_running = true;
        self.fill_queue();
    }

    fn pause_queue(&mut self) {
        self.queue_running = false;
        let ids = self
            .tasks
            .keys()
            .copied()
            .filter(|id| {
                self.tasks
                    .get(id)
                    .is_some_and(|task| !task.shared.paused.load(Ordering::SeqCst))
            })
            .collect::<Vec<_>>();
        for id in ids {
            self.toggle(id);
        }
        self.message = "Queue paused".into();
    }

    fn import_links_file(&mut self) {
        let path = rfd::FileDialog::new()
            .add_filter("URL list", &["txt", "url"])
            .set_title("Import download links")
            .pick_file();
        let Some(path) = path else { return };
        let urls = crate::basket_service::urls_from_dropped(&path);
        if urls.is_empty() {
            self.message = "No http(s) links found in that file".into();
            return;
        }
        let mut count = 0;
        for url in urls {
            count += usize::from(self.add_url(&url));
        }
        self.message = if count == 0 {
            "No new links to import".into()
        } else {
            format!("Imported {count} new link(s)")
        };
    }

    fn fill_queue(&mut self) {
        if !self.queue_running {
            return;
        }
        while self.tasks.len() < self.max_concurrent {
            let next = self
                .items
                .iter()
                .find(|item| matches!(item.status, Status::Queued) && !self.tasks.contains_key(&item.id))
                .map(|item| item.id);
            let Some(id) = next else { break };
            self.start(id);
        }
    }

    fn toggle(&mut self, id: u64) {
        if let Some(active) = self.tasks.get(&id) {
            if active.shared.external_helper.load(Ordering::SeqCst) {
                active.shared.cancel();
                if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
                    item.status = Status::Paused;
                }
                self.message = format!("Paused #{id}; resume will continue from its partial file");
                self.persist();
                return;
            }
            let paused = !active.shared.paused.load(Ordering::SeqCst);
            active.shared.paused.store(paused, Ordering::SeqCst);
            if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
                item.status = if paused { Status::Paused } else { Status::Downloading };
            }
            self.message = if paused { format!("Paused #{id}") } else { format!("Resumed #{id}") };
            self.persist();
        } else {
            self.start(id);
        }
    }

    fn remove(&mut self, id: u64) {
        if let Some(active) = self.tasks.get(&id) {
            active.shared.cancel();
        }
        self.items.retain(|item| item.id != id);
        self.removed_ids.insert(id);
        self.message = format!("Removed #{id}");
        self.persist();
    }

    fn toggle_selection(&mut self, id: u64) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.selected = !item.selected;
        }
    }

    fn set_selected(&mut self, ids: &[u64], selected: bool) {
        let ids = ids.iter().copied().collect::<HashSet<_>>();
        for item in &mut self.items {
            if ids.contains(&item.id) {
                item.selected = selected;
            }
        }
    }

    fn selected_ids(&self) -> Vec<u64> {
        self.items.iter().filter(|item| item.selected).map(|item| item.id).collect()
    }

    fn pause_selected(&mut self) {
        let ids = self.selected_ids();
        for id in &ids {
            if let Some(active) = self.tasks.get(id) {
                if active.shared.external_helper.load(Ordering::SeqCst) {
                    active.shared.cancel();
                } else {
                    active.shared.paused.store(true, Ordering::SeqCst);
                }
            }
            if let Some(item) = self.items.iter_mut().find(|item| item.id == *id) {
                if matches!(item.status, Status::Downloading | Status::Queued) {
                    item.status = Status::Paused;
                }
            }
        }
        if !ids.is_empty() {
            self.message = format!("Paused {} selected download(s)", ids.len());
            self.persist();
        }
    }

    fn resume_selected(&mut self) {
        let ids = self.selected_ids();
        let mut queued = false;
        for id in &ids {
            if let Some(active) = self.tasks.get(id) {
                if active.shared.paused.load(Ordering::SeqCst) {
                    active.shared.paused.store(false, Ordering::SeqCst);
                    if let Some(item) = self.items.iter_mut().find(|item| item.id == *id) {
                        item.status = Status::Downloading;
                    }
                }
            } else if let Some(item) = self.items.iter_mut().find(|item| item.id == *id) {
                if matches!(item.status, Status::Paused | Status::Queued | Status::Failed(_)) {
                    item.status = Status::Queued;
                    queued = true;
                }
            }
        }
        if queued {
            self.queue_running = true;
            self.fill_queue();
        }
        if !ids.is_empty() {
            self.message = format!("Resumed {} selected download(s)", ids.len());
            self.persist();
        }
    }

    fn remove_selected(&mut self) {
        let ids = self.selected_ids();
        if ids.is_empty() {
            return;
        }
        let id_set = ids.iter().copied().collect::<HashSet<_>>();
        for id in &ids {
            if let Some(active) = self.tasks.get(id) {
                active.shared.cancel();
            }
            self.removed_ids.insert(*id);
        }
        self.items.retain(|item| !id_set.contains(&item.id));
        self.message = format!("Removed {} selected download(s)", ids.len());
        self.persist();
    }

    fn refresh_pending_capture(&mut self) -> bool {
        let mut pending = match crate::native_host::pending_captures() {
            Ok(items) => items,
            Err(error) => {
                if self.pending_capture.is_none() {
                    let message = format!("Browser inbox unavailable: {error}");
                    let changed = self.message != message;
                    self.message = message;
                    return changed;
                }
                return false;
            }
        };
        pending.sort_by(|left, right| left.received_at.cmp(&right.received_at));
        let previous = self.pending_capture.as_ref().map(capture_identity);
        let next = previous
            .as_ref()
            .and_then(|key| pending.iter().find(|item| capture_identity(item).as_str() == key.as_str()).cloned())
            .or_else(|| pending.into_iter().next());
        let changed = match (&self.pending_capture, &next) {
            (Some(left), Some(right)) => {
                capture_identity(left) != capture_identity(right)
                    || left.filename != right.filename
                    || left.size != right.size
                    || left.referer != right.referer
                    || left.cookie != right.cookie
                    || left.headers != right.headers
            }
            (None, None) => false,
            _ => true,
        };
        self.pending_capture = next;
        changed
    }

    fn pause_one(&mut self, id: u64) {
        if let Some(active) = self.tasks.get(&id) {
            if !active.shared.paused.load(Ordering::SeqCst) {
                self.toggle(id);
            }
        } else if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            if matches!(item.status, Status::Queued) {
                item.status = Status::Paused;
                self.message = format!("Paused #{id}");
                self.persist();
            }
        }
    }

    fn resume_one(&mut self, id: u64) {
        if let Some(active) = self.tasks.get(&id) {
            if active.shared.paused.load(Ordering::SeqCst) {
                self.toggle(id);
            }
            return;
        }
        let can_resume = self.items.iter().any(|item| {
            item.id == id && matches!(item.status, Status::Paused | Status::Queued | Status::Failed(_))
        });
        if can_resume {
            if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
                item.status = Status::Queued;
            }
            self.queue_running = true;
            self.start(id);
        }
    }

    fn cancel_one(&mut self, id: u64) {
        if let Some(active) = self.tasks.get(&id) {
            active.shared.cancel();
            if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
                item.status = Status::Paused;
            }
            self.message = format!("Stopped #{id}; the partial file is kept for resume");
            self.persist();
        }
    }

    fn process_commands(&mut self) {
        let Some(path) = model::config_dir().map(|dir| dir.join("cmd.json")) else {
            return;
        };
        let guard = model::fs_lock("cmd");
        if !guard.owned() {
            return;
        }
        let Ok(data) = std::fs::read_to_string(&path) else {
            return;
        };
        // Claim before applying, matching the previous desktop command loop's
        // at-most-once behavior across the two frontends.
        let _ = std::fs::remove_file(&path);
        let Ok(commands) = serde_json::from_str::<Vec<serde_json::Value>>(&data) else {
            return;
        };
        for command in commands {
            let action = command.get("action").and_then(|value| value.as_str()).unwrap_or("");
            let id = command.get("id").and_then(|value| value.as_u64());
            match action {
                "pause" => { if let Some(id) = id { self.pause_one(id); } }
                "resume" => { if let Some(id) = id { self.resume_one(id); } }
                "cancel" => { if let Some(id) = id { self.cancel_one(id); } }
                "pause_all" => self.pause_queue(),
                "resume_all" => self.start_queue(),
                "limit" => {
                    if let Some(limit) = command.get("kbps").and_then(|value| value.as_u64()) {
                        model::ui_pref_set_u64("speed_limit_kbps", limit);
                        self.speed_limit_live.store(limit.saturating_mul(1024), Ordering::SeqCst);
                        self.message = format!("Speed limit set to {limit} KB/s");
                    }
                }
                "finish" => {
                    if let Some(value) = command.get("value").and_then(|value| value.as_str()) {
                        if ["nothing", "shutdown", "sleep", "hibernate"].contains(&value) {
                            model::ui_pref_set_str("finish_action", value);
                            self.message = format!("Queue finish action set to {value}");
                        }
                    }
                }
                "check_update" | "self_update" | "restart_update" | "restart_when_idle" => {
                    let events = self.updates.request(action);
                    self.apply_update_events(events);
                }
                _ => {}
            }
        }
    }

    fn apply_update_events(&mut self, events: Vec<crate::update_service::UpdateEvent>) -> bool {
        let mut changed = false;
        for event in events {
            match event {
                crate::update_service::UpdateEvent::QueueDownload { url, name } => {
                    if self.add_url(&url) {
                        let id = self.items.iter_mut().find(|item| item.url == url).map(|item| {
                            item.filename = name;
                            item.id
                        });
                        if let Some(id) = id {
                            if self.tasks.len() < self.max_concurrent {
                                self.start(id);
                            } else {
                                self.queue_running = true;
                            }
                        }
                        self.persist();
                    }
                    changed = true;
                }
                crate::update_service::UpdateEvent::RestartRequested => {
                    self.prepare_shutdown();
                    if !self.shutdown_prepared {
                        model::set_app_lifecycle("running");
                        self.message = "Update is ready, but the queue could not be saved. Retry the restart after saving succeeds.".into();
                        changed = true;
                        continue;
                    }
                    match crate::selfupdate::relaunch() {
                        Ok(()) => crate::tray::request_quit(),
                        Err(error) => {
                            self.shutdown_prepared = false;
                            model::set_app_lifecycle("running");
                            self.message = format!("Update installed, but restart failed: {error:#}");
                        }
                    }
                    changed = true;
                }
                crate::update_service::UpdateEvent::Changed => changed = true,
            }
        }
        changed
    }

    fn refresh(&mut self) -> bool {
        let old_message = self.message.clone();
        let old_filter = self.filter;
        let mut changed = false;
        if let Some(result) = self.grab_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.grab_rx = None;
            self.grab_busy = false;
            match result {
                Ok(results) => {
                    let count = results.len();
                    self.grab_results = results;
                    self.grab_notice = format!("Found {count} matching link(s).");
                }
                Err(error) => self.grab_notice = format!("Grab failed: {error}"),
            }
            changed = true;
        }
        self.process_commands();
        let update_events = self.updates.poll(self.tasks.len());
        changed |= self.apply_update_events(update_events);
        changed |= old_message != self.message || old_filter != self.filter;
        self.speed_limit_live.store(
            model::ui_pref_u64("speed_limit_kbps", 0).saturating_mul(1024),
            Ordering::SeqCst,
        );
        let mut finished = Vec::new();
        for (id, active) in &self.tasks {
            if let Some(item) = self.items.iter_mut().find(|item| item.id == *id) {
                let downloaded = active.shared.downloaded.load(Ordering::SeqCst);
                if item.downloaded_bytes != downloaded {
                    item.downloaded_bytes = downloaded;
                    changed = true;
                }
                if active.shared.has_total.load(Ordering::SeqCst) {
                    let total = Some(active.shared.total.load(Ordering::SeqCst));
                    if item.total_bytes != total {
                        item.total_bytes = total;
                        changed = true;
                    }
                }
                let speed = active.shared.speed_bps.load(Ordering::SeqCst) as f64;
                if item.speed_bps != speed {
                    item.speed_bps = speed;
                    changed = true;
                }
            }
            if active.task.is_finished() {
                finished.push(*id);
            }
        }
        let has_finished = !finished.is_empty();
        let progress_checkpoint_due = self.last_progress_persist.elapsed() >= Duration::from_secs(3);
        for id in finished {
            if let Some(active) = self.tasks.remove(&id) {
                if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
                    let old_status = item.status.clone();
                    if active.shared.succeeded.load(Ordering::SeqCst) {
                        item.status = Status::Completed;
                        item.downloaded_bytes = std::fs::metadata(item.dest_path())
                            .map(|metadata| metadata.len())
                            .unwrap_or(item.downloaded_bytes);
                        item.total_bytes = Some(item.downloaded_bytes);
                        item.cookie.clear();
                        item.extra_headers.clear();
                        self.message = format!("Completed {}", item.filename);
                    } else if active.shared.cancelled.load(Ordering::SeqCst) {
                        item.status = Status::Paused;
                    } else {
                        let error = active
                            .shared
                            .error
                            .lock()
                            .map(|value| value.clone().unwrap_or_else(|| "Download failed".into()))
                            .unwrap_or_else(|error| error.into_inner().clone().unwrap_or_else(|| "Download failed".into()));
                        item.status = Status::Failed(error.clone());
                        self.message = format!("Failed {}: {error}", item.filename);
                    }
                    changed |= item.status != old_status;
                }
            }
        }
        changed |= has_finished;
        changed |= self.reload();
        let start_ids = crate::capture_service::take_wake_ids();
        if !start_ids.is_empty() {
            // A confirmation's "Start Download" signal is persisted separately
            // from the queue row. Consume it only after reloading, then enter
            // queue-running mode so freed worker slots keep advancing the batch.
            self.queue_running = true;
            for id in start_ids {
                if self.tasks.len() >= self.max_concurrent {
                    break;
                }
                if self.items.iter().any(|item| item.id == id && matches!(item.status, Status::Queued)) {
                    self.start(id);
                    changed = true;
                }
            }
        }
        changed |= self.refresh_pending_capture();
        if has_finished || progress_checkpoint_due {
            self.persist();
            self.last_progress_persist = Instant::now();
        }
        self.fill_queue();
        changed
    }
}

impl Drop for DownloadManagerStore {
    fn drop(&mut self) {
        self.prepare_shutdown();
        let deadline = Instant::now() + Duration::from_millis(200);
        while self.tasks.values().any(|active| !active.task.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

struct MainView {
    manager: Entity<DownloadManagerStore>,
    grab_focus: FocusHandle,
    site_rule_focus: FocusHandle,
    search_focus: FocusHandle,
    capture_focus: FocusHandle,
    capture_window: Option<WindowHandle<crate::gpui_confirm::GpuiConfirmView>>,
    capture_window_key: Option<String>,
    last_window_prune: Instant,
    last_links_scan: Instant,
    #[cfg(windows)]
    _tray_icon: Option<tray_icon::TrayIcon>,
}

fn normalize_block_host(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty()
        || raw.chars().any(|ch| matches!(ch, '/' | '\\' | ':' | '@' | '?' | '#'))
        || raw.chars().any(char::is_whitespace)
    {
        return None;
    }
    let parsed = url::Url::parse(&format!("https://{raw}/")).ok()?;
    if parsed.port().is_some() || parsed.username() != "" || parsed.password().is_some() || parsed.path() != "/" {
        return None;
    }
    let host = parsed.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty() && host.len() <= 253).then_some(host)
}

fn configured_block_hosts() -> Vec<String> {
    let mut seen = HashSet::new();
    model::ui_pref_str("capture_skip_sites", "")
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == ';')
        .filter_map(normalize_block_host)
        .filter(|host| seen.insert(host.clone()))
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsPane {
    Appearance,
    Downloads,
    Plugins,
    Browser,
    Proxy,
    Categories,
    Sounds,
    Updates,
}

impl SettingsPane {
    const ALL: [Self; 8] = [Self::Appearance, Self::Downloads, Self::Plugins, Self::Browser, Self::Proxy, Self::Categories, Self::Sounds, Self::Updates];

    fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Downloads => "Downloads",
            Self::Plugins => "Plugins",
            Self::Browser => "Browser",
            Self::Proxy => "Proxy",
            Self::Categories => "Categories",
            Self::Sounds => "Sounds",
            Self::Updates => "Updates",
        }
    }
}

struct SettingsView {
    pane: Arc<Mutex<SettingsPane>>,
    proxy_focus: FocusHandle,
    proxy_draft: Arc<Mutex<String>>,
    proxy_notice: Arc<Mutex<String>>,
}

struct SetupData {
    states: Vec<(crate::setup_service::Part, crate::setup_service::State)>,
    tx: std::sync::mpsc::Sender<crate::setup_service::Msg>,
    rx: std::sync::mpsc::Receiver<crate::setup_service::Msg>,
    busy: bool,
    show_at_start: bool,
    auto_fix_pending: bool,
    last_extension_check: Instant,
}

struct SetupView {
    data: Arc<Mutex<SetupData>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProgressTab {
    Info,
    Speed,
    Settings,
    Log,
}

impl ProgressTab {
    const ALL: [Self; 4] = [Self::Info, Self::Speed, Self::Settings, Self::Log];

    fn label(self) -> &'static str {
        match self {
            Self::Info => "Info",
            Self::Speed => "Speed",
            Self::Settings => "Settings",
            Self::Log => "Log",
        }
    }
}

struct ProgressView {
    id: u64,
    snapshot: crate::progress_service::GpuiProgressSnapshot,
    tab: Arc<Mutex<ProgressTab>>,
    last_log_read: Instant,
}

struct CompleteView {
    item: DownloadItem,
    dont_show: Arc<Mutex<bool>>,
}

impl SettingsView {
    fn toggle(window: &mut Window, key: &'static str, default: bool) {
        model::ui_pref_set_bool(key, !model::ui_pref_bool(key, default));
        window.refresh();
    }
}

impl SetupView {
    fn start_fix(data: &Arc<Mutex<SetupData>>, parts: Vec<crate::setup_service::Part>) {
        let tx = {
            let mut state = data.lock().unwrap_or_else(|error| error.into_inner());
            if state.busy || parts.is_empty() {
                return;
            }
            state.busy = true;
            state.tx.clone()
        };
        crate::setup_service::run_fixes(tx, parts);
    }

    fn poll(&mut self) {
        let (tx, fix_parts, check_extension) = {
            let mut data = self.data.lock().unwrap_or_else(|error| error.into_inner());
            while let Ok(message) = data.rx.try_recv() {
                match message {
                    crate::setup_service::Msg::State(part, value) => {
                        if let Some(state) = data.states.iter_mut().find(|(current, _)| *current == part) {
                            state.1 = value;
                        }
                    }
                    crate::setup_service::Msg::Idle => data.busy = false,
                }
            }

            let fix_parts = if !data.busy && data.auto_fix_pending {
                data.auto_fix_pending = false;
                let missing = data.states.iter()
                    .filter(|(part, state)| part.automatic() && matches!(state, crate::setup_service::State::Missing(_) | crate::setup_service::State::Failed(_)))
                    .map(|(part, _)| *part)
                    .collect::<Vec<_>>();
                if !missing.is_empty() {
                    data.busy = true;
                    Some(missing)
                } else {
                    None
                }
            } else {
                None
            };

            let check_extension = !data.busy
                && data.last_extension_check.elapsed() >= Duration::from_secs(2)
                && !data.states.iter().find(|(part, _)| *part == crate::setup_service::Part::Extension).is_some_and(|(_, state)| state.ready());
            if check_extension {
                data.last_extension_check = Instant::now();
                data.busy = true;
            }
            let tx = if fix_parts.is_some() || check_extension { Some(data.tx.clone()) } else { None };
            (tx, fix_parts, check_extension)
        };

        if let Some(parts) = fix_parts {
            if let Some(tx) = tx { crate::setup_service::run_fixes(tx, parts); }
        } else if check_extension {
            if let Some(tx) = tx { crate::setup_service::run_checks(tx, vec![crate::setup_service::Part::Extension]); }
        }
    }

    fn missing_automatic(data: &SetupData) -> Vec<crate::setup_service::Part> {
        data.states.iter()
            .filter(|(part, state)| part.automatic() && matches!(state, crate::setup_service::State::Missing(_) | crate::setup_service::State::Failed(_)))
            .map(|(part, _)| *part)
            .collect()
    }
}

struct AboutView {
    snapshot: crate::about_service::GpuiSnapshot,
    helpers: Vec<(&'static str, String, bool)>,
    helpers_rx: Option<std::sync::mpsc::Receiver<Vec<(&'static str, String, bool)>>>,
    auto_updates: bool,
    window_manager: Option<Entity<DownloadManagerStore>>,
}

struct MiniView {
    snapshot: crate::basket_service::MiniSnapshot,
}

#[derive(Default)]
struct BasketState {
    items: Vec<DownloadItem>,
    notice: String,
}

struct BasketView {
    state: Arc<Mutex<BasketState>>,
}

struct LinksState {
    batch: crate::links_service::GpuiLinksBatch,
    notice: String,
}

struct LinksView {
    state: Arc<Mutex<LinksState>>,
}

impl Render for LinksView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let (title, page, rows, notice) = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            (state.batch.title.clone(), state.batch.page_url.clone(), state.batch.rows.clone(), state.notice.clone())
        };
        let mut list = div().id("links-batch-list").flex().flex_col().gap_1().overflow_scroll();
        for (index, row) in rows.iter().enumerate() {
            let selected = row.checked;
            let state = self.state.clone();
            list = list.child(
                div().id(SharedString::from(format!("link-row-{index}")))
                    .flex().flex_col().gap_1().p_2().rounded_sm().bg(rgb(card_color()))
                    .cursor_pointer()
                    .child(div().text_sm().font_weight(gpui::FontWeight::BOLD)
                        .child(format!("{} {}", if selected { "☑" } else { "☐" }, row.name)))
                    .child(div().text_xs().text_color(rgb(muted_color())).child(row.url.clone()))
                    .on_click(move |_, window, _cx| {
                        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                        if let Some(row) = state.batch.rows.get_mut(index) { row.checked = !row.checked; }
                        window.refresh();
                    }),
            );
        }
        let selected_count = rows.iter().filter(|row| row.checked).count();
        let state = self.state.clone();
        let select_all = action_button("Select all", "links-select-all", move |window, _| {
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            for row in &mut state.batch.rows { row.checked = true; }
            window.refresh();
        });
        let state = self.state.clone();
        let select_none = action_button("Select none", "links-select-none", move |window, _| {
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            for row in &mut state.batch.rows { row.checked = false; }
            window.refresh();
        });
        let state = self.state.clone();
        let queue = action_button("Add to queue", "links-queue", move |window, _| {
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            let selected = state.batch.rows.iter().enumerate().filter_map(|(i, row)| row.checked.then_some(i)).collect::<Vec<_>>();
            state.notice = crate::links_service::gpui_commit(&state.batch, &selected, false);
            window.refresh();
        });
        let state = self.state.clone();
        let download = action_button("Download selected", "links-download", move |window, _| {
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            let selected = state.batch.rows.iter().enumerate().filter_map(|(i, row)| row.checked.then_some(i)).collect::<Vec<_>>();
            state.notice = crate::links_service::gpui_commit(&state.batch, &selected, true);
            window.refresh();
        });
        div().size_full().bg(rgb(background_color())).text_color(rgb(foreground_color()))
            .flex().flex_col().gap_3().p_4()
            .child(div().text_2xl().font_weight(gpui::FontWeight::BOLD)
                .child(if title.trim().is_empty() { "Download page links".to_string() } else { title }))
            .child(div().text_xs().text_color(rgb(muted_color())).child(page))
            .child(div().text_sm().child(format!("{selected_count} selected · {} link(s)", rows.len())))
            .child(div().flex().gap_2().child(select_all).child(select_none).child(queue).child(download))
            .child(div().text_sm().text_color(rgb(muted_color())).child(notice))
            .child(list)
    }
}

impl BasketView {
    fn reload(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.items = crate::basket_service::gpui_queue_items();
    }

    fn add_clipboard(state: &Arc<Mutex<BasketState>>) {
        let url = arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.get_text())
            .unwrap_or_default();
        let result = crate::basket_service::gpui_queue_one(&url);
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        state.notice = match result {
            "queued" => "Link added to the queue".into(),
            "already queued" => "That link is already queued".into(),
            "not a URL" => "Copy an http(s) download link first".into(),
            _ => "Could not add that link".into(),
        };
        state.items = crate::basket_service::gpui_queue_items();
    }

    fn import_file(state: &Arc<Mutex<BasketState>>) {
        let path = rfd::FileDialog::new()
            .add_filter("URL list", &["txt", "url"])
            .set_title("Import download links")
            .pick_file();
        let Some(path) = path else { return };
        let urls = crate::basket_service::urls_from_dropped(&path);
        let mut added = 0;
        for url in urls {
            if crate::basket_service::gpui_queue_one(&url) == "queued" {
                added += 1;
            }
        }
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        state.notice = if added > 0 {
            format!("Added {added} link(s) from {}", path.file_name().unwrap_or_default().to_string_lossy())
        } else {
            "No new http(s) links found in that file".into()
        };
        state.items = crate::basket_service::gpui_queue_items();
    }
}

impl Render for BasketView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let (items, notice) = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            (state.items.clone(), state.notice.clone())
        };
        let state = self.state.clone();
        let mut rows = div().id("basket-queue-list").flex().flex_col().gap_1().overflow_scroll();
        let mut recent = items.iter().rev().take(12).cloned().collect::<Vec<_>>();
        recent.reverse();
        for item in recent.into_iter().rev() {
            let details = format!("{} · {}", item.status.label(), item.filename);
            rows = rows.child(
                div()
                    .w_full()
                    .p_2()
                    .bg(rgb(card_color()))
                    .rounded_sm()
                    .child(div().text_sm().child(details)),
            );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .bg(rgb(background_color()))
            .text_color(rgb(foreground_color()))
            .p_4()
            .child(div().text_xl().font_weight(gpui::FontWeight::BOLD).child("Drop Basket"))
            .child(div().text_sm().text_color(rgb(muted_color())).child(notice))
            .child(
                div().flex().gap_2()
                    .child(action_button("Add clipboard URL", "basket-add-clipboard", move |window, _cx| {
                        Self::add_clipboard(&state);
                        window.refresh();
                    }))
                    .child(action_button("Import links", "basket-import", {
                        let state = self.state.clone();
                        move |window, _cx| {
                            Self::import_file(&state);
                            window.refresh();
                        }
                    }))
                    .child(action_button("Open main", "basket-open-main", move |window, _cx| {
                        crate::basket_service::gpui_launch_main();
                        window.refresh();
                    })),
            )
            .child(div().text_sm().text_color(rgb(muted_color())).child(format!("{} item(s) in queue", items.len())))
            .child(rows)
    }
}

impl MiniView {
    fn refresh(&mut self) {
        self.snapshot = crate::basket_service::mini_snapshot();
    }
}

impl Render for MiniView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.snapshot;
        let fraction = snapshot.frac.clamp(0.0, 1.0);
        div()
            .size_full()
            .flex()
            .flex_col()
            .justify_between()
            .gap_3()
            .bg(rgb(background_color()))
            .text_color(rgb(foreground_color()))
            .p_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().font_weight(gpui::FontWeight::BOLD).child("Download Craft"))
                    .child(div().text_sm().text_color(rgb(muted_color())).child(format!(
                        "{} active · {} queued",
                        snapshot.active, snapshot.queued
                    )))
                    .child(div().text_sm().child(if snapshot.speed > 0.0 {
                        format!("{}/s", model::format_bytes(snapshot.speed as u64))
                    } else {
                        "—".into()
                    }))
                    .child(
                        div()
                            .h(px(9.0))
                            .w_full()
                            .bg(rgb(button_color()))
                            .rounded_sm()
                            .child(
                                div()
                                    .h_full()
                                    .w(px(300.0 * fraction))
                                    .bg(rgb(0x8d78e8))
                                    .rounded_sm(),
                            ),
                    )
                    .child(div().text_xs().text_color(rgb(muted_color())).child(format!("{:.0}% overall", fraction * 100.0))),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(action_button("Open main", "mini-open-main", move |window, _cx| {
                        crate::basket_service::gpui_launch_main();
                        window.refresh();
                    }))
                    .child(action_button("Pause all", "mini-pause-all", move |window, _cx| {
                        crate::basket_service::gpui_send_cmd("pause_all");
                        window.refresh();
                    }))
                    .child(action_button("Resume", "mini-resume", move |window, _cx| {
                        crate::basket_service::gpui_send_cmd("resume_all");
                        window.refresh();
                    }))
                    .child(action_button("Close", "mini-close", move |window, _cx| {
                        window.remove_window();
                    })),
            )
    }
}

impl AboutView {
    fn refresh(&mut self) {
        self.snapshot = crate::about_service::gpui_snapshot();
        self.auto_updates = model::ui_pref_bool("update_auto", true);
        if let Some(rx) = &self.helpers_rx {
            if let Ok(helpers) = rx.try_recv() {
                self.helpers = helpers;
                self.helpers_rx = None;
            }
        }
    }
}

impl Render for AboutView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.snapshot.clone();
        let phase = state.phase.as_str();
        let title = state.tag.trim_start_matches('v');
        let manager = div()
            .id("update-window-content")
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_2xl().font_weight(gpui::FontWeight::BOLD).child("About Download Craft"))
                    .child(div().text_sm().text_color(rgb(muted_color())).child(format!("Version {} · Free and open source · MIT", model::APP_VERSION))),
            );

        let summary = match phase {
            "available" => format!("Version {title} is ready to install."),
            "installing" => format!("Updating to {title}. {}", state.status),
            "ready" => format!("Version {title} is installed. Restart to use it."),
            "checking" => "Checking for updates…".into(),
            "failed" => format!("Update check failed: {}", state.status),
            "uptodate" => format!("You're up to date. Version {} is current.", model::APP_VERSION),
            _ => "Check for the latest version of Download Craft.".into(),
        };
        let update_action = match phase {
            "available" => Some(("Update now", "self_update")),
            "ready" => Some(("Restart now", "restart_update")),
            "installing" | "checking" => None,
            _ => Some(("Check for updates", "check_update")),
        };
        let mut update_card = div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .p_4()
            .bg(rgb(card_color()))
            .rounded_md()
            .child(div().font_weight(gpui::FontWeight::BOLD).child("Updates"))
            .child(div().text_sm().text_color(rgb(foreground_color())).child(summary));
        if state.size > 0 && phase == "available" {
            update_card = update_card.child(
                div().text_sm().text_color(rgb(muted_color())).child(format!(
                    "{} · your settings and downloads are kept",
                    model::format_bytes(state.size)
                )),
            );
        }
        update_card = update_card.children(state.highlights.iter().map(|line| {
            div().text_sm().child(format!("•  {line}"))
        }));
        if let Some((label, command)) = update_action {
            update_card = update_card.child(action_button(label, "about-update-action", move |window, _cx| {
                crate::about_service::gpui_request(command);
                window.refresh();
            }));
        }
        if phase == "ready" && state.running > 0 {
            update_card = update_card.child(action_button(
                "Restart when downloads finish",
                "about-restart-when-idle",
                move |window, _cx| {
                    crate::about_service::gpui_request("restart_when_idle");
                    window.refresh();
                },
            ));
        }
        if !state.url.is_empty() && phase == "available" {
            let url = state.url.clone();
            update_card = update_card.child(action_button("Full release notes", "about-release-notes", move |window, _cx| {
                crate::platform::open_url(&url);
                window.refresh();
            }));
        }
        let auto_label = if self.auto_updates { "Automatic updates: On" } else { "Automatic updates: Off" };
        update_card = update_card.child(action_button(auto_label, "about-auto-updates", move |window, _cx| {
            let value = !crate::model::ui_pref_bool("update_auto", true);
            crate::model::ui_pref_set_bool("update_auto", value);
            window.refresh();
        }));

        let helper_rows = if self.helpers.is_empty() {
            vec![
                ("yt-dlp", "Checking…".to_owned(), false),
                ("FFmpeg", "Checking…".to_owned(), false),
                ("Deno", "Checking…".to_owned(), false),
            ]
        } else {
            self.helpers.clone()
        };
        let links = [
            ("GitHub", "https://github.com/hungry-detective/downloadcraft"),
            ("Release notes", "https://github.com/hungry-detective/downloadcraft/releases"),
            ("Report a problem", "https://github.com/hungry-detective/downloadcraft/issues"),
            ("Licenses", "https://github.com/hungry-detective/downloadcraft/blob/master/NOTICE.md"),
        ];
        manager
            .child(update_card)
            .child(div().font_weight(gpui::FontWeight::BOLD).child("Helpers"))
            .children(helper_rows.into_iter().map(|(name, detail, ready)| {
                let state = if ready { detail } else { format!("{detail} · Get ready for setup help") };
                div()
                    .flex()
                    .justify_between()
                    .w_full()
                    .px_3()
                    .py_2()
                    .bg(rgb(card_color()))
                    .rounded_sm()
                    .child(name)
                    .child(div().text_sm().text_color(rgb(if ready { if light_theme() { 0x16834a } else { 0x7ed6a4 } } else { muted_color() })).child(state))
            }))
            .child(action_button("Get ready", "about-setup", {
                let manager = self.window_manager.clone();
                move |window, cx| {
                if let Some(manager) = manager.as_ref() {
                    open_managed_setup(manager, cx, false);
                } else {
                    crate::setup_service::spawn(false);
                }
                window.refresh();
            }}))
            .child(div().flex().gap_3().children(links.into_iter().map(|(label, url)| {
                action_button(label, format!("about-link-{label}"), move |window, _cx| {
                    crate::platform::open_url(url);
                    window.refresh();
                })
            })))
            .child(div().text_xs().text_color(rgb(muted_color())).child(format!(
                "{} downloads currently running",
                state.running
            )))
    }
}

impl MainView {
    fn observe_manager(manager: &Entity<DownloadManagerStore>, cx: &mut Context<Self>) {
        cx.observe(manager, |_, _manager, cx| cx.notify()).detach();
        let shutdown_manager = manager.clone();
        cx.on_app_quit(move |_, cx| {
            let _ = shutdown_manager.update(cx, |manager, _cx| manager.prepare_shutdown());
            crate::tray::request_quit();
            async {}
        }).detach();
    }

    fn tick_capture_dialog(&mut self, _main_window: &mut Window, cx: &mut Context<Self>) {
        if self.last_window_prune.elapsed() >= Duration::from_secs(5) {
            prune_managed_windows(&self.manager, cx);
            self.last_window_prune = Instant::now();
        }
        if self.last_links_scan.elapsed() >= Duration::from_millis(500) {
            self.last_links_scan = Instant::now();
            for path in crate::links_service::pending_batches() {
                open_managed_links(&self.manager, cx, path);
            }
        }
        if self.capture_window.is_none() {
            let requested = self.manager.read(cx).pending_capture.clone();
            if let Some(requested) = requested {
                let key = format!("confirm:{}", capture_identity(&requested));
                if focus_managed_window(&self.manager, cx, &key) {
                    self.capture_window = self.manager.read(cx).window_handles.get(&key)
                        .and_then(|handle| handle.downcast::<crate::gpui_confirm::GpuiConfirmView>());
                    self.capture_window_key = Some(key);
                } else {
                    let runtime = self.manager.read(cx).runtime.clone();
                    if let Some(view) = crate::gpui_confirm::gpui_confirm_view(requested, runtime, self.capture_focus.clone()) {
                    use gpui::WindowKind;
                    let bounds = Bounds::centered(None, size(px(600.0), px(420.0)), cx);
                    if let Some(window) = open_managed_window(
                        &self.manager,
                        cx,
                        key.clone(),
                        WindowOptions {
                            window_bounds: Some(WindowBounds::Windowed(bounds)),
                            titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from("Add download")), ..Default::default() }),
                            kind: WindowKind::PopUp,
                            is_resizable: true,
                            is_movable: true,
                            ..Default::default()
                        },
                        move |_, cx| cx.new(|_| view),
                    ) {
                        self.capture_window = Some(window);
                        self.capture_window_key = Some(key);
                    }
                    }
                }
            }
        }

        let Some(capture_window) = self.capture_window.clone() else { return };
        let latest_capture = self.manager.read(cx).pending_capture.clone();
        match capture_window.update(cx, |view, window, cx| {
            if let Some(latest_capture) = latest_capture.as_ref() {
                if view.refresh_capture(latest_capture) == Some(true) {
                    cx.notify();
                }
            }
            view.tick();
            if view.is_done() {
                window.remove_window();
            } else {
                cx.notify();
            }
            view.is_done()
        }) {
            Ok(true) => {
                self.capture_window = None;
                if let Some(key) = self.capture_window_key.take() {
                    let _ = self.manager.update(cx, |manager, _| { manager.window_handles.remove(&key); });
                }
            }
            Err(_) => {
                self.capture_window = None;
                if let Some(key) = self.capture_window_key.take() {
                    let _ = self.manager.update(cx, |manager, _| { manager.window_handles.remove(&key); });
                }
                // Closing the confirmation window while the app is running
                // is an explicit dismissal. Keep the inbox intact during
                // application shutdown so an unresolved capture can recover
                // on the next launch.
                if matches!(model::app_lifecycle_state().as_str(), "running" | "starting") {
                    if let Some(pending) = self.manager.read(cx).pending_capture.clone() {
                        let key = capture_identity(&pending);
                        let still_pending = crate::native_host::pending_captures()
                            .ok()
                            .is_some_and(|items| items.iter().any(|item| capture_identity(item) == key));
                        if still_pending && crate::native_host::acknowledge_capture(&pending).unwrap_or(false) {
                            let _ = self.manager.update(cx, |manager, cx| {
                                manager.message = format!("Dismissed browser capture: {}", pending.filename.as_deref().unwrap_or(&pending.url));
                                cx.notify();
                            });
                        }
                    }
                }
            }
            Ok(false) => {}
        }
    }
}

fn update_manager(
    manager: &Entity<DownloadManagerStore>,
    cx: &mut App,
    update: impl FnOnce(&mut DownloadManagerStore),
) {
    let _ = manager.update(cx, |manager, cx| {
        update(manager);
        cx.notify();
    });
}

fn focus_managed_window(
    manager: &Entity<DownloadManagerStore>,
    cx: &mut App,
    key: &str,
) -> bool {
    let Some(handle) = manager.read(cx).window_handles.get(key).copied() else {
        return false;
    };
    if handle.update(cx, |_, window, _| window.activate_window()).is_ok() {
        return true;
    }
    let _ = manager.update(cx, |manager, _| {
        manager.window_handles.remove(key);
    });
    false
}

fn open_managed_window<V: 'static + Render>(
    manager: &Entity<DownloadManagerStore>,
    cx: &mut App,
    key: String,
    options: WindowOptions,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
) -> Option<WindowHandle<V>> {
    if let Some(handle) = manager.read(cx).window_handles.get(&key).copied() {
        if handle.update(cx, |_, window, _| window.activate_window()).is_ok() {
            return handle.downcast::<V>();
        }
        let _ = manager.update(cx, |manager, _| {
            manager.window_handles.remove(&key);
        });
    }
    let handle = cx.open_window(options, build).ok()?;
    let any_handle: AnyWindowHandle = handle.into();
    let registry_key = key.clone();
    let _ = manager.update(cx, |manager, _| {
        manager.window_handles.insert(registry_key, any_handle);
    });
    Some(handle)
}

fn prune_managed_windows(manager: &Entity<DownloadManagerStore>, cx: &mut App) {
    let handles = manager.read(cx).window_handles.iter()
        .map(|(key, handle)| (key.clone(), *handle))
        .collect::<Vec<_>>();
    let closed = handles.into_iter()
        .filter_map(|(key, handle)| handle.update(cx, |_, _, _| ()).is_err().then_some(key))
        .collect::<Vec<_>>();
    if !closed.is_empty() {
        let _ = manager.update(cx, |manager, _| {
            for key in closed {
                manager.window_handles.remove(&key);
            }
        });
    }
}

fn open_managed_about(manager: &Entity<DownloadManagerStore>, cx: &mut App) {
    if focus_managed_window(manager, cx, "about") {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::setup_service::helper_lines());
    });
    let initial = AboutView {
        snapshot: crate::about_service::gpui_snapshot(),
        helpers: Vec::new(),
        helpers_rx: Some(rx),
        auto_updates: model::ui_pref_bool("update_auto", true),
        window_manager: Some(manager.clone()),
    };
    let bounds = Bounds::centered(None, size(px(560.0), px(700.0)), cx);
    let Some(window) = open_managed_window(
        manager,
        cx,
        "about".into(),
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from("About Download Craft")), ..Default::default() }),
            ..Default::default()
        },
        move |_, cx| cx.new(|_| initial),
    ) else { return };
    let refresh_window = window;
    cx.spawn(async move |cx| loop {
        Timer::after(Duration::from_millis(400)).await;
        if refresh_window.update(cx, |view, native_window, cx| {
            view.refresh();
            cx.notify();
            native_window.refresh();
        }).is_err() {
            break;
        }
    }).detach();
}

fn open_managed_settings(manager: &Entity<DownloadManagerStore>, cx: &mut App, pane: SettingsPane) {
    if focus_managed_window(manager, cx, "settings") {
        return;
    }
    let initial_proxy = model::ui_pref_str("proxy_url", "");
    let bounds = Bounds::centered(None, size(px(720.0), px(620.0)), cx);
    let Some(window) = open_managed_window(
        manager,
        cx,
        "settings".into(),
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from("Download Craft Settings")), ..Default::default() }),
            ..Default::default()
        },
        move |_, cx| {
            let initial_proxy = initial_proxy.clone();
            cx.new(|cx| SettingsView {
                pane: Arc::new(Mutex::new(pane)),
                proxy_focus: cx.focus_handle(),
                proxy_draft: Arc::new(Mutex::new(initial_proxy)),
                proxy_notice: Arc::new(Mutex::new(String::new())),
            })
        },
    ) else { return };
    let refresh_window = window;
    cx.spawn(async move |cx| loop {
        Timer::after(Duration::from_millis(500)).await;
        if refresh_window.update(cx, |_view, native_window, cx| {
            cx.notify();
            native_window.refresh();
        }).is_err() {
            break;
        }
    }).detach();
}

fn open_managed_setup(manager: &Entity<DownloadManagerStore>, cx: &mut App, auto: bool) {
    if focus_managed_window(manager, cx, "setup") {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let data = Arc::new(Mutex::new(SetupData {
        states: crate::setup_service::Part::ALL.into_iter()
            .map(|part| (part, crate::setup_service::State::Checking)).collect(),
        tx: tx.clone(),
        rx,
        busy: true,
        show_at_start: model::ui_pref_bool("setup_at_start", false),
        auto_fix_pending: auto,
        last_extension_check: Instant::now(),
    }));
    crate::setup_service::run_checks(tx, crate::setup_service::Part::ALL.to_vec());
    let bounds = Bounds::centered(None, size(px(600.0), px(700.0)), cx);
    let data_for_view = data.clone();
    let Some(window) = open_managed_window(
        manager,
        cx,
        "setup".into(),
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from("Get ready — Download Craft")), ..Default::default() }),
            ..Default::default()
        },
        move |_, cx| cx.new(|_| SetupView { data: data_for_view }),
    ) else { return };
    let refresh_window = window;
    cx.spawn(async move |cx| loop {
        Timer::after(Duration::from_millis(300)).await;
        if refresh_window.update(cx, |view, native_window, cx| {
            view.poll();
            cx.notify();
            native_window.refresh();
        }).is_err() {
            break;
        }
    }).detach();
}

pub(crate) enum ManagedWindowRequest {
    About,
    Settings { pane: usize },
    Setup { auto: bool },
    Progress { id: u64 },
    Complete { id: u64 },
    Basket,
    Mini,
    Links { path: PathBuf },
}

pub(crate) fn open_managed_secondary(
    manager: &Entity<DownloadManagerStore>,
    cx: &mut App,
    request: ManagedWindowRequest,
) {
    match request {
        ManagedWindowRequest::About => open_managed_about(manager, cx),
        ManagedWindowRequest::Settings { pane } => {
            let pane = match pane {
                1 => SettingsPane::Downloads,
                2 => SettingsPane::Plugins,
                3 => SettingsPane::Browser,
                4 => SettingsPane::Proxy,
                5 => SettingsPane::Categories,
                6 => SettingsPane::Sounds,
                7 => SettingsPane::Updates,
                _ => SettingsPane::Appearance,
            };
            open_managed_settings(manager, cx, pane);
        }
        ManagedWindowRequest::Setup { auto } => open_managed_setup(manager, cx, auto),
        ManagedWindowRequest::Progress { id } => open_managed_progress(manager, cx, id),
        ManagedWindowRequest::Complete { id } => open_managed_complete(manager, cx, id),
        ManagedWindowRequest::Basket => open_managed_basket(manager, cx),
        ManagedWindowRequest::Mini => open_managed_mini(manager, cx),
        ManagedWindowRequest::Links { path } => open_managed_links(manager, cx, path),
    }
}

fn open_managed_progress(manager: &Entity<DownloadManagerStore>, cx: &mut App, id: u64) {
    let key = format!("progress:{id}");
    if focus_managed_window(manager, cx, &key) { return; }
    let view = ProgressView {
        id,
        snapshot: crate::progress_service::gpui_snapshot(id, false),
        tab: Arc::new(Mutex::new(ProgressTab::Info)),
        last_log_read: Instant::now(),
    };
    let bounds = Bounds::centered(None, size(px(540.0), px(620.0)), cx);
    let Some(window) = open_managed_window(
        manager,
        cx,
        key,
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from(format!("Download Craft · #{id}"))), ..Default::default() }),
            ..Default::default()
        },
        move |_, cx| cx.new(|_| view),
    ) else { return };
    cx.spawn(async move |cx| loop {
        Timer::after(Duration::from_millis(250)).await;
        if window.update(cx, |view, native_window, cx| {
            view.refresh();
            let handoff = view.snapshot.item.as_ref().is_some_and(|item| {
                !view.snapshot.live && matches!(&item.status, Status::Completed)
                    && model::ui_pref_bool("show_complete", true)
            });
            if handoff { native_window.remove_window(); } else { cx.notify(); native_window.refresh(); }
            handoff
        }).is_err() { break; }
    }).detach();
}

fn open_managed_complete(manager: &Entity<DownloadManagerStore>, cx: &mut App, id: u64) {
    let key = format!("complete:{id}");
    if focus_managed_window(manager, cx, &key) { return; }
    let Some(item) = crate::complete_service::gpui_item(id) else { return };
    let view = CompleteView {
        item,
        dont_show: Arc::new(Mutex::new(!model::ui_pref_bool("show_complete", true))),
    };
    let bounds = Bounds::centered(None, size(px(560.0), px(360.0)), cx);
    let Some(window) = open_managed_window(
        manager,
        cx,
        key,
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from("Download complete")), ..Default::default() }),
            ..Default::default()
        },
        move |_, cx| cx.new(|_| view),
    ) else { return };
    cx.spawn(async move |cx| loop {
        Timer::after(Duration::from_millis(500)).await;
        if window.update(cx, |_view, native_window, cx| {
            cx.notify();
            native_window.refresh();
        }).is_err() { break; }
    }).detach();
}

fn open_managed_mini(manager: &Entity<DownloadManagerStore>, cx: &mut App) {
    if focus_managed_window(manager, cx, "mini") { return; }
    let initial = MiniView { snapshot: crate::basket_service::mini_snapshot() };
    let bounds = Bounds::centered(None, size(px(380.0), px(250.0)), cx);
    let Some(window) = open_managed_window(
        manager,
        cx,
        "mini".into(),
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from("Download Craft Mini")), ..Default::default() }),
            ..Default::default()
        },
        move |_, cx| cx.new(|_| initial),
    ) else { return };
    cx.spawn(async move |cx| loop {
        Timer::after(Duration::from_secs(1)).await;
        if window.update(cx, |view, native_window, cx| {
            view.refresh();
            cx.notify();
            native_window.refresh();
        }).is_err() { break; }
    }).detach();
}

fn open_managed_basket(manager: &Entity<DownloadManagerStore>, cx: &mut App) {
    if focus_managed_window(manager, cx, "basket") { return; }
    let state = Arc::new(Mutex::new(BasketState {
        items: crate::basket_service::gpui_queue_items(),
        notice: "Copy a download URL or import a .txt / .url list.".into(),
    }));
    let bounds = Bounds::centered(None, size(px(440.0), px(500.0)), cx);
    let state_for_view = state.clone();
    let Some(window) = open_managed_window(
        manager,
        cx,
        "basket".into(),
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from("Drop Basket")), ..Default::default() }),
            ..Default::default()
        },
        move |_, cx| cx.new(|_| BasketView { state: state_for_view }),
    ) else { return };
    cx.spawn(async move |cx| loop {
        Timer::after(Duration::from_secs(2)).await;
        if window.update(cx, |view, native_window, cx| {
            view.reload();
            cx.notify();
            native_window.refresh();
        }).is_err() { break; }
    }).detach();
}

fn open_managed_links(manager: &Entity<DownloadManagerStore>, cx: &mut App, path: PathBuf) {
    let key = format!("links:{}", path.display());
    if focus_managed_window(manager, cx, &key) { return; }
    let batch = match crate::links_service::gpui_load(&path) {
        Ok(batch) => batch,
        Err(error) => {
            eprintln!("--links: {error}");
            return;
        }
    };
    let count = batch.rows.len();
    let state = Arc::new(Mutex::new(LinksState {
        batch,
        notice: "Select files to add to the download queue.".into(),
    }));
    let bounds = Bounds::centered(None, size(px(760.0), px(640.0)), cx);
    let state_for_view = state.clone();
    let _ = open_managed_window(
        manager,
        cx,
        key,
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from(format!("Download page links ({count})"))), ..Default::default() }),
            ..Default::default()
        },
        move |_, cx| cx.new(|_| LinksView { state: state_for_view }),
    );
}

fn initialize_gpui_app(cx: &mut App) {
    if let Err(error) = cx.text_system().add_fonts(vec![
        std::borrow::Cow::Borrowed(&include_bytes!("../assets/fonts/MaterialIcons-Regular.ttf")[..]),
    ]) {
        eprintln!("Could not register Material Icons font: {error}");
    }
    cx.on_window_closed(|cx| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    }).detach();
}

impl Render for MainView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.manager.read(cx);
        let message = state.message.clone();
        let grabber_open = state.grabber_open;
        let site_rules_open = state.site_rules_open;
        let site_rule_draft = state.site_rule_draft.clone();
        let site_rule_notice = state.site_rule_notice.clone();
        let grab_url = state.grab_url.clone();
        let grab_exts = state.grab_exts.clone();
        let grab_depth = state.grab_depth;
        let grab_stay_under = state.grab_stay_under;
        let grab_busy = state.grab_busy;
        let grab_notice = state.grab_notice.clone();
        let grab_results = state.grab_results.clone();
        let filter = state.filter;
        let sort = state.sort;
        let search_query = state.search_query.clone();
        let search_focus = self.search_focus.clone();
        let selected_count = state.items.iter().filter(|item| item.selected).count();
        let item_count = state.items.len();
        let completed_count = state.items.iter().filter(|item| matches!(item.status, Status::Completed)).count();
        let active_count = state.items.iter().filter(|item| item.status.is_active()).count();
        let queued_count = state.items.iter().filter(|item| matches!(item.status, Status::Queued | Status::Paused)).count();
        let failed_count = state.items.iter().filter(|item| matches!(item.status, Status::Failed(_))).count();
        let query = search_query.trim().to_lowercase();
        let mut visible_indices = state.items.iter().enumerate()
            .filter_map(|(index, item)| {
                let matches_query = query.is_empty()
                    || item.filename.to_lowercase().contains(&query)
                    || item.url.to_lowercase().contains(&query)
                    || item.status.label().to_lowercase().contains(&query);
                (filter.matches(&item.status) && matches_query).then_some(index)
            })
            .collect::<Vec<_>>();
        visible_indices.sort_by(|left, right| {
            let left_item = &state.items[*left];
            let right_item = &state.items[*right];
            let order = match sort {
                QueueSort::Added => left_item.created_at.cmp(&right_item.created_at),
                QueueSort::Name => left_item.filename.cmp(&right_item.filename),
                QueueSort::Size => left_item.total_bytes.unwrap_or(left_item.downloaded_bytes)
                    .cmp(&right_item.total_bytes.unwrap_or(right_item.downloaded_bytes)),
                QueueSort::Status => left_item.status.label().cmp(&right_item.status.label()),
            };
            order.then_with(|| left_item.created_at.cmp(&right_item.created_at)).then_with(|| left_item.id.cmp(&right_item.id))
        });
        let manager = self.manager.clone();
        let row_manager = self.manager.clone();
        let sidebar_manager = manager.clone();
        if grabber_open {
            let grab_manager = manager.clone();
            let focus = self.grab_focus.clone();
            let result_rows = grab_results.iter().take(200).enumerate().map(|(index, link)| {
                div()
                    .id(SharedString::from(format!("grab-result-{index}")))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w_full()
                    .p_3()
                    .bg(rgb(card_color()))
                    .rounded_md()
                    .child(link.name.clone())
                    .child(div().text_xs().text_color(rgb(muted_color())).child(link.url.clone()))
            }).collect::<Vec<_>>();
            let input_manager = grab_manager.clone();
            let focused_input = focus.clone();
            let grab_input = div()
                .id("grab-page-url")
                .track_focus(&focus)
                .w_full()
                .p_3()
                .bg(rgb(card_color()))
                .rounded_md()
                .border_1()
                .border_color(rgb(if focus.is_focused(_window) { accent_color() } else { button_color() }))
                .child(if grab_url.is_empty() { "Click here, then type or paste a page URL".to_string() } else { grab_url.clone() })
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |_this, _, window, _| focused_input.focus(window)))
                .on_key_down(cx.listener(move |_this, event: &KeyDownEvent, window, cx| {
                    let key = event.keystroke.key.as_str();
                    let modifiers = &event.keystroke.modifiers;
                    let paste = (modifiers.control || modifiers.platform) && key.eq_ignore_ascii_case("v");
                    let mut pasted = None;
                    if paste {
                        if let Ok(mut clipboard) = arboard::Clipboard::new() {
                            pasted = clipboard.get_text().ok();
                        }
                    }
                    let enter = key == "enter" || key == "return";
                    let _ = input_manager.update(cx, |manager, cx| {
                        if enter {
                            manager.start_grab();
                        } else if paste {
                            if let Some(text) = pasted {
                                manager.grab_url = text.trim().to_string();
                            }
                        } else if key == "backspace" {
                            manager.grab_url.pop();
                        } else if !modifiers.control && !modifiers.platform && !modifiers.alt {
                            if let Some(ch) = &event.keystroke.key_char {
                                if !ch.chars().any(char::is_control) {
                                    manager.grab_url.push_str(ch);
                                }
                            }
                        }
                        cx.notify();
                    });
                    window.refresh();
                }));
            let grab_controls = div().flex().items_center().gap_2()
                .child(action_button(if grab_busy { "Scanning…" } else { "Grab page" }, "grab-start", {
                    let manager = grab_manager.clone();
                    move |window, cx| { update_manager(&manager, cx, |manager| manager.start_grab()); window.refresh(); }
                }).when(grab_busy, |button| button.opacity(0.6)))
                .child(action_button("Queue found links", "grab-queue", {
                    let manager = grab_manager.clone();
                    move |window, cx| { update_manager(&manager, cx, |manager| manager.queue_grab_results()); window.refresh(); }
                }).when(grab_results.is_empty(), |button| button.opacity(0.6)))
                .child(action_button(format!("Depth: {grab_depth}"), "grab-depth", {
                    let manager = grab_manager.clone();
                    move |window, cx| { update_manager(&manager, cx, |manager| manager.grab_depth = if manager.grab_depth >= 3 { 1 } else { manager.grab_depth + 1 }); window.refresh(); }
                }))
                .child(action_button(if grab_stay_under { "Same site: on" } else { "Same site: off" }, "grab-same-site", {
                    let manager = grab_manager.clone();
                    move |window, cx| { update_manager(&manager, cx, |manager| manager.grab_stay_under = !manager.grab_stay_under); window.refresh(); }
                }))
                .child(action_button("Next file preset", "grab-file-preset", {
                    let manager = grab_manager.clone();
                    move |window, cx| {
                        update_manager(&manager, cx, |manager| {
                            let presets = ["Mixed", "Video", "Images", "Documents", "Archives", "Audio"];
                            let current = presets.iter().position(|preset| crate::grabber::preset_exts(preset) == manager.grab_exts).unwrap_or(0);
                            manager.grab_exts = crate::grabber::preset_exts(presets[(current + 1) % presets.len()]);
                        });
                        window.refresh();
                    }
                }))
                .child(action_button("Back to downloads", "grab-back", move |window, cx| {
                    update_manager(&grab_manager, cx, |manager| manager.grabber_open = false);
                    window.refresh();
                }));
            let grabber_panel = div().flex_1().min_w_0().h_full().flex().flex_col().gap_4()
                .bg(rgb(background_color())).text_color(rgb(foreground_color())).p_5()
                .child(div().text_2xl().font_weight(gpui::FontWeight::BOLD).child("Site Grabber"))
                .child(div().text_sm().text_color(rgb(muted_color())).child("Scan a page for downloadable files. Passive browser media detections are not sent here automatically."))
                .child(grab_input)
                .child(div().flex().items_center().gap_2()
                    .child(div().text_sm().text_color(rgb(muted_color())).child("File types"))
                    .child(div().flex_1().p_2().bg(rgb(card_color())).rounded_sm().child(grab_exts)))
                .child(grab_controls)
                .child(div().text_sm().text_color(rgb(muted_color())).child(grab_notice))
                .child(div().id("grab-results").flex_1().min_h_0().flex().flex_col().gap_2().overflow_scroll().children(result_rows));
            let back_manager = manager.clone();
            let grab_sidebar = div().w(px(180.0)).flex_none().flex().flex_col().gap_2().h_full().pr_3()
                .child(div().px_2().py_2().text_xs().font_weight(gpui::FontWeight::BOLD).text_color(rgb(muted_color())).child("DOWNLOADS"))
                .child(action_button("All downloads", "grabber-back-sidebar", move |window, cx| {
                    update_manager(&back_manager, cx, |manager| manager.grabber_open = false);
                    window.refresh();
                }).w_full())
                .child(div().h(px(12.0)))
                .child(div().px_2().py_2().text_xs().font_weight(gpui::FontWeight::BOLD).text_color(rgb(accent_color())).child("SITE TOOLS"))
                .child(action_button("Site Grabber", "grabber-current", move |window, _| window.refresh()).w_full().bg(rgb(accent_color())))
                .child(action_button("Site Rules", "grabber-open-rules", {
                    let manager = manager.clone();
                    move |window, cx| {
                        update_manager(&manager, cx, |manager| {
                            manager.grabber_open = false;
                            manager.site_rules_open = true;
                        });
                        window.refresh();
                    }
                }).w_full());
            return div().size_full().flex().bg(rgb(background_color())).p_4().gap_3().child(grab_sidebar).child(grabber_panel);
        }
        if site_rules_open {
            let manager = manager.clone();
            let focus = self.site_rule_focus.clone();
            let input_manager = manager.clone();
            let focused_input = focus.clone();
            let site_input = div()
                .id("site-rule-host")
                .track_focus(&focus)
                .w_full()
                .p_3()
                .bg(rgb(card_color()))
                .rounded_md()
                .border_1()
                .border_color(rgb(if focus.is_focused(_window) { accent_color() } else { button_color() }))
                .child(if site_rule_draft.is_empty() { "Enter a host, for example example.com".to_string() } else { site_rule_draft.clone() })
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |_this, _, window, _| focused_input.focus(window)))
                .on_key_down(cx.listener(move |_this, event: &KeyDownEvent, window, cx| {
                    let key = event.keystroke.key.as_str();
                    let modifiers = &event.keystroke.modifiers;
                    let paste = (modifiers.control || modifiers.platform) && key.eq_ignore_ascii_case("v");
                    let mut pasted = None;
                    if paste {
                        if let Ok(mut clipboard) = arboard::Clipboard::new() {
                            pasted = clipboard.get_text().ok();
                        }
                    }
                    let _ = input_manager.update(cx, |manager, cx| {
                        if paste {
                            if let Some(text) = pasted {
                                manager.site_rule_draft = text.trim().to_string();
                            }
                        } else if key == "backspace" {
                            manager.site_rule_draft.pop();
                        } else if !modifiers.control && !modifiers.platform && !modifiers.alt {
                            if let Some(ch) = &event.keystroke.key_char {
                                if !ch.chars().any(char::is_control) {
                                    manager.site_rule_draft.push_str(ch);
                                }
                            }
                        }
                        manager.site_rule_notice.clear();
                        cx.notify();
                    });
                    window.refresh();
                }));
            let hosts = configured_block_hosts();
            let host_rows = hosts.iter().map(|host| {
                let host = host.clone();
                settings_row(
                    host.clone(),
                    "This host and its subdomains stay in the browser.".into(),
                    "Remove",
                    format!("site-rule-remove-{host}"),
                    move |window| {
                        let mut remaining = configured_block_hosts();
                        remaining.retain(|entry| entry != &host);
                        model::ui_pref_set_str("capture_skip_sites", &remaining.join(", "));
                        window.refresh();
                    },
                )
            }).collect::<Vec<_>>();
            let official_rules = crate::native_host::site_rules()
                .get("capture")
                .and_then(|capture| capture.get("neverSites"))
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let official_rows = official_rules.iter().filter_map(serde_json::Value::as_str).map(|host| {
                div().w_full().p_2().bg(rgb(card_color())).rounded_sm().child(host.to_string())
            }).collect::<Vec<_>>();
            let add_manager = manager.clone();
            let add_site = action_button("Add host", "site-rule-add", move |window, cx| {
                let raw = add_manager.read(cx).site_rule_draft.clone();
                let Some(host) = normalize_block_host(&raw) else {
                    update_manager(&add_manager, cx, |manager| manager.site_rule_notice = "Enter a host name only; schemes, paths, and ports are not accepted.".into());
                    window.refresh();
                    return;
                };
                let mut hosts = configured_block_hosts();
                if hosts.iter().any(|existing| existing == &host) {
                    update_manager(&add_manager, cx, |manager| {
                        manager.site_rule_notice = format!("{host} is already excluded.");
                        manager.site_rule_draft.clear();
                    });
                } else {
                    hosts.push(host.clone());
                    model::ui_pref_set_str("capture_skip_sites", &hosts.join(", "));
                    update_manager(&add_manager, cx, |manager| {
                        manager.site_rule_notice = format!("Added {host}; it applies to this host and its subdomains.");
                        manager.site_rule_draft.clear();
                    });
                }
                window.refresh();
            });
            let grab_nav = manager.clone();
            let all_nav = manager.clone();
            let sidebar = div().w(px(180.0)).flex_none().flex().flex_col().gap_2().h_full().pr_3()
                .child(div().px_2().py_2().text_xs().font_weight(gpui::FontWeight::BOLD).text_color(rgb(muted_color())).child("DOWNLOADS"))
                .child(action_button("All downloads", "rules-back-downloads", move |window, cx| {
                    update_manager(&all_nav, cx, |manager| manager.site_rules_open = false);
                    window.refresh();
                }).w_full())
                .child(div().h(px(12.0)))
                .child(div().px_2().py_2().text_xs().font_weight(gpui::FontWeight::BOLD).text_color(rgb(accent_color())).child("SITE TOOLS"))
                .child(action_button("Site Grabber", "rules-open-grabber", move |window, cx| {
                    update_manager(&grab_nav, cx, |manager| {
                        manager.site_rules_open = false;
                        manager.grabber_open = true;
                    });
                    window.refresh();
                }).w_full())
                .child(action_button("Site Rules", "rules-current", move |window, _| window.refresh()).w_full().bg(rgb(accent_color())));
            let panel = div().flex_1().min_w_0().h_full().flex().flex_col().gap_4().bg(rgb(background_color())).text_color(rgb(foreground_color())).p_5()
                .child(div().text_2xl().font_weight(gpui::FontWeight::BOLD).child("Site Rules"))
                .child(div().text_sm().text_color(rgb(muted_color())).child("Manage personal capture exclusions. Excluded hosts and their subdomains stay in the browser."))
                .child(div().flex().gap_2().items_center().child(site_input).child(add_site))
                .child(div().text_sm().text_color(rgb(muted_color())).child(site_rule_notice))
                .child(div().text_lg().font_weight(gpui::FontWeight::BOLD).child("Your exclusions"))
                .child(div().id("site-rule-hosts").flex().flex_col().gap_2().w_full().overflow_scroll().children(host_rows))
                .child(div().text_sm().text_color(rgb(muted_color())).child(if hosts.is_empty() { "No personal exclusions" } else { "Remove a host to resume browser capture there." }))
                .child(div().text_lg().font_weight(gpui::FontWeight::BOLD).child("Download Craft rules"))
                .child(div().text_sm().text_color(rgb(muted_color())).child("Published rules are read-only and may update from the project."))
                .child(div().id("official-site-rules").flex().flex_col().gap_1().w_full().overflow_scroll().children(official_rows));
            return div().size_full().flex().bg(rgb(background_color())).p_4().gap_3().child(sidebar).child(panel);
        }
        let sidebar = div()
            .w(px(180.0))
            .flex_none()
            .flex()
            .flex_col()
            .h_full()
            .gap_2()
            .pr_3()
            .child(div().px_2().py_2().text_xs().font_weight(gpui::FontWeight::BOLD).text_color(rgb(muted_color())).child("DOWNLOADS"))
            .children(QueueFilter::ALL.into_iter().map(|entry| {
                let count = match entry {
                    QueueFilter::All => item_count,
                    QueueFilter::Active => active_count,
                    QueueFilter::Queued => queued_count,
                    QueueFilter::Completed => completed_count,
                    QueueFilter::Failed => failed_count,
                };
                let selected = entry == filter;
                let entry_manager = sidebar_manager.clone();
                sidebar_button(entry.label(), count, format!("sidebar-{}", entry.label()), selected, move |window, cx| {
                    update_manager(&entry_manager, cx, |manager| manager.filter = entry);
                    window.refresh();
                })
            }))
            .child(div().h(px(12.0)))
            .child(div().px_2().py_2().text_xs().font_weight(gpui::FontWeight::BOLD).text_color(rgb(muted_color())).child("SITE TOOLS"))
            .child(action_button("Site Grabber", "sidebar-grabber", move |window, cx| {
                update_manager(&sidebar_manager, cx, |manager| {
                    manager.grabber_open = true;
                    manager.site_rules_open = false;
                });
                window.refresh();
            }).w_full())
            .child(action_button("Site Rules", "sidebar-site-rules", {
                let rules_manager = self.manager.clone();
                move |window, cx| {
                    update_manager(&rules_manager, cx, |manager| {
                        manager.grabber_open = false;
                        manager.site_rules_open = true;
                    });
                    window.refresh();
                }
            }).w_full());

        let search_manager = manager.clone();
        let search_input = div()
            .id("download-search")
            .track_focus(&search_focus)
            .flex_1()
            .min_w_0()
            .p_2()
            .bg(rgb(card_color()))
            .rounded_sm()
            .border_1()
            .border_color(rgb(if search_focus.is_focused(_window) { accent_color() } else { button_color() }))
            .child(if search_query.is_empty() { "Search downloads by name, URL, or status".to_string() } else { search_query.clone() })
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |_this, _, window, _| search_focus.focus(window)))
            .on_key_down(cx.listener(move |_this, event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                let modifiers = &event.keystroke.modifiers;
                let paste = (modifiers.control || modifiers.platform) && key.eq_ignore_ascii_case("v");
                let mut pasted = None;
                if paste {
                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                        pasted = clipboard.get_text().ok();
                    }
                }
                let _ = search_manager.update(cx, |manager, cx| {
                    if paste {
                        if let Some(text) = pasted {
                            manager.search_query = text.trim().to_string();
                        }
                    } else if key == "escape" {
                        manager.search_query.clear();
                    } else if key == "backspace" {
                        manager.search_query.pop();
                    } else if !modifiers.control && !modifiers.platform && !modifiers.alt {
                        if let Some(ch) = &event.keystroke.key_char {
                            if !ch.chars().any(char::is_control) {
                                manager.search_query.push_str(ch);
                            }
                        }
                    }
                    cx.notify();
                });
                window.refresh();
            }));

        let visible_ids = visible_indices.iter().map(|index| state.items[*index].id).collect::<Vec<_>>();
        let all_visible_selected = !visible_indices.is_empty()
            && visible_indices.iter().all(|index| state.items[*index].selected);
        let selection_manager = manager.clone();
        let select_visible = command_button(
            if all_visible_selected { "Clear visible" } else { "Select visible" },
            "select-visible-downloads",
            false,
            move |window, cx| {
                update_manager(&selection_manager, cx, |manager| manager.set_selected(&visible_ids, !all_visible_selected));
                window.refresh();
            },
        );
        let sort_manager = manager.clone();
        let sort_button = command_button(format!("Sort · {}", sort.label()), "sort-downloads", false, move |window, cx| {
            update_manager(&sort_manager, cx, |manager| manager.sort = manager.sort.next());
            window.refresh();
        });
        let pause_selected_manager = manager.clone();
        let resume_selected_manager = manager.clone();
        let remove_selected_manager = manager.clone();
        let selection_controls = div().flex().items_center().gap_1()
            .child(select_visible)
            .child(div().px_2().text_xs().font_family("monospace").text_color(rgb(muted_color())).child(format!("{selected_count} selected · {} shown", visible_indices.len())))
            .child(command_button("Pause selected", "pause-selected", false, move |window, cx| {
                update_manager(&pause_selected_manager, cx, |manager| manager.pause_selected());
                window.refresh();
            }).when(selected_count == 0, |button| button.opacity(0.55)))
            .child(command_button("Resume selected", "resume-selected", false, move |window, cx| {
                update_manager(&resume_selected_manager, cx, |manager| manager.resume_selected());
                window.refresh();
            }).when(selected_count == 0, |button| button.opacity(0.55)))
            .child(command_button("Remove selected", "remove-selected", false, move |window, cx| {
                update_manager(&remove_selected_manager, cx, |manager| manager.remove_selected());
                window.refresh();
            }).when(selected_count == 0, |button| button.opacity(0.55)));

        let rows = uniform_list(
            "download-rows",
            visible_indices.len(),
            cx.processor(move |_view, range: std::ops::Range<usize>, _window, app| {
                let mut rendered = Vec::with_capacity(range.len());
                for index in range {
                    let item_index = visible_indices[index];
                    let Some(item) = row_manager.read(app).items.get(item_index).cloned() else {
                        continue;
                    };
                    let id = item.id;
                    let status = item.status.label();
                    let progress = format!("{:.0}%", item.progress() * 100.0);
                    let speed = if item.status.is_active() && item.speed_bps > 0.0 {
                        model::format_speed(item.speed_bps)
                    } else {
                        String::new()
                    };
                    let details = if let Some(total) = item.total_bytes {
                        format!("{} / {}  ·  {}  ·  {}  ·  {}", model::format_bytes(item.downloaded_bytes), model::format_bytes(total), progress, speed, status)
                    } else {
                        format!("{}  ·  {}  ·  {}  ·  {}", model::format_bytes(item.downloaded_bytes), progress, speed, status)
                    };
                    let action_label = match &item.status {
                        Status::Downloading => "Pause",
                        Status::Completed => "Open file",
                        Status::Failed(_) => "Retry",
                        Status::Queued | Status::Paused => "Resume",
                    };
                    let detail_label = if matches!(&item.status, Status::Completed) { "Complete" } else { "Progress" };
                    let complete_details = matches!(&item.status, Status::Completed);
                    let completed_path = item.dest_path();
                    let toggle_manager = row_manager.clone();
                    let remove_manager = row_manager.clone();
                    let select_manager = row_manager.clone();
                    let detail_manager = row_manager.clone();
                    let selected = item.selected;
                    rendered.push(
                        div()
                            .id(SharedString::from(format!("download-row-{id}")))
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .w_full()
                            .h(px(48.0))
                            .px_2()
                            .border_b_1()
                            .border_color(rgb(button_color()))
                            .child(row_action_button(if selected { "☑" } else { "☐" }, format!("select-{id}"), move |window, app| {
                                update_manager(&select_manager, app, |manager| manager.toggle_selection(id));
                                window.refresh();
                            }))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_sm().child(item.filename))
                                    .child(div().text_xs().font_family("monospace").text_color(rgb(muted_color())).child(details)),
                            )
                            .child(row_action_button(action_label, format!("toggle-{id}"), move |window, app| {
                                if action_label == "Open file" {
                                    crate::platform::open(&completed_path, false);
                                } else {
                                    update_manager(&toggle_manager, app, |manager| manager.toggle(id));
                                }
                                window.refresh();
                            }))
                            .child(row_action_button(detail_label, format!("details-{id}"), move |window, app| {
                                let request = if complete_details {
                                    ManagedWindowRequest::Complete { id }
                                } else {
                                    ManagedWindowRequest::Progress { id }
                                };
                                open_managed_secondary(&detail_manager, app, request);
                                window.refresh();
                            }))
                            .child(row_action_button("Remove", format!("remove-{id}"), move |window, app| {
                                update_manager(&remove_manager, app, |manager| manager.remove(id));
                                window.refresh();
                            })),
                    );
                }
                rendered
            }),
        )
        .w_full()
        .h_full();

        let content = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(background_color()))
            .text_color(rgb(foreground_color()))
            .p_4()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_2xl().font_weight(gpui::FontWeight::BOLD).child("Download Craft"))
                            .child(div().text_sm().text_color(rgb(muted_color())).child(message)),
                    )
                    .child(div().text_sm().text_color(rgb(muted_color())).child(format!("{item_count} downloads · {active_count} active · {completed_count} completed"))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1()
                    .py_1()
                    .border_b_1()
                    .border_color(rgb(button_color()))
                    .child(command_button("Paste URL", "add-clipboard", true, {
                        let manager = manager.clone();
                        move |window, cx| {
                        let mut url = String::new();
                        if let Ok(mut clipboard) = arboard::Clipboard::new() {
                            url = clipboard.get_text().unwrap_or_default();
                        }
                        update_manager(&manager, cx, |manager| { manager.add_url(&url); });
                        window.refresh();
                    }}))
                    .child(command_button("Import", "import-links", false, {
                        let manager = manager.clone();
                        move |window, cx| {
                            update_manager(&manager, cx, |manager| manager.import_links_file());
                            window.refresh();
                        }
                    }))
                    .child(command_button("Review list", "review-link-list", false, {
                        let manager = manager.clone();
                        move |window, cx| {
                            let path = rfd::FileDialog::new()
                                .add_filter("URL list", &["txt", "url"])
                                .set_title("Review download links")
                                .pick_file();
                            if let Some(path) = path {
                                open_managed_secondary(&manager, cx, ManagedWindowRequest::Links { path });
                            }
                            window.refresh();
                        }
                    }))
                    .child(command_button("Start queue", "start-queue", false, {
                        let manager = manager.clone();
                        move |window, cx| {
                            update_manager(&manager, cx, |manager| manager.start_queue());
                            window.refresh();
                        }
                    }))
                    .child(command_button("Pause queue", "pause-queue", false, {
                        let manager = manager.clone();
                        move |window, cx| {
                            update_manager(&manager, cx, |manager| manager.pause_queue());
                            window.refresh();
                        }
                    })),
            )
            .child(div().flex().items_center().gap_1().child(search_input).child(sort_button))
            .child(selection_controls)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1()
                    .child(command_button("Settings", "open-settings", false, {
                        let manager = manager.clone();
                        move |window, cx| {
                        open_managed_secondary(&manager, cx, ManagedWindowRequest::Settings { pane: 0 });
                        window.refresh();
                    }}))
                    .child(command_button("About", "open-about", false, {
                        let manager = manager.clone();
                        move |window, cx| {
                        open_managed_secondary(&manager, cx, ManagedWindowRequest::About);
                        window.refresh();
                    }}))
                    .child(command_button("Setup", "open-setup", false, {
                        let manager = manager.clone();
                        move |window, cx| {
                        open_managed_secondary(&manager, cx, ManagedWindowRequest::Setup { auto: false });
                        window.refresh();
                    }}))
                    .child(command_button("Basket", "open-basket", false, {
                        let manager = manager.clone();
                        move |window, cx| {
                            open_managed_secondary(&manager, cx, ManagedWindowRequest::Basket);
                            window.refresh();
                        }
                    }))
                    .child(command_button("Mini", "open-mini", false, {
                        let manager = manager.clone();
                        move |window, cx| {
                            open_managed_secondary(&manager, cx, ManagedWindowRequest::Mini);
                            window.refresh();
                        }
                    })),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .px_2()
                    .text_xs()
                    .text_color(rgb(muted_color()))
                    .child("FILE")
                    .child("PROGRESS · STATUS · ACTIONS"),
            )
            .child(rows)
            ;

        div()
            .size_full()
            .flex()
            .flex_row()
            .bg(rgb(background_color()))
            .text_color(rgb(foreground_color()))
            .p_4()
            .gap_3()
            .child(sidebar)
            .child(content)
    }
}

fn action_button(
    label: impl Into<SharedString>,
    id: impl Into<SharedString>,
    callback: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let label: SharedString = label.into();
    div()
        .id(id.into())
        .flex_none()
        .px_3()
        .py_2()
        .bg(rgb(button_color()))
        .hover(|style| style.bg(rgb(button_hover_color())))
        .rounded_sm()
        .cursor_pointer()
        .child(label)
        .on_click(move |_, window, cx| callback(window, cx))
}

fn command_button(
    label: impl Into<SharedString>,
    id: impl Into<SharedString>,
    primary: bool,
    callback: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let label: SharedString = label.into();
    div()
        .id(id.into())
        .flex_none()
        .px_2()
        .py_1()
        .text_sm()
        .text_color(rgb(foreground_color()))
        .when(primary, |style| style.bg(rgb(accent_color())))
        .when(!primary, |style| style.hover(|style| style.bg(rgb(button_hover_color()))))
        .when(primary, |style| style.hover(|style| style.bg(rgb(accent_hover_color()))))
        .rounded_sm()
        .cursor_pointer()
        .child(label)
        .on_click(move |_, window, cx| callback(window, cx))
}

fn sidebar_button(
    label: impl Into<SharedString>,
    count: usize,
    id: impl Into<SharedString>,
    selected: bool,
    callback: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let label: SharedString = label.into();
    div()
        .id(id.into())
        .flex()
        .items_center()
        .justify_between()
        .w_full()
        .h(px(32.0))
        .px_2()
        .text_sm()
        .text_color(rgb(if selected { foreground_color() } else { muted_color() }))
        .when(selected, |style| style.bg(rgb(button_color())))
        .when(!selected, |style| style.hover(|style| style.bg(rgb(button_hover_color())).text_color(rgb(foreground_color()))))
        .rounded_sm()
        .cursor_pointer()
        .child(label)
        .child(div().text_xs().font_family("monospace").text_color(rgb(muted_color())).child(count.to_string()))
        .on_click(move |_, window, cx| callback(window, cx))
}

fn row_action_button(
    label: impl Into<SharedString>,
    id: impl Into<SharedString>,
    callback: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let label: SharedString = label.into();
    div()
        .id(id.into())
        .flex_none()
        .px_2()
        .py_1()
        .text_xs()
        .text_color(rgb(muted_color()))
        .hover(|style| style.bg(rgb(button_hover_color())).text_color(rgb(foreground_color())))
        .rounded_sm()
        .cursor_pointer()
        .child(label)
        .on_click(move |_, window, cx| callback(window, cx))
}

fn light_theme() -> bool {
    model::ui_pref_str("theme", "system") == "light"
}

fn background_color() -> u32 { if light_theme() { 0xf4f5f8 } else { 0x11131b } }
fn foreground_color() -> u32 { if light_theme() { 0x202331 } else { 0xe6e8ef } }
fn card_color() -> u32 { if light_theme() { 0xffffff } else { 0x1a1d29 } }
fn muted_color() -> u32 { if light_theme() { 0x626b7a } else { 0x9aa2b8 } }
fn button_color() -> u32 { if light_theme() { 0xe5e8ef } else { 0x292d3d } }
fn button_hover_color() -> u32 { if light_theme() { 0xd6dbe5 } else { 0x373c50 } }
fn accent_color() -> u32 { if light_theme() { 0x6652bd } else { 0x4b3b88 } }
fn accent_hover_color() -> u32 { if light_theme() { 0x5844aa } else { 0x5a4a98 } }

fn settings_row(
    title: impl Into<SharedString>,
    detail: String,
    button: impl Into<SharedString>,
    id: impl Into<SharedString>,
    action: impl Fn(&mut Window) + 'static,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .w_full()
        .p_3()
        .bg(rgb(card_color()))
        .rounded_md()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .flex_1()
                .child(title.into())
                .child(div().text_sm().text_color(rgb(muted_color())).child(detail)),
        )
        .child(action_button(button, id, move |window, _cx| action(window)))
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = *self.pane.lock().unwrap_or_else(|error| error.into_inner());
        let mut rows = div().flex().flex_col().gap_2().w_full();
        match pane {
            SettingsPane::Appearance => {
                let theme = model::ui_pref_str("theme", "system");
                let next_theme = match theme.as_str() {
                    "system" => "dark",
                    "dark" | "black" => "light",
                    _ => "system",
                };
                rows = rows.child(settings_row(
                    "Color theme",
                    format!("Current: {theme}"),
                    format!("Use {next_theme}"),
                    "settings-theme",
                    move |_window| model::ui_pref_set_str("theme", next_theme),
                ));
            }
            SettingsPane::Downloads => {
                let save_dir = model::ui_pref_str(
                    "save_dir",
                    &model::default_download_dir().to_string_lossy(),
                );
                rows = rows.child(settings_row(
                    "Download folder",
                    save_dir,
                    "Choose folder",
                    "settings-folder",
                    move |window| {
                        if let Some(path) = rfd::FileDialog::new().set_directory(&model::ui_pref_str("save_dir", "")).pick_folder() {
                            model::ui_pref_set_str("save_dir", &path.to_string_lossy());
                            window.refresh();
                        }
                    },
                ));
                let connections = model::ui_pref_u64("connections_default", 8).clamp(1, 32);
                rows = rows.child(settings_row(
                    "Connections per download",
                    "Higher values can improve speed on some servers.".into(),
                    format!("{connections} · Change"),
                    "settings-connections",
                    move |_window| {
                        let next = match connections { 1 => 4, 4 => 8, 8 => 16, 16 => 32, _ => 1 };
                        model::ui_pref_set_u64("connections_default", next);
                    },
                ));
                let retries = model::ui_pref_u64("max_retries", 5).clamp(1, 20);
                rows = rows.child(settings_row(
                    "Retry attempts",
                    "Maximum automatic retries for a failed transfer.".into(),
                    format!("{retries} · Change"),
                    "settings-retries",
                    move |_window| model::ui_pref_set_u64("max_retries", if retries >= 10 { 1 } else { retries + 1 }),
                ));
                let timeout = model::ui_pref_u64("timeout_secs", 60).clamp(5, 600);
                rows = rows.child(settings_row(
                    "Network timeout",
                    "How long a transfer can wait for a network response.".into(),
                    format!("{timeout} seconds · Change"),
                    "settings-timeout",
                    move |_window| {
                        let next = match timeout { 30 => 60, 60 => 120, 120 => 300, _ => 30 };
                        model::ui_pref_set_u64("timeout_secs", next);
                    },
                ));
                let limit = model::ui_pref_u64("speed_limit_kbps", 0);
                rows = rows.child(settings_row(
                    "Speed limit",
                    "Limit the combined download speed. Zero disables the limit.".into(),
                    if limit == 0 { "Off · Change".to_string() } else { format!("{} KB/s · Change", limit) },
                    "settings-speed-limit",
                    move |_window| {
                        let next = match limit { 0 => 512, 512 => 1024, 1024 => 2048, _ => 0 };
                        model::ui_pref_set_u64("speed_limit_kbps", next);
                    },
                ));
                let folders = model::ui_pref_bool("category_folders", true);
                rows = rows.child(settings_row(
                    "Organize by file type",
                    "Save downloads in category folders inside the selected folder.".into(),
                    if folders { "On" } else { "Off" },
                    "settings-category-folders",
                    move |window| Self::toggle(window, "category_folders", true),
                ));
            }
            SettingsPane::Browser => {
                for (title, key, detail, default) in [
                    ("Capture browser downloads", "capture_auto", "Send eligible browser downloads to Download Craft.", true),
                    ("Browser fallback", "browser_fallback", "Use the browser when Download Craft cannot fetch a media link.", true),
                    ("Notify when a browser capture finishes", "notify_done", "Show a browser notification after captured downloads finish.", true),
                ] {
                    let enabled = model::ui_pref_bool(key, default);
                    rows = rows.child(settings_row(
                        title,
                        detail.into(),
                        if enabled { "On" } else { "Off" },
                        match key { "capture_auto" => "settings-capture", "browser_fallback" => "settings-fallback", _ => "settings-notify" },
                        move |window| Self::toggle(window, key, default),
                    ));
                }
                rows = rows.child(div().text_sm().text_color(rgb(muted_color())).child(
                    "The browser extension remains JavaScript; the desktop app uses no Node.js runtime.",
                ));
            }
            SettingsPane::Plugins => {
                let yt_status = crate::engine::ytdlp_managed_path()
                    .filter(|path| crate::platform::executable_file(path))
                    .map(|path| format!("Managed copy · {}", path.display()))
                    .unwrap_or_else(|| "Managed copy not installed; check Setup for helper availability".into());
                let ffmpeg_status = crate::engine::ffmpeg_path()
                    .map(|path| format!("Available · {}", path.display()))
                    .unwrap_or_else(|| "Not installed or not found on PATH".into());
                rows = rows
                    .child(settings_row("yt-dlp", yt_status, "Project website", "settings-plugin-ytdlp", move |window| {
                        crate::platform::open_url("https://github.com/yt-dlp/yt-dlp/releases");
                        window.refresh();
                    }))
                    .child(settings_row("FFmpeg", ffmpeg_status, "Project website", "settings-plugin-ffmpeg", move |window| {
                        crate::platform::open_url("https://ffmpeg.org/download.html");
                        window.refresh();
                    }));
                let engine = model::ui_pref_str("youtube_engine", "auto");
                let next_engine = match engine.as_str() { "auto" => "native", "native" => "ytdlp", _ => "auto" };
                let engine_label = match engine.as_str() { "native" => "App first", "ytdlp" => "yt-dlp first", _ => "Auto" };
                rows = rows.child(settings_row(
                    "YouTube download engine",
                    "The selected engine gets a fallback attempt when it cannot fetch the video.".into(),
                    format!("{engine_label} · Change"),
                    "settings-youtube-engine",
                    move |_window| model::ui_pref_set_str("youtube_engine", next_engine),
                ));
                let subtitles = model::ui_pref_bool("subtitles", false);
                rows = rows.child(settings_row(
                    "Save subtitles",
                    "Save available subtitles beside the video when yt-dlp is used.".into(),
                    if subtitles { "On" } else { "Off" },
                    "settings-subtitles",
                    move |window| Self::toggle(window, "subtitles", false),
                ));
            }
            SettingsPane::Proxy => {
                let proxy = model::ui_pref_str("proxy_url", "");
                let proxy_draft = self.proxy_draft.clone();
                let proxy_focus = self.proxy_focus.clone();
                let proxy_notice = self.proxy_notice.clone();
                let input_proxy_draft = proxy_draft.clone();
                let input_proxy_notice = proxy_notice.clone();
                let draft_value = proxy_draft.lock().unwrap_or_else(|error| error.into_inner()).clone();
                let mode = match crate::engine::proxy_choice(&proxy) {
                    crate::engine::ProxyChoice::System => "System settings",
                    crate::engine::ProxyChoice::Direct => "Direct connection",
                    crate::engine::ProxyChoice::Pac(_) => "PAC configuration",
                    crate::engine::ProxyChoice::Manual(_) => "Manual proxy",
                };
                let next_mode = match crate::engine::proxy_choice(&proxy) {
                    crate::engine::ProxyChoice::System => "direct",
                    crate::engine::ProxyChoice::Direct
                    | crate::engine::ProxyChoice::Pac(_)
                    | crate::engine::ProxyChoice::Manual(_) => "",
                };
                let action = match crate::engine::proxy_choice(&proxy) {
                    crate::engine::ProxyChoice::System => "Use direct connection",
                    crate::engine::ProxyChoice::Direct => "Use system settings",
                    crate::engine::ProxyChoice::Pac(_) | crate::engine::ProxyChoice::Manual(_) => "Use system settings",
                };
                let uri_input = div()
                    .id("settings-proxy-uri")
                    .track_focus(&proxy_focus)
                    .w_full()
                    .p_3()
                    .bg(rgb(card_color()))
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if proxy_focus.is_focused(_window) { accent_color() } else { button_color() }))
                    .child(if draft_value.is_empty() {
                        "Proxy URL: http://host:port, socks5h://host:port, or pac:https://host/proxy.pac".to_string()
                    } else {
                        draft_value
                    })
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |_this, _, window, _| proxy_focus.focus(window)))
                    .on_key_down(cx.listener(move |_this, event: &KeyDownEvent, window, cx| {
                        let key = event.keystroke.key.as_str();
                        let modifiers = &event.keystroke.modifiers;
                        let paste = (modifiers.control || modifiers.platform) && key.eq_ignore_ascii_case("v");
                        let mut pasted = None;
                        if paste {
                            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                pasted = clipboard.get_text().ok();
                            }
                        }
                        if let Ok(mut draft) = input_proxy_draft.lock() {
                            if key == "backspace" {
                                draft.pop();
                            } else if paste {
                                if let Some(value) = pasted {
                                    *draft = value.trim().to_string();
                                }
                            } else if !modifiers.control && !modifiers.platform && !modifiers.alt {
                                if let Some(ch) = &event.keystroke.key_char {
                                    if !ch.chars().any(char::is_control) {
                                        draft.push_str(ch);
                                    }
                                }
                            }
                        }
                        let _ = input_proxy_notice.lock().map(|mut notice| notice.clear());
                        cx.notify();
                        window.refresh();
                    }));
                let proxy_notice_text = proxy_notice.lock().unwrap_or_else(|error| error.into_inner()).clone();
                let save_draft = proxy_draft.clone();
                let save_notice = proxy_notice.clone();
                let proxy_save = action_button("Save proxy URL", "settings-proxy-save", move |window, _cx| {
                    let value = save_draft.lock().unwrap_or_else(|error| error.into_inner()).trim().to_string();
                    let parsed = value.trim() == "direct" || if let Some(pac_url) = value.strip_prefix("pac:") {
                        url::Url::parse(pac_url).ok().filter(|parsed| matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some())
                        .is_some()
                    } else {
                        url::Url::parse(&value).ok().filter(|parsed| matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h") && parsed.host_str().is_some())
                        .is_some()
                    };
                    let mut notice = save_notice.lock().unwrap_or_else(|error| error.into_inner());
                    if value.is_empty() {
                        model::ui_pref_set_str("proxy_url", "");
                        *notice = "Using system proxy settings".into();
                    } else if parsed {
                        model::ui_pref_set_str("proxy_url", &value);
                        *notice = "Proxy setting saved".into();
                    } else {
                        *notice = "Enter a valid HTTP, HTTPS, SOCKS5, SOCKS5H, or PAC URL".into();
                    }
                    window.refresh();
                });
                let mode_draft = proxy_draft.clone();
                rows = rows
                    .child(settings_row(
                        "Connection mode",
                        format!("Current: {mode}. System settings use the operating system proxy configuration."),
                        action,
                        "settings-proxy-mode",
                        move |_window| {
                            model::ui_pref_set_str("proxy_url", next_mode);
                            *mode_draft.lock().unwrap_or_else(|error| error.into_inner()) = next_mode.to_string();
                        },
                    ))
                    .child(div().flex().flex_col().gap_2().w_full().child(uri_input).child(proxy_save))
                    .child(div().text_sm().text_color(rgb(muted_color())).child(match crate::engine::proxy_choice(&proxy) {
                        crate::engine::ProxyChoice::Manual(_) => "A manual proxy is configured. Its credentials are hidden here.".to_string(),
                        crate::engine::ProxyChoice::Pac(_) => "A PAC script is configured.".to_string(),
                        crate::engine::ProxyChoice::Direct => "Downloads connect directly.".to_string(),
                        crate::engine::ProxyChoice::System => "Downloads use system proxy settings.".to_string(),
                    }));
                if !proxy_notice_text.is_empty() {
                    rows = rows.child(div().text_sm().text_color(rgb(muted_color())).child(proxy_notice_text));
                }
            }
            SettingsPane::Categories => {
                let template = model::ui_pref_str("rename_template", "{name}");
                let next_template = if template.trim() == "{name}" { "{name}_{date}" } else { "{name}" };
                rows = rows.child(settings_row(
                    "Filename template",
                    format!("Current: {template}. Tokens: {{name}}, {{ext}}, {{date}}, {{host}}."),
                    "Cycle preset",
                    "settings-rename-template",
                    move |_window| model::ui_pref_set_str("rename_template", next_template),
                ));
                let mut rules = model::ui_pref_value("folder_rules")
                    .and_then(|value| value.as_array().cloned())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|value| serde_json::from_value::<crate::model::FolderRule>(value).ok())
                    .take(8)
                    .collect::<Vec<_>>();
                if rules.is_empty() {
                    rules = model::default_folder_rules();
                }
                rows = rows.child(div().font_weight(gpui::FontWeight::BOLD).child("Type to folder rules · first match wins"));
                for (index, rule) in rules.iter().enumerate() {
                    let extensions = rule.exts.clone();
                    let folder = rule.folder.clone();
                    let mut updated = rules.clone();
                    updated.remove(index);
                    rows = rows.child(settings_row(
                        format!("{extensions} → {folder}"),
                        "Matching downloads are saved in this category folder.".into(),
                        "Remove",
                        format!("settings-rule-remove-{index}"),
                        move |_window| {
                            model::ui_pref_set_value("folder_rules", serde_json::to_value(&updated).unwrap_or_default());
                        },
                    ));
                }
                let defaults = crate::model::default_folder_rules();
                rows = rows.child(settings_row(
                    "Folder rules",
                    format!("{} rules configured; at most 8 are applied.", rules.len()),
                    "Restore defaults",
                    "settings-rules-defaults",
                    move |_window| model::ui_pref_set_value("folder_rules", serde_json::to_value(&defaults).unwrap_or_default()),
                ));
            }
            SettingsPane::Sounds => {
                for (event, label, default) in [
                    ("complete", "Download complete", true),
                    ("failed", "Download failed", true),
                    ("queue_start", "Queue processing started", false),
                    ("queue_done", "Queue processing finished", false),
                ] {
                    let key = format!("sound_{event}");
                    let fallback = if event == "complete" || event == "failed" {
                        model::ui_pref_bool("sounds", default)
                    } else {
                        default
                    };
                    let enabled = model::ui_pref_bool(&key, fallback);
                    let detail = model::ui_pref_str(&format!("sound_{event}_file"), "");
                    rows = rows.child(settings_row(
                        label,
                        if detail.is_empty() { "Use the system sound.".into() } else { format!("Sound file: {detail}") },
                        if enabled { "On" } else { "Off" },
                        format!("settings-sound-{event}"),
                        move |window| {
                            model::ui_pref_set_bool(&key, !enabled);
                            if event == "complete" || event == "failed" {
                                let other = if event == "complete" { "sound_failed" } else { "sound_complete" };
                                let legacy = !enabled || model::ui_pref_bool(other, true);
                                model::ui_pref_set_bool("sounds", legacy);
                            }
                            window.refresh();
                        },
                    ));
                }
            }
            SettingsPane::Updates => {
                for (title, key, detail, default) in [
                    ("Check for updates", "update_check_on", "Check for a newer release when the app starts.", true),
                    ("Install updates automatically", "update_auto", "Download available updates without asking first.", true),
                ] {
                    let enabled = model::ui_pref_bool(key, default);
                    rows = rows.child(settings_row(
                        title,
                        detail.into(),
                        if enabled { "On" } else { "Off" },
                        if key == "update_check_on" { "settings-update-check" } else { "settings-update-auto" },
                        move |window| Self::toggle(window, key, default),
                    ));
                }
            }
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .bg(rgb(background_color()))
            .text_color(rgb(foreground_color()))
            .p_5()
            .child(div().text_2xl().font_weight(gpui::FontWeight::BOLD).child("Settings"))
            .child(div().flex().gap_2().children(SettingsPane::ALL.into_iter().map(|entry| {
                let selected = entry == pane;
                let pane = self.pane.clone();
                action_button(entry.label(), format!("settings-pane-{}", entry.label()), move |window, _cx| {
                    *pane.lock().unwrap_or_else(|error| error.into_inner()) = entry;
                    window.refresh();
                })
                .when(selected, |button| button.bg(rgb(accent_color())))
            })))
            .child(div().text_xl().font_weight(gpui::FontWeight::BOLD).child(pane.label()))
            .child(rows)
    }
}

impl Render for SetupView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let (states, busy, show_at_start, missing) = {
            let data = self.data.lock().unwrap_or_else(|error| error.into_inner());
            (data.states.clone(), data.busy, data.show_at_start, Self::missing_automatic(&data))
        };
        let ready = states.iter().filter(|(_, state)| state.ready()).count();
        let all = states.len();
        let data = self.data.clone();
        let mut rows = div().id("setup-state-list").flex().flex_col().gap_2().w_full().overflow_scroll();
        for (part, state) in states {
            let detail = state.detail();
            let ready = state.ready();
            let button = if ready {
                "Ready"
            } else if busy {
                "Working…"
            } else if matches!(state, crate::setup_service::State::Failed(_)) {
                "Try again"
            } else if part == crate::setup_service::Part::Connection {
                "Set up"
            } else if part.automatic() {
                "Install"
            } else {
                "Manual setup"
            };
            let mut row = div()
                .flex()
                .flex_col()
                .gap_1()
                .w_full()
                .p_3()
                .bg(rgb(card_color()))
                .rounded_md()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(div().font_weight(gpui::FontWeight::BOLD).child(part.title()))
                        .child(div().text_sm().text_color(rgb(if ready { 0x16834a } else { muted_color() })).child(button)),
                )
                .child(div().text_sm().text_color(rgb(muted_color())).child(part.purpose()))
                .child(div().text_sm().text_color(rgb(if matches!(state, crate::setup_service::State::Failed(_)) { 0xc0392b } else { muted_color() })).child(detail));

            if !ready && part.automatic() {
                let data = data.clone();
                let id = format!("setup-fix-{:?}", part);
                row = row.child(action_button(button, id, move |window, _cx| {
                    Self::start_fix(&data, vec![part]);
                    window.refresh();
                }));
            }
            if part == crate::setup_service::Part::Extension && !ready {
                row = row
                    .child(div().text_sm().child("1. Open chrome://extensions and enable Developer mode."))
                    .child(div().text_sm().child("2. Choose Load unpacked, then select the extension folder."))
                    .child(div().text_xs().text_color(rgb(muted_color())).child(
                        "Open the folder or copy its path to paste in the browser's Load unpacked dialog.",
                    ));
                let folder = crate::setup_service::extension_directory();
                let open_folder = folder.clone();
                row = row.child(div().flex().gap_2()
                    .child(action_button("Open extension folder", "setup-open-extension", move |window, _cx| {
                        if let Some(path) = &open_folder { crate::platform::open(path, false); }
                        window.refresh();
                    }))
                    .child(action_button("Copy folder path", "setup-copy-extension", move |window, _cx| {
                        if let Some(path) = &folder {
                            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                let _ = clipboard.set_text(path.to_string_lossy().to_string());
                            }
                        }
                        window.refresh();
                    })));
            }
            rows = rows.child(row);
        }

        let footer_data = data.clone();
        let fix_data = data.clone();
        let fix_label = if busy { "Working…" } else if missing.is_empty() { "Done" } else { "Fix everything" };
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .bg(rgb(background_color()))
            .text_color(rgb(foreground_color()))
            .p_4()
            .child(div().text_2xl().font_weight(gpui::FontWeight::BOLD).child("Get ready"))
            .child(div().text_sm().text_color(rgb(muted_color())).child(
                if ready == all { "Everything is ready.".to_string() } else { format!("{ready} of {all} ready — set up what is missing.") },
            ))
            .child(rows)
            .child(div().flex().items_center().justify_between().gap_2()
                .child(action_button(
                    if show_at_start { "Show at startup: On" } else { "Show at startup: Off" },
                    "setup-show-at-start",
                    move |window, _cx| {
                        let value = !model::ui_pref_bool("setup_at_start", false);
                        model::ui_pref_set_bool("setup_at_start", value);
                        if let Ok(mut data) = footer_data.lock() { data.show_at_start = value; }
                        window.refresh();
                    },
                ))
                .child(action_button(fix_label, "setup-fix-all", move |window, _cx| {
                    if missing.is_empty() && !busy {
                        window.remove_window();
                    } else {
                        Self::start_fix(&fix_data, missing.clone());
                        window.refresh();
                    }
                })))
    }
}

impl ProgressView {
    fn refresh(&mut self) {
        let on_log_tab = *self.tab.lock().unwrap_or_else(|error| error.into_inner()) == ProgressTab::Log;
        let include_log = on_log_tab && self.last_log_read.elapsed() >= Duration::from_secs(1);
        let previous_log = self.snapshot.log_lines.clone();
        let mut snapshot = crate::progress_service::gpui_snapshot(self.id, include_log);
        if include_log {
            self.last_log_read = Instant::now();
        } else if on_log_tab {
            snapshot.log_lines = previous_log;
        }
        self.snapshot = snapshot;
    }

    fn fraction(&self) -> f32 {
        let Some(item) = &self.snapshot.item else { return 0.0 };
        match (item.total_bytes, self.snapshot.fragments) {
            (Some(total), _) if total > 0 => (item.downloaded_bytes as f32 / total as f32).clamp(0.0, 1.0),
            (_, Some((index, count))) if count > 0 && self.snapshot.live => (index as f32 / count as f32).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }

    fn close(window: &mut Window, id: u64, snapshot: &crate::progress_service::GpuiProgressSnapshot) {
        if snapshot.item.as_ref().is_some_and(|item| matches!(&item.status, Status::Downloading | Status::Queued)) {
            crate::progress_service::gpui_command(id, "pause");
        }
        window.remove_window();
    }

    fn info_row(label: &'static str, value: String) -> impl IntoElement {
        div()
            .flex()
            .justify_between()
            .gap_3()
            .w_full()
            .px_2()
            .py_1()
            .child(div().text_sm().text_color(rgb(muted_color())).child(label))
            .child(div().text_sm().child(value))
    }
}

impl Render for ProgressView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.snapshot.clone();
        let id = self.id;
        let tab = *self.tab.lock().unwrap_or_else(|error| error.into_inner());
        let fraction = self.fraction();
        let item = snapshot.item.clone();
        let title = item.as_ref().map(|item| item.filename.clone()).unwrap_or_else(|| format!("Download #{id}"));
        let status = item.as_ref().map(|item| {
            if snapshot.live && !snapshot.phase.is_empty() && snapshot.phase != "Downloading" && item.downloaded_bytes == 0 {
                format!("Downloading — {}", snapshot.phase)
            } else {
                item.status.label()
            }
        }).unwrap_or_else(|| "No longer in the queue".into());
        let mut body = div().id("settings-browser-body").flex().flex_col().gap_1().w_full().overflow_scroll();

        if let Some(item) = &item {
            match tab {
                ProgressTab::Info => {
                    body = body
                        .child(Self::info_row("Status", status.clone()))
                        .child(Self::info_row("Size", item.total_bytes.map(model::format_bytes).unwrap_or_else(|| "Unknown".into())))
                        .child(Self::info_row("Downloaded", model::format_bytes(item.downloaded_bytes)))
                        .child(Self::info_row("Speed", if item.speed_bps > 0.0 { model::format_speed(item.speed_bps) } else { "—".into() }))
                        .child(Self::info_row("Resolution", if !snapshot.resolution.is_empty() { snapshot.resolution.clone() } else { item.resolution.clone() }))
                        .child(Self::info_row("Resume support", if item.resumable || snapshot.resumable { "Yes".into() } else { "No".into() }));
                    let eta = model::format_eta(item);
                    if eta != "-" {
                        body = body.child(Self::info_row("Remaining time", eta));
                    }
                    if let Status::Failed(error) = &item.status {
                        body = body.child(div().p_2().text_sm().text_color(rgb(0xc0392b)).child(error.clone()));
                    }
                }
                ProgressTab::Speed => {
                    let active_parts = snapshot.parts.iter().filter(|(_, end, done)| done < end).count();
                    body = body
                        .child(Self::info_row("Connections", format!("up to {}", item.connections)))
                        .child(Self::info_row("Active parts", active_parts.to_string()))
                        .child(Self::info_row("Current speed", if item.speed_bps > 0.0 { model::format_speed(item.speed_bps) } else { "—".into() }))
                        .child(Self::info_row("Transferred", model::format_bytes(item.downloaded_bytes)));
                }
                ProgressTab::Settings => {
                    let (limit, finish) = crate::progress_service::gpui_settings();
                    let next_limit = match limit { 0 => 512, 512 => 1024, 1024 => 2048, _ => 0 };
                    let next_finish = match finish.as_str() { "nothing" => "shutdown", "shutdown" => "sleep", "sleep" => "hibernate", _ => "nothing" };
                    body = body
                        .child(div().text_sm().text_color(rgb(muted_color())).child("Changes use the shared app settings and command queue."))
                        .child(action_button(
                            if limit == 0 { "Speed limit: Off".to_string() } else { format!("Speed limit: {} KB/s", limit) },
                            "progress-speed-limit",
                            move |window, _cx| {
                                crate::progress_service::gpui_set_speed_limit(next_limit);
                                window.refresh();
                            },
                        ))
                        .child(action_button(
                            format!("When queue finishes: {finish}"),
                            "progress-finish-action",
                            move |window, _cx| {
                                crate::progress_service::gpui_set_finish_action(next_finish);
                                window.refresh();
                            },
                        ));
                }
                ProgressTab::Log => {
                    body = body.children(snapshot.log_lines.iter().cloned().map(|line| {
                        div().w_full().text_xs().child(line)
                    }));
                }
            }
        } else {
            body = body.child(div().text_sm().text_color(rgb(muted_color())).child("This download was removed from the queue."));
        }

        let mut parts = div().flex().flex_col().gap_1().w_full();
        for (index, (start, end, done)) in snapshot.parts.iter().take(12).enumerate() {
            let total = end.saturating_sub(*start);
            parts = parts.child(div().flex().justify_between().w_full().px_2().py_1()
                .bg(rgb(card_color()))
                .child(div().text_xs().child(format!("Part {}", index + 1)))
                .child(div().text_xs().text_color(rgb(muted_color())).child(format!("{} / {}", model::format_bytes(*done), model::format_bytes(total)))));
        }
        if let Some((index, count)) = snapshot.fragments {
            parts = parts.child(Self::info_row("Stream fragments", format!("{index} / {count}")));
        }

        let close_snapshot = snapshot.clone();
        let mut actions = div().flex().gap_2().w_full();
        if let Some(item) = &item {
            match &item.status {
                Status::Completed => {
                    let path = item.dest_path();
                    let folder = path.parent().map(PathBuf::from);
                    actions = actions.child(action_button("Open file", "progress-open-file", move |window, _cx| {
                        crate::platform::open(&path, false);
                        window.refresh();
                    }));
                    if let Some(folder) = folder {
                        actions = actions.child(action_button("Open folder", "progress-open-folder", move |window, _cx| {
                            crate::platform::open(&folder, true);
                            window.refresh();
                        }));
                    }
                }
                Status::Downloading => {
                    actions = actions
                        .child(action_button("Pause", "progress-pause", move |window, _cx| {
                            crate::progress_service::gpui_command(id, "pause");
                            window.refresh();
                        }))
                        .child(action_button("Cancel", "progress-cancel", move |window, _cx| {
                            crate::progress_service::gpui_command(id, "cancel");
                            window.refresh();
                        }));
                }
                Status::Queued | Status::Paused | Status::Failed(_) => {
                    actions = actions.child(action_button("Resume", "progress-resume", move |window, _cx| {
                        crate::progress_service::gpui_command(id, "resume");
                        window.refresh();
                    }));
                }
            }
        }
        actions = actions.child(action_button("Close", "progress-close", move |window, _cx| {
            Self::close(window, id, &close_snapshot);
        }));

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(rgb(background_color()))
            .text_color(rgb(foreground_color()))
            .child(div().font_weight(gpui::FontWeight::BOLD).child(title))
            .child(div().text_sm().text_color(rgb(muted_color())).child(status))
            .child(div().flex().gap_2().children(ProgressTab::ALL.into_iter().map(|tab| {
                let selected = tab == *self.tab.lock().unwrap_or_else(|error| error.into_inner());
                let current_tab = self.tab.clone();
                action_button(tab.label(), format!("progress-tab-{}", tab.label()), move |window, _cx| {
                    *current_tab.lock().unwrap_or_else(|error| error.into_inner()) = tab;
                    window.refresh();
                })
                .when(selected, |button| button.bg(rgb(accent_color())))
            })))
            .child(body)
            .child(div().flex().flex_col().gap_1().child(div().h(px(10.0)).w_full().bg(rgb(button_color())).rounded_sm().child(
                div().h_full().w(px(420.0 * fraction)).bg(rgb(accent_color())).rounded_sm(),
            )).child(div().text_xs().text_color(rgb(muted_color())).child(format!("{:.0}%", fraction * 100.0))))
            .child(parts)
            .child(actions)
    }
}

impl Render for CompleteView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let item = self.item.clone();
        let path = item.dest_path();
        let folder = path.parent().map(PathBuf::from);
        let dont_show = *self.dont_show.lock().unwrap_or_else(|error| error.into_inner());
        let checkbox = self.dont_show.clone();
        let summary = if item.resolution.is_empty() {
            format!("Downloaded {} ({} bytes)", model::format_bytes(item.downloaded_bytes), item.downloaded_bytes)
        } else {
            format!("Downloaded {} ({} bytes) · {}", model::format_bytes(item.downloaded_bytes), item.downloaded_bytes, item.resolution)
        };
        let mut actions = div().flex().gap_2().w_full()
            .child(action_button("Open", "complete-open", move |window, _cx| {
                crate::platform::open(&path, false);
                window.remove_window();
            }));
        #[cfg(windows)]
        {
            let open_with_path = item.dest_path();
            actions = actions.child(action_button("Open with…", "complete-open-with", move |window, _cx| {
                crate::complete_service::gpui_open_with(&open_with_path);
                window.remove_window();
            }));
        }
        if let Some(folder) = folder {
            actions = actions.child(action_button("Open folder", "complete-open-folder", move |window, _cx| {
                crate::platform::open(&folder, true);
                window.remove_window();
            }));
        }
        actions = actions.child(action_button("Close", "complete-close", move |window, _cx| {
            window.remove_window();
        }));

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(rgb(background_color()))
            .text_color(rgb(foreground_color()))
            .child(div().font_weight(gpui::FontWeight::BOLD).child("Download complete"))
            .child(div().text_sm().text_color(rgb(muted_color())).child(summary))
            .child(div().flex().flex_col().gap_1()
                .child(div().text_xs().text_color(rgb(muted_color())).child("Address"))
                .child(div().w_full().p_2().bg(rgb(card_color())).rounded_sm().text_sm().child(item.url.clone())))
            .child(div().flex().flex_col().gap_1()
                .child(div().text_xs().text_color(rgb(muted_color())).child("The file saved as"))
                .child(div().w_full().p_2().bg(rgb(card_color())).rounded_sm().text_sm().child(item.dest_path().to_string_lossy().to_string())))
            .child(actions)
            .child(action_button(
                if dont_show { "✓  Don't show this dialog again" } else { "□  Don't show this dialog again" },
                "complete-dont-show",
                move |window, _cx| {
                    let mut selected = checkbox.lock().unwrap_or_else(|error| error.into_inner());
                    let value = !*selected;
                    *selected = value;
                    drop(selected);
                    model::ui_pref_set_bool("show_complete", !value);
                    window.refresh();
                },
            ))
    }
}

pub fn run(runtime: Arc<tokio::runtime::Runtime>) {
    model::touch_heartbeat();
    let app_runtime = runtime.clone();
    Application::new().run(move |cx: &mut App| {
        initialize_gpui_app(cx);
        #[cfg(windows)]
        let tray_icon = crate::tray::build_gpui_tray();
        let manager = cx.new(|_| DownloadManagerStore::load(app_runtime.clone()));
        let bounds = Bounds::centered(None, size(px(960.0), px(620.0)), cx);
        let view_manager = manager.clone();
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from(model::APP_NAME)), ..Default::default() }),
                    ..Default::default()
                },
                move |window, cx| {
                    window.on_window_should_close(cx, |_, _| crate::tray::close_main_window());
                    #[cfg(windows)]
                    if let Ok(handle) = raw_window_handle::HasWindowHandle::window_handle(window) {
                        if let raw_window_handle::RawWindowHandle::Win32(handle) = handle.as_raw() {
                            crate::tray::set_hwnd(handle.hwnd.get());
                        }
                    }
                    cx.new(move |cx| {
                    MainView::observe_manager(&view_manager, cx);
                    MainView {
                        manager: view_manager,
                        grab_focus: cx.focus_handle(),
                        site_rule_focus: cx.focus_handle(),
                        search_focus: cx.focus_handle(),
                        capture_focus: cx.focus_handle(),
                        capture_window: None,
                        capture_window_key: None,
                        last_window_prune: Instant::now(),
                        last_links_scan: Instant::now(),
                        #[cfg(windows)]
                        _tray_icon: tray_icon,
                    }
                    })
                },
            )
            .expect("open Download Craft GPUI window");

        let refresh_manager = manager.clone();
        cx.spawn(async move |cx| {
            let mut last_heartbeat = Instant::now();
            loop {
                Timer::after(Duration::from_millis(100)).await;
                if last_heartbeat.elapsed() >= Duration::from_secs(2) {
                    model::touch_heartbeat();
                    last_heartbeat = Instant::now();
                }
                if window.update(cx, |view, native_window, cx| {
                    view.tick_capture_dialog(native_window, cx);
                }).is_err() {
                    break;
                }
                if crate::tray::take_quit() {
                    let _ = cx.update(|cx| cx.quit());
                    break;
                }
                if crate::tray::take_show_request() {
                    crate::tray::show();
                    let _ = window.update(cx, |_, window, _| window.activate_window());
                }
                let _ = refresh_manager.update(cx, |manager, cx| {
                    if manager.refresh() {
                        cx.notify();
                    }
                });
            }
        })
        .detach();
        cx.activate(true);
    });
    model::clear_heartbeat();
    match Arc::try_unwrap(runtime) {
        Ok(runtime) => runtime.shutdown_timeout(Duration::from_millis(200)),
        Err(runtime) => {
            let _ = std::thread::spawn(move || drop(runtime));
        }
    }
}

pub fn run_about() {
    let Some(_instance) = model::claim_window("about") else {
        return;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::setup_service::helper_lines());
    });
    let initial = AboutView {
        snapshot: crate::about_service::gpui_snapshot(),
        helpers: Vec::new(),
        helpers_rx: Some(rx),
        auto_updates: model::ui_pref_bool("update_auto", true),
        window_manager: None,
    };
    Application::new().run(move |cx: &mut App| {
        initialize_gpui_app(cx);
        let bounds = Bounds::centered(None, size(px(560.0), px(700.0)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from("About Download Craft")), ..Default::default() }),
                    ..Default::default()
                },
                move |_, cx| cx.new(|_| initial),
            )
            .expect("open Download Craft About window");
        cx.spawn(async move |cx| loop {
            Timer::after(Duration::from_millis(400)).await;
            if window
                .update(cx, |view, native_window, cx| {
                    view.refresh();
                    cx.notify();
                    native_window.refresh();
                })
                .is_err()
            {
                break;
            }
        })
        .detach();
        cx.activate(true);
    });
}

pub fn run_settings() {
    let args = std::env::args().collect::<Vec<_>>();
    let requested_pane = args
        .iter()
        .position(|arg| arg == "--pane")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<usize>().ok());
    let pane = match requested_pane {
        None | Some(0) => SettingsPane::Appearance,
        Some(1) => SettingsPane::Downloads,
        Some(2) => SettingsPane::Plugins,
        Some(3) => SettingsPane::Browser,
        Some(4) => SettingsPane::Proxy,
        Some(5) => SettingsPane::Categories,
        Some(6) => SettingsPane::Sounds,
        Some(7) => SettingsPane::Updates,
        // Unsupported/obsolete pane indices land on the primary settings
        // page. Keep every Settings launch in the GPUI frontend.
        Some(_) => SettingsPane::Appearance,
    };
    let Some(_instance) = model::claim_window("settings") else {
        return;
    };
    let initial_proxy = model::ui_pref_str("proxy_url", "");
    Application::new().run(move |cx: &mut App| {
        initialize_gpui_app(cx);
        let bounds = Bounds::centered(None, size(px(720.0), px(620.0)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from("Download Craft Settings")), ..Default::default() }),
                    ..Default::default()
                },
                move |_, cx| {
                    let initial_proxy = initial_proxy.clone();
                    cx.new(|cx| SettingsView {
                        pane: Arc::new(Mutex::new(pane)),
                        proxy_focus: cx.focus_handle(),
                        proxy_draft: Arc::new(Mutex::new(initial_proxy)),
                        proxy_notice: Arc::new(Mutex::new(String::new())),
                    })
                },
            )
            .expect("open Download Craft Settings GPUI window");
        cx.spawn(async move |cx| loop {
            Timer::after(Duration::from_millis(500)).await;
            if window
                .update(cx, |_view, native_window, cx| {
                    cx.notify();
                    native_window.refresh();
                })
                .is_err()
            {
                break;
            }
        })
        .detach();
        cx.activate(true);
    });
}

pub fn run_setup() {
    let Some(_instance) = model::claim_window("setup") else {
        return;
    };
    let auto = std::env::args().any(|argument| argument == "--auto");
    let (tx, rx) = std::sync::mpsc::channel();
    let data = Arc::new(Mutex::new(SetupData {
        states: crate::setup_service::Part::ALL
            .into_iter()
            .map(|part| (part, crate::setup_service::State::Checking))
            .collect(),
        tx: tx.clone(),
        rx,
        busy: true,
        show_at_start: model::ui_pref_bool("setup_at_start", false),
        auto_fix_pending: auto,
        last_extension_check: Instant::now(),
    }));
    crate::setup_service::run_checks(tx, crate::setup_service::Part::ALL.to_vec());

    Application::new().run(move |cx: &mut App| {
        initialize_gpui_app(cx);
        let bounds = Bounds::centered(None, size(px(600.0), px(700.0)), cx);
        let data = data.clone();
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from("Get ready — Download Craft")), ..Default::default() }),
                    ..Default::default()
                },
                move |_, cx| cx.new(|_| SetupView { data }),
            )
            .expect("open Download Craft setup GPUI window");
        cx.spawn(async move |cx| loop {
            Timer::after(Duration::from_millis(300)).await;
            if window
                .update(cx, |view, native_window, cx| {
                    view.poll();
                    cx.notify();
                    native_window.refresh();
                })
                .is_err()
            {
                break;
            }
        })
        .detach();
        cx.activate(true);
    });
}

pub fn run_progress() {
    let args = std::env::args().collect::<Vec<_>>();
    let id = args
        .iter()
        .position(|argument| argument == "--progress")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_else(|| {
            eprintln!("--progress needs <id>");
            std::process::exit(2);
        });
    let Some(_instance) = model::claim_window(&format!("progress_{id}")) else {
        return;
    };
    let view = ProgressView {
        id,
        snapshot: crate::progress_service::gpui_snapshot(id, false),
        tab: Arc::new(Mutex::new(ProgressTab::Info)),
        last_log_read: Instant::now(),
    };
    Application::new().run(move |cx: &mut App| {
        initialize_gpui_app(cx);
        let bounds = Bounds::centered(None, size(px(540.0), px(620.0)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from(format!("Download Craft · #{id}"))), ..Default::default() }),
                    ..Default::default()
                },
                move |_, cx| cx.new(|_| view),
            )
            .expect("open Download Craft Progress GPUI window");
        cx.spawn(async move |cx| loop {
            Timer::after(Duration::from_millis(250)).await;
            if window
                .update(cx, |view, native_window, cx| {
                    view.refresh();
                    let handoff = view.snapshot.item.as_ref().is_some_and(|item| {
                        !view.snapshot.live
                            && matches!(&item.status, Status::Completed)
                            && model::ui_pref_bool("show_complete", true)
                    });
                    if handoff {
                        native_window.remove_window();
                        return;
                    }
                    cx.notify();
                    native_window.refresh();
                })
                .is_err()
            {
                let snapshot = crate::progress_service::gpui_snapshot(id, false);
                if snapshot.item.as_ref().is_some_and(|item| matches!(&item.status, Status::Downloading | Status::Queued)) {
                    crate::progress_service::gpui_command(id, "pause");
                }
                break;
            }
        })
        .detach();
        cx.activate(true);
    });
}

pub fn run_complete() {
    let args = std::env::args().collect::<Vec<_>>();
    let id = args
        .iter()
        .position(|argument| argument == "--complete")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_else(|| {
            eprintln!("--complete needs <id>");
            std::process::exit(2);
        });
    let Some(_instance) = model::claim_window(&format!("complete_{id}")) else {
        return;
    };
    let Some(item) = crate::complete_service::gpui_item(id) else {
        return;
    };
    let view = CompleteView {
        item,
        dont_show: Arc::new(Mutex::new(!model::ui_pref_bool("show_complete", true))),
    };
    Application::new().run(move |cx: &mut App| {
        initialize_gpui_app(cx);
        let bounds = Bounds::centered(None, size(px(560.0), px(360.0)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from("Download complete")), ..Default::default() }),
                    ..Default::default()
                },
                move |_, cx| cx.new(|_| view),
            )
            .expect("open Download Craft Completion GPUI window");
        cx.spawn(async move |cx| loop {
            Timer::after(Duration::from_millis(500)).await;
            if window
                .update(cx, |_view, native_window, cx| {
                    cx.notify();
                    native_window.refresh();
                })
                .is_err()
            {
                break;
            }
        })
        .detach();
        cx.activate(true);
    });
}

pub fn run_mini() {
    let Some(_instance) = model::claim_window("mini") else {
        return;
    };
    let initial = MiniView {
        snapshot: crate::basket_service::mini_snapshot(),
    };
    Application::new().run(move |cx: &mut App| {
        initialize_gpui_app(cx);
        let bounds = Bounds::centered(None, size(px(380.0), px(250.0)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from("Download Craft Mini")), ..Default::default() }),
                    ..Default::default()
                },
                move |_, cx| cx.new(|_| initial),
            )
            .expect("open Download Craft Mini window");
        cx.spawn(async move |cx| loop {
            Timer::after(Duration::from_secs(1)).await;
            if window
                .update(cx, |view, native_window, cx| {
                    view.refresh();
                    cx.notify();
                    native_window.refresh();
                })
                .is_err()
            {
                break;
            }
        })
        .detach();
        cx.activate(true);
    });
}

pub fn run_basket() {
    let Some(_instance) = model::claim_window("basket") else {
        return;
    };
    let state = Arc::new(Mutex::new(BasketState {
        items: crate::basket_service::gpui_queue_items(),
        notice: "Copy a download URL or import a .txt / .url list.".into(),
    }));
    Application::new().run(move |cx: &mut App| {
        initialize_gpui_app(cx);
        let bounds = Bounds::centered(None, size(px(440.0), px(500.0)), cx);
        let state_for_view = state.clone();
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from("Drop Basket")), ..Default::default() }),
                    ..Default::default()
                },
                move |_, cx| cx.new(|_| BasketView { state: state_for_view }),
            )
            .expect("open Download Craft Drop Basket window");
        cx.spawn(async move |cx| loop {
            Timer::after(Duration::from_secs(2)).await;
            if window
                .update(cx, |view, native_window, cx| {
                    view.reload();
                    cx.notify();
                    native_window.refresh();
                })
                .is_err()
            {
                break;
            }
        })
        .detach();
        cx.activate(true);
    });
}

pub fn run_links(path: PathBuf) {
    let batch = match crate::links_service::gpui_load(&path) {
        Ok(batch) => batch,
        Err(error) => {
            eprintln!("--links: {error}");
            std::process::exit(2);
        }
    };
    let count = batch.rows.len();
    let state = Arc::new(Mutex::new(LinksState {
        batch,
        notice: "Select files to add to the download queue.".into(),
    }));
    Application::new().run(move |cx: &mut App| {
        initialize_gpui_app(cx);
        let bounds = Bounds::centered(None, size(px(760.0), px(640.0)), cx);
        let state_for_view = state.clone();
        let _window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from(format!("Download page links ({count})"))), ..Default::default() }),
                ..Default::default()
            },
            move |_, cx| cx.new(|_| LinksView { state: state_for_view }),
        ).expect("open Download Craft links window");
        cx.activate(true);
    });
}
