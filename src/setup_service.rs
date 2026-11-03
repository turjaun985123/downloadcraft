//! UI-independent helper checks and repair actions shared by setup views.

use std::sync::mpsc::Sender;

const BROWSERS: &[&str] = crate::platform::HOST_BROWSERS;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Part {
    Connection,
    Extension,
    YtDlp,
    Ffmpeg,
    Deno,
}

impl Part {
    pub(crate) const ALL: [Part; 5] = [Part::Connection, Part::Extension, Part::YtDlp, Part::Ffmpeg, Part::Deno];

    pub(crate) fn title(self) -> &'static str {
        match self {
            Part::Connection => "Browser connection",
            Part::Extension => "Browser extension",
            Part::YtDlp => "yt-dlp",
            Part::Ffmpeg => "FFmpeg",
            Part::Deno => "Deno",
        }
    }

    pub(crate) fn purpose(self) -> &'static str {
        match self {
            Part::Connection => "Lets the extension hand downloads to the app",
            Part::Extension => "Download buttons on videos, and capturing downloads",
            Part::YtDlp => "Downloads from YouTube and 1,000+ video sites",
            Part::Ffmpeg => "Joins video and sound (YouTube above 360p)",
            Part::Deno => "Runs YouTube's player check",
        }
    }

    /// Fixed by the app itself (the extension needs the user's browser).
    pub(crate) fn automatic(self) -> bool {
        self != Part::Extension
    }
}

#[derive(Clone, PartialEq, Debug)]
pub(crate) enum State {
    Checking,
    Ready(String),
    Missing(String),
    Working(String),
    Failed(String),
}

impl State {
    pub(crate) fn ready(&self) -> bool {
        matches!(self, State::Ready(_))
    }

    pub(crate) fn detail(&self) -> String {
        match self {
            State::Checking => "Checking…".into(),
            State::Ready(value) | State::Missing(value) | State::Working(value) => value.clone(),
            State::Failed(error) => format!("Could not fix: {error}"),
        }
    }
}

/// Worker → setup view.
pub(crate) enum Msg {
    State(Part, State),
    Idle,
}

/// When the extension last talked to the app (written by the native host).
fn extension_seen() -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let path = crate::model::config_dir()?.join("extension_seen.json");
    let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    chrono::DateTime::parse_from_rfc3339(value.get("at")?.as_str()?).ok()
}

/// Check one part. Version probes may block; call from a worker thread.
pub(crate) fn check(part: Part) -> State {
    match part {
        Part::Connection => {
            let ready: Vec<&str> = BROWSERS.iter().copied().filter(|browser| crate::integration::native_host_status(browser)).collect();
            if ready.len() == BROWSERS.len() {
                State::Ready("Chrome, Edge, Brave, Opera, Vivaldi and Firefox".into())
            } else if ready.is_empty() {
                State::Missing("Not set up for any browser".into())
            } else {
                State::Missing(format!("Missing for {}", BROWSERS.iter().filter(|browser| !ready.contains(browser)).copied().collect::<Vec<_>>().join(", ")))
            }
        }
        Part::Extension => match extension_seen() {
            Some(at) => State::Ready(format!("Connected · last seen {}", crate::model::relative_time(&at.with_timezone(&chrono::Local)))),
            None => State::Missing("Not added to a browser yet (or the browser has not been opened since)".into()),
        },
        Part::YtDlp => {
            let info = crate::engine::ytdlp_info();
            match (info.path, info.version) {
                (Some(_), Some(version)) => State::Ready(format!("Version {version}")),
                (Some(_), None) => State::Ready("Installed".into()),
                _ => State::Missing("Not installed".into()),
            }
        }
        Part::Ffmpeg => {
            let info = crate::engine::ffmpeg_info();
            match (info.path, info.version) {
                (Some(_), Some(version)) => State::Ready(format!("Version {}", version.split_whitespace().next().unwrap_or(&version))),
                (Some(_), None) => State::Ready("Installed".into()),
                _ => State::Missing("Not installed".into()),
            }
        }
        Part::Deno => match crate::engine::js_runtime_name() {
            Some(name) => State::Ready(format!("Using {name}")),
            None => State::Missing("Not installed".into()),
        },
    }
}

fn fix(part: Part) -> Result<(), String> {
    match part {
        Part::Connection => {
            let mut errors = Vec::new();
            for browser in BROWSERS {
                if let Err(error) = crate::integration::ensure_native_host(browser) {
                    errors.push(format!("{browser}: {error}"));
                }
            }
            if errors.len() == BROWSERS.len() { Err(errors.join("; ")) } else { Ok(()) }
        }
        Part::YtDlp => crate::engine::ytdlp_refresh_blocking().map(|_| ()).map_err(|error| format!("{error:#}")),
        Part::Ffmpeg => crate::engine::ffmpeg_refresh_blocking().map(|_| ()).map_err(|error| format!("{error:#}")),
        Part::Deno => crate::engine::deno_install_blocking().map(|_| ()).map_err(|error| format!("{error:#}")),
        Part::Extension => Ok(()),
    }
}

fn working_text(part: Part) -> &'static str {
    match part {
        Part::Connection => "Setting up…",
        _ => "Downloading…",
    }
}

pub(crate) fn run_fixes(tx: Sender<Msg>, parts: Vec<Part>) {
    std::thread::spawn(move || {
        for part in parts {
            let _ = tx.send(Msg::State(part, State::Working(working_text(part).into())));
            let state = match fix(part) {
                Ok(()) => check(part),
                Err(error) => State::Failed(error),
            };
            let _ = tx.send(Msg::State(part, state));
        }
        let _ = tx.send(Msg::Idle);
    });
}

pub(crate) fn run_checks(tx: Sender<Msg>, parts: Vec<Part>) {
    std::thread::spawn(move || {
        for part in parts {
            let _ = tx.send(Msg::State(part, check(part)));
        }
        let _ = tx.send(Msg::Idle);
    });
}

pub(crate) fn helper_lines() -> Vec<(&'static str, String, bool)> {
    [Part::YtDlp, Part::Ffmpeg, Part::Deno]
        .into_iter()
        .map(|part| match check(part) {
            State::Ready(detail) => (part.title(), detail, true),
            State::Missing(detail) | State::Failed(detail) | State::Working(detail) => (part.title(), detail, false),
            State::Checking => (part.title(), String::new(), false),
        })
        .collect()
}

pub(crate) fn extension_directory() -> Option<std::path::PathBuf> {
    crate::platform::extension_directory(false).ok()
}

/// Launch the GPUI setup window in a new process for external callers.
pub(crate) fn spawn(auto: bool) {
    let Ok(exe) = crate::platform::launch_path() else { return };
    let mut command = crate::platform::silent_command(exe);
    command.arg("--setup");
    if auto {
        command.arg("--auto");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0008);
    }
    let _ = command.spawn();
}
