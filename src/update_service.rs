//! UI-independent application update orchestration shared by the GPUI
//! download manager and the legacy frontend.
//!
//! The service owns network/update state and publishes `update_state.json`;
//! its caller owns queue persistence and decides when it is safe to exit for
//! a restart.

use crate::selfupdate::Asset;
use std::{sync::mpsc, time::Duration};
use tokio::runtime::Handle;

enum CheckMessage {
    Finished(String, Option<Asset>, bool),
}

enum InstallMessage {
    Status(String),
    Installed(usize),
    Failed(String),
}

/// Work the host application must perform after polling the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateEvent {
    /// The current executable cannot replace itself. Queue this trusted asset
    /// through the normal download manager instead.
    QueueDownload { url: String, name: String },
    /// Persist queue and settings before relaunching and exiting.
    RestartRequested,
    /// The published status changed; refresh any UI observing it.
    Changed,
}

/// Update check/install state without any UI framework dependency.
pub struct UpdateService {
    runtime: Handle,
    status: String,
    asset: Option<Asset>,
    found: bool,
    busy: bool,
    restart_ready: bool,
    restart_when_idle: bool,
    automatic_attempt: bool,
    check_rx: Option<mpsc::Receiver<CheckMessage>>,
    install_rx: Option<mpsc::Receiver<InstallMessage>>,
    last_published: String,
    active_downloads: usize,
}

impl UpdateService {
    pub fn new(runtime: Handle) -> Self {
        Self {
            runtime,
            status: String::new(),
            asset: None,
            found: false,
            busy: false,
            restart_ready: false,
            restart_when_idle: false,
            automatic_attempt: false,
            check_rx: None,
            install_rx: None,
            last_published: String::new(),
            active_downloads: 0,
        }
    }

    /// Process a command written by the GPUI About window.
    pub fn request(&mut self, action: &str) -> Vec<UpdateEvent> {
        let mut events = Vec::new();
        match action {
            "check_update" => self.start_check(),
            "self_update" => {
                self.restart_when_idle = false;
                self.automatic_attempt = false;
                if self.install_rx.is_none() {
                    if let Some(asset) = self.asset.clone() {
                        if can_self_update() {
                            self.start_install(asset, false);
                        } else {
                            self.status = "This copy cannot update itself; the update archive is being downloaded instead.".into();
                            events.push(queue_event(asset));
                        }
                    } else {
                        self.status = "No update asset is ready; check for updates first.".into();
                    }
                }
            }
            "restart_when_idle" => {
                self.restart_when_idle = true;
                if self.restart_ready && self.active_downloads == 0 {
                    self.restart_when_idle = false;
                    events.push(UpdateEvent::RestartRequested);
                }
            }
            "restart_update" => {
                if self.restart_ready {
                    if self.active_downloads == 0 {
                        self.restart_when_idle = false;
                        events.push(UpdateEvent::RestartRequested);
                    } else {
                        self.status = format!(
                            "Update installed. Restart after {} active download(s) finish.",
                            self.active_downloads
                        );
                    }
                }
            }
            _ => {}
        }
        self.publish();
        events
    }

    /// Advance async work and return host actions. Call from the GPUI timer
    /// alongside the normal queue refresh.
    pub fn poll(&mut self, active_downloads: usize) -> Vec<UpdateEvent> {
        self.active_downloads = active_downloads;
        let mut events = Vec::new();
        if let Some(rx) = self.check_rx.as_ref() {
            match rx.try_recv() {
                Ok(CheckMessage::Finished(status, asset, found)) => {
                    self.check_rx = None;
                    self.busy = false;
                    self.status = status;
                    self.asset = asset;
                    self.found = found;
                    if found
                        && can_self_update()
                        && crate::model::ui_pref_bool("update_auto", true)
                        && self.asset.as_ref().is_some_and(|asset| !asset.url.is_empty())
                        && self.install_rx.is_none()
                    {
                        if let Some(asset) = self.asset.clone() {
                            self.restart_when_idle = true;
                            self.automatic_attempt = true;
                            self.start_install(asset, true);
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.check_rx = None;
                    self.busy = false;
                    self.status = "Update check failed unexpectedly.".into();
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        let install_messages = self
            .install_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        for message in install_messages {
            match message {
                    InstallMessage::Status(status) => self.status = status,
                    InstallMessage::Installed(count) => {
                        self.install_rx = None;
                        self.busy = false;
                        self.found = false;
                        self.restart_ready = true;
                        self.status = format!("Update installed ({count} files). Restart to use it.");
                        if self.active_downloads == 0 {
                            self.restart_when_idle = false;
                            events.push(UpdateEvent::RestartRequested);
                        } else {
                            self.status = format!(
                                "Update installed. Restart after {} active download(s) finish.",
                                self.active_downloads
                            );
                        }
                        break;
                    }
                    InstallMessage::Failed(error) => {
                        self.install_rx = None;
                        self.busy = false;
                        if std::mem::take(&mut self.automatic_attempt) {
                            self.restart_when_idle = false;
                            self.status = format!(
                                "Update failed ({error}). Nothing was changed — click Update to try again."
                            );
                        } else if let Some(asset) = self.asset.clone() {
                            self.status = format!(
                                "Update failed ({error}). Nothing was changed; downloading the update archive instead."
                            );
                            events.push(queue_event(asset));
                        } else {
                            self.status = format!("Update failed: {error}");
                        }
                        break;
                    }
            }
        }

        if self.restart_ready && self.restart_when_idle && self.active_downloads == 0 {
            self.restart_when_idle = false;
            events.push(UpdateEvent::RestartRequested);
        }
        if self.publish() {
            events.push(UpdateEvent::Changed);
        }
        events
    }

    fn start_check(&mut self) {
        if self.busy {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.check_rx = Some(rx);
        self.busy = true;
        self.status = "Checking for updates…".into();
        self.asset = None;
        self.found = false;
        self.runtime.spawn(async move {
            let (status, asset, found) = update_check_async().await;
            let _ = tx.send(CheckMessage::Finished(status, asset, found));
        });
    }

    fn start_install(&mut self, asset: Asset, automatic: bool) {
        if self.install_rx.is_some() {
            return;
        }
        let target = if cfg!(windows) { crate::selfupdate::portable_dir() } else { None };
        let Some(target) = target else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.install_rx = Some(rx);
        self.busy = true;
        self.status = format!("Downloading {}…", asset.name);
        self.automatic_attempt = automatic;
        let user_agent = format!("download_craft/{}", crate::model::APP_VERSION);
        self.runtime.spawn(async move {
            let result: anyhow::Result<usize> = async {
                let client = reqwest::Client::builder().user_agent(user_agent).build()?;
                let progress_tx = tx.clone();
                let mut last_percent = -1i32;
                let archive = crate::selfupdate::download(&client, &asset, move |fraction| {
                    let percent = (fraction * 100.0) as i32;
                    if percent != last_percent && percent % 5 == 0 {
                        last_percent = percent;
                        let _ = progress_tx.send(InstallMessage::Status(format!(
                            "Downloading update… {percent}%"
                        )));
                    }
                })
                .await?;
                let _ = tx.send(InstallMessage::Status("Checked. Installing…".into()));
                tokio::task::spawn_blocking(move || {
                    let unpacked = crate::selfupdate::unpack(&archive)?;
                    crate::selfupdate::install(&unpacked, &target)
                })
                .await?
            }
            .await;
            let message = match result {
                Ok(count) => InstallMessage::Installed(count),
                Err(error) => InstallMessage::Failed(format!("{error:#}")),
            };
            let _ = tx.send(message);
        });
    }

    fn publish(&mut self) -> bool {
        let phase = if self.restart_ready {
            "ready"
        } else if self.install_rx.is_some() {
            "installing"
        } else if self.busy {
            "checking"
        } else if self.found && self.asset.is_some() {
            "available"
        } else if self.status.starts_with("Update failed") {
            "failed"
        } else if self.status.starts_with("Up to date") {
            "uptodate"
        } else {
            "idle"
        };
        let state = serde_json::json!({
            "phase": phase,
            "status": self.status.clone(),
            "size": self.asset.as_ref().map(|asset| asset.size).unwrap_or(0),
            "running": self.active_downloads,
            "portable": can_self_update(),
        });
        let Ok(serialized) = serde_json::to_string(&state) else {
            return false;
        };
        if serialized == self.last_published {
            return false;
        }
        let Some(path) = crate::model::config_dir().map(|dir| dir.join("update_state.json")) else {
            return false;
        };
        if crate::model::atomic_write(&path, serialized.as_bytes()).is_ok() {
            self.last_published = serialized;
            true
        } else {
            false
        }
    }
}

fn can_self_update() -> bool {
    cfg!(windows) && crate::selfupdate::portable_dir().is_some()
}

fn queue_event(asset: Asset) -> UpdateEvent {
    let name = asset
        .url
        .split(['?', '#'])
        .next()
        .unwrap_or(&asset.url)
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| asset.name);
    UpdateEvent::QueueDownload { url: asset.url, name }
}

async fn update_check_async() -> (String, Option<Asset>, bool) {
    let current = crate::model::APP_VERSION;
    let client = match reqwest::Client::builder()
        .user_agent(format!("download_craft/{current}"))
        .connect_timeout(Duration::from_secs(15))
        .build()
    {
        Ok(client) => client,
        Err(error) => return (format!("Update check failed: {error:#}"), None, false),
    };
    let api = std::env::var("DOWNLOAD_CRAFT_UPDATE_API")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            "https://api.github.com/repos/hungry-detective/downloadcraft/releases/latest".into()
        });
    let response = match client
        .get(api)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return (format!("Update check failed (network): {error:#}"), None, false),
    };
    if !response.status().is_success() {
        return (format!("Update check failed: HTTP {}", response.status()), None, false);
    }
    let release: serde_json::Value = match response.json().await {
        Ok(release) => release,
        Err(error) => return (format!("Update check failed (parse): {error:#}"), None, false),
    };
    let tag = release.get("tag_name").and_then(serde_json::Value::as_str).unwrap_or("");
    if tag.is_empty() {
        return ("Update check failed: no releases found.".into(), None, false);
    }
    let newer = crate::model::is_newer_version(current, tag);
    crate::about_service::save_release_info(&release, newer);
    if newer {
        let url = release
            .get("html_url")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("https://github.com/hungry-detective/downloadcraft/releases");
        (
            format!("Update available: {tag} (you have v{current}) — {url}"),
            crate::selfupdate::pick_asset(&release),
            true,
        )
    } else {
        (format!("Up to date (v{current})."), None, false)
    }
}
