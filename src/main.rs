#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Download Craft: GUI + CLI + native host + standalone dialogs.
//! Release builds run without a console window; all diagnostics go to
//! %APPDATA%/download_craft/crash.log and the in-app log/queues panels.

mod aes128;
mod archive;
mod about_service;
mod basket_service;
mod capture_service;
mod integration;
#[cfg(feature = "gpui-ui")]
mod gpui_app;
mod cli;
mod complete_service;
mod confirm_core;
#[cfg(feature = "gpui-ui")]
mod gpui_confirm;
mod engine;
mod ftp;
mod gdrive;
mod grabber;
mod i18n;
mod links_service;
mod model;
mod native_host;
mod selfupdate;
mod autostart;
mod sysproxy;
mod winauth;
#[cfg(windows)]
mod tray;
#[cfg(not(windows))]
#[path="tray_portable.rs"]
mod tray;
mod platform;
mod progress_service;
mod setup_service;
mod update_service;
mod helpers;

use clap::Parser;
use std::sync::Arc;

/// Persist panics to %APPDATA%/download_craft/crash.log so a crash can be
/// diagnosed after the fact (the message also goes to the console).
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let msg = format!("panic: {info}\nlocation: {:?}\nbacktrace:\n{bt}\n", info.location());
        if let Some(dir) = model::config_dir() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join("crash.log"), &msg);
        }
        eprintln!("{msg}");
        prev(info);
    }));
}

#[cfg(not(feature = "gpui-ui"))]
fn ui_unavailable() -> ! {
    eprintln!("Download Craft's desktop interface was disabled in this build.");
    std::process::exit(2);
}

fn main() {
    // Native messaging is a short-lived stdio service. Detect it before GUI
    // setup or instance locks so a running desktop process cannot intercept it.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--native-host") || native_host::launched_directly(&args) {
        model::migrate_config();
        if let Err(error) = model::secure_config_directory() {
            eprintln!("Could not protect application state directory: {error}");
        }
        native_host::run();
        return;
    }

    // Migrate legacy `idm_rust` config (queue/settings/inbox) forward.
    model::migrate_config();
    if let Err(e)=model::secure_config_directory() { eprintln!("Could not protect application state directory: {e}"); }
    install_panic_hook();
    // IDM-style standalone "Download File Info" dialog (one OS window per
    // captured download, always on top). Spawned by the native host.
    if args.iter().any(|a| a == "--confirm-download") {
        #[cfg(feature = "gpui-ui")]
        { gpui_confirm::run_gpui(); return; }
        #[cfg(not(feature = "gpui-ui"))]
        { ui_unavailable(); }
    }
    // IDM-style "Download all links" picker (spawned by the native host).
    if args.iter().any(|a| a == "--links") {
        #[cfg(feature = "gpui-ui")]
        {
            let path = args.iter().position(|arg| arg == "--links")
                .and_then(|index| args.get(index + 1))
                .map(std::path::PathBuf::from)
                .unwrap_or_default();
            gpui_app::run_links(path);
            return;
        }
        #[cfg(not(feature = "gpui-ui"))]
        { ui_unavailable(); }
    }
    // IDM-style standalone per-download progress window.
    if args.iter().any(|a| a == "--progress") {
        #[cfg(feature = "gpui-ui")]
        { gpui_app::run_progress(); return; }
        #[cfg(not(feature = "gpui-ui"))]
        { ui_unavailable(); }
    }
    // IDM-style standalone "Download complete" dialog (popped by the app when
    // a download finishes).
    if args.iter().any(|a| a == "--complete") {
        #[cfg(feature = "gpui-ui")]
        { gpui_app::run_complete(); return; }
        #[cfg(not(feature = "gpui-ui"))]
        { ui_unavailable(); }
    }
    // P3.3 companions: drop basket + mini status.
    if args.iter().any(|a| a == "--basket") {
        #[cfg(feature = "gpui-ui")]
        { gpui_app::run_basket(); return; }
        #[cfg(not(feature = "gpui-ui"))]
        { ui_unavailable(); }
    }
    // Standalone Settings window (two-pane, separate OS window).
    if args.iter().any(|a| a == "--settings") {
        #[cfg(feature = "gpui-ui")]
        { gpui_app::run_settings(); return; }
        #[cfg(not(feature = "gpui-ui"))]
        { ui_unavailable(); }
    }
    // "Get ready": what the app needs, with one-click fixes.
    if args.iter().any(|a| a == "--about") {
        #[cfg(feature = "gpui-ui")]
        { gpui_app::run_about(); return; }
        #[cfg(not(feature = "gpui-ui"))]
        { ui_unavailable(); }
    }
    if args.iter().any(|a| a == "--setup") {
        #[cfg(feature = "gpui-ui")]
        { gpui_app::run_setup(); return; }
        #[cfg(not(feature = "gpui-ui"))]
        { ui_unavailable(); }
    }
    if args.iter().any(|a| a == "--mini") {
        #[cfg(feature = "gpui-ui")]
        { gpui_app::run_mini(); return; }
        #[cfg(not(feature = "gpui-ui"))]
        { ui_unavailable(); }
    }
    // Started by a one-click update: wait for the old version to close,
    // then start the window as usual.
    let after_update = args.iter().any(|a| a == "--wait-pid");
    // A sign-in launch (Start with Windows) opens the window into the tray.
    let at_startup = autostart::is_startup_launch(&args);
    if after_update {
        selfupdate::wait_for_previous(&args);
    }
    // CLI mode when a subcommand is given (`--help` works too).
    if args.len() > 1 && !after_update && !at_startup {
        match cli::Cli::try_parse_from(&args) {
            Ok(c) => {
                if let Err(e) = cli::run(c) {
                    eprintln!("error: {e:#}");
                    std::process::exit(1);
                }
                return;
            }
            Err(e) => {
                // clap handles --help/--version (exit 0) vs real errors (exit 2).
                e.exit();
            }
        }
    }

    // One manager window only. A second launch (Start menu, pinned shortcut,
    // taskbar) wakes the running instance and raises it instead of opening a
    // fresh window behind whatever the user is looking at.
    let _instance = match tray::claim_main_instance() {
        Some(slot) => slot,
        None => return,
    };
    crate::model::set_app_lifecycle("starting");

    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime"),
    );
    #[cfg(feature = "gpui-ui")]
    {
        std::thread::spawn(crate::native_host::refresh_site_rules);
        crate::gpui_app::run(rt);
        crate::model::clear_heartbeat();
        return;
    }
    #[cfg(not(feature = "gpui-ui"))]
    {
        let _ = rt;
        ui_unavailable();
    }
}
