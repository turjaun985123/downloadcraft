//! GPUI browser-download confirmation window.

use crate::capture_service::{load_prefs, main_app_running};
use crate::confirm_core::{build_confirm_app, ConfirmApp, NOT_RUNNING, START_WAIT};
use crate::engine::{self};
use crate::model::{category_of, filename_from_url, is_download_url};
use gpui::prelude::*;
use gpui::{div, px, rgb, size, Application, Bounds, FocusHandle, SharedString, Timer, TitlebarOptions, Window, WindowBounds, WindowOptions};
use std::time::Duration;

pub(crate) struct GpuiConfirmView {
    pub(crate) app: ConfirmApp,
    _confirm_slot: std::fs::File,
    filename_focus: FocusHandle,
    capture: Option<crate::native_host::InboxItem>,
    capture_acknowledged: bool,
}

impl GpuiConfirmView {
    fn acknowledge_capture_if_done(&mut self) {
        if !self.app.done || self.capture_acknowledged { return; }
        let Some(capture) = self.capture.as_ref() else {
            self.capture_acknowledged = true;
            return;
        };
        match crate::native_host::acknowledge_capture(capture) {
            Ok(_) => self.capture_acknowledged = true,
            Err(error) => eprintln!("Could not acknowledge browser capture: {error}"),
        }
    }

    pub(crate) fn tick(&mut self) {
        self.app.gpui_tick();
        self.acknowledge_capture_if_done();
    }

    pub(crate) fn is_done(&self) -> bool { self.app.done }

    /// Merge newer native-host metadata only into fields the user has not edited.
    pub(crate) fn refresh_capture(&mut self, item: &crate::native_host::InboxItem) -> Option<bool> {
        let previous = self.capture.as_ref()?;
        if previous.url != item.url || previous.capture_id != item.capture_id
            || (previous.capture_id.is_none() && previous.received_at != item.received_at) { return None; }
        let mut changed = false;
        macro_rules! update_if_untouched {
            ($field:ident, $previous:expr, $next:expr) => {{
                let old = $previous;
                let next = $next;
                if self.app.$field == old && self.app.$field != next {
                    self.app.$field = next;
                    changed = true;
                }
            }};
        }
        let previous_referer = previous.referer.as_deref().filter(|v| !v.trim().is_empty())
            .or(previous.page_url.as_deref()).unwrap_or_default().to_string();
        let next_referer = item.referer.as_deref().filter(|v| !v.trim().is_empty())
            .or(item.page_url.as_deref()).unwrap_or_default().to_string();
        update_if_untouched!(referer, previous_referer, next_referer);
        update_if_untouched!(cookie, previous.cookie.clone().unwrap_or_default(), item.cookie.clone().unwrap_or_default());
        update_if_untouched!(headers, previous.headers.clone().unwrap_or_default(), item.headers.clone().unwrap_or_default());
        update_if_untouched!(browser, previous.browser.clone(), item.browser.clone());
        update_if_untouched!(page_mime, previous.mime.clone().unwrap_or_default(), item.mime.clone().unwrap_or_default());
        update_if_untouched!(pair_url,
            previous.pair_url.clone().filter(|v| v.starts_with("http")).unwrap_or_default(),
            item.pair_url.clone().filter(|v| v.starts_with("http")).unwrap_or_default());
        update_if_untouched!(pair_mime, previous.pair_mime.clone().unwrap_or_default(), item.pair_mime.clone().unwrap_or_default());
        update_if_untouched!(quality_pref, previous.quality.clone().unwrap_or_default(), item.quality.clone().unwrap_or_default());
        update_if_untouched!(duration_secs, previous.dur.unwrap_or(0).min(86_399), item.dur.unwrap_or(0).min(86_399));
        let old_size = previous.size.filter(|s| *s > 0)
            .filter(|_| !engine::is_stream_manifest(&previous.url, None, previous.mime.as_deref().unwrap_or_default()));
        let next_size = item.size.filter(|s| *s > 0)
            .filter(|_| !engine::is_stream_manifest(&item.url, None, item.mime.as_deref().unwrap_or_default()));
        if self.app.browser_size == old_size && self.app.browser_size != next_size {
            self.app.browser_size = next_size;
            self.app.probe_size = next_size.or(self.app.probe_size);
            changed = true;
        }
        if self.app.auto_name.as_deref() == Some(self.app.filename.as_str()) {
            if let Some(name) = item.filename.as_deref().filter(|v| !v.trim().is_empty()) {
                let name = crate::model::apply_rename_template(&self.app.rename_template, &engine::sanitize_filename(name), &self.app.url);
                self.app.filename = name.clone();
                self.app.auto_name = Some(name.clone());
                self.app.category = category_of(&name).to_string();
                changed = true;
            }
        }
        self.capture = Some(item.clone());
        Some(changed)
    }
}

impl Drop for GpuiConfirmView {
    fn drop(&mut self) {
        if !self.app.done { self.app.stop_prestart(None); }
    }
}

pub(crate) fn gpui_confirm_view(
    requested: crate::native_host::InboxItem,
    runtime: tokio::runtime::Handle,
    filename_focus: FocusHandle,
) -> Option<GpuiConfirmView> {
    let url = requested.url.trim().to_string();
    if !is_download_url(&url) { return None; }
    let slot = crate::model::claim_window(&crate::native_host::confirm_window_name(&url))?;
    let item = crate::native_host::pending_captures().ok()?.into_iter().find(|item| {
        item.url == requested.url && item.capture_id == requested.capture_id
            && (item.capture_id.is_some() || item.received_at == requested.received_at)
    })?;
    let prefs = load_prefs();
    let page_mime = item.mime.clone().unwrap_or_default();
    let caller_name = item.filename.clone().filter(|name| !name.trim().is_empty());
    let named_by_caller = caller_name.is_some();
    let filename = caller_name.unwrap_or_else(|| crate::model::filename_from_url_with_mime(&url, Some(&page_mime)));
    let filename = crate::model::apply_rename_template(&prefs.rename_template, &engine::sanitize_filename(&filename), &url);
    let known_size = item.size.filter(|size| *size > 0)
        .filter(|_| !engine::is_stream_manifest(&url, None, &page_mime));
    let quality_pref = item.quality.clone().unwrap_or_default();
    let initial_notice = match crate::model::queued_entry(&url, &quality_pref) {
        Some((id, false)) => format!("Already in the queue (#{id})."),
        _ => String::new(),
    };
    let mut app = ConfirmApp {
        pre: None, pre_tried: false, url: url.clone(),
        auto_name: (!named_by_caller).then(|| filename.clone()),
        category: category_of(&filename).to_string(), filename,
        remember_path: prefs.category_folders, save_base: prefs.save_dir,
        description: String::new(),
        referer: item.referer.clone().filter(|v| !v.trim().is_empty()).or_else(|| item.page_url.clone()).unwrap_or_default(),
        cookie: item.cookie.clone().unwrap_or_default(), headers: item.headers.clone().unwrap_or_default(),
        browser: item.browser.clone(), pair_url: item.pair_url.clone().filter(|v| v.starts_with("http")).unwrap_or_default(),
        pair_mime: item.pair_mime.clone().unwrap_or_default(), page_mime,
        duration_secs: item.dur.unwrap_or(0).min(86_399), expects_audio: item.audio_expected,
        quality_pref, connections: prefs.connections, user_agent: prefs.user_agent,
        proxy_url: prefs.proxy_url, timeout_secs: prefs.timeout_secs,
        rename_template: prefs.rename_template, folder_rules: prefs.folder_rules,
        probing: false, probe_size: known_size, browser_size: known_size, probe_rx: None,
        rt: Some(runtime), done: false,
        waiting_app: None, notice: initial_notice,
    };
    if known_size.is_none() { app.start_probe(); }
    Some(GpuiConfirmView { app, _confirm_slot: slot, filename_focus, capture: Some(item), capture_acknowledged: false })
}

impl ConfirmApp {
    pub(crate) fn gpui_tick(&mut self) {
        if let Some(since) = self.waiting_app {
            if main_app_running() {
                self.notice.clear();
                self.commit(true);
            } else if since.elapsed() > START_WAIT {
                self.waiting_app = None;
                self.notice = crate::i18n::t(NOT_RUNNING).to_string();
            }
        } else if !main_app_running() && self.notice.is_empty() {
            self.notice = crate::i18n::t(NOT_RUNNING).to_string();
        } else if main_app_running() && self.notice == crate::i18n::t(NOT_RUNNING) {
            self.notice.clear();
        }
        if let Some((size, name)) = self.probe_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.probing = false;
            self.probe_size = size.or(self.browser_size);
            if let Some(name) = name {
                let untouched = self.auto_name.as_deref() == Some(self.filename.as_str());
                if untouched || self.filename == filename_from_url(&self.url) || crate::model::is_fallback_filename(&self.filename) {
                    self.filename = crate::model::apply_rename_template(&self.rename_template, &name, &self.url);
                    self.category = category_of(&self.filename).to_string();
                    if untouched { self.auto_name = Some(self.filename.clone()); }
                }
            }
        }
        self.maybe_prestart();
    }
}

impl Render for GpuiConfirmView {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        use gpui::KeyDownEvent;
        let name = self.app.filename.clone();
        let dir = self.app.effective_dir();
        let url = self.app.url.clone();
        let notice = self.app.notice.clone();
        let size_text = self.app.probe_size.or(self.app.browser_size).map(crate::model::format_bytes)
            .unwrap_or_else(|| if self.app.probing { "Checking size…".into() } else { "Size unavailable".into() });
        let focus = self.filename_focus.clone();
        let filename = div().id("confirm-filename").track_focus(&focus).w_full().px_3().py_2()
            .bg(rgb(0x111317)).rounded_sm().border_1()
            .border_color(rgb(if focus.is_focused(window) { 0x3b82f6 } else { 0x292d34 }))
            .child(if name.is_empty() { "File name".to_string() } else { name })
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |view, _, window, _| view.filename_focus.focus(window)))
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                if key == "escape" {
                    view.app.stop_prestart(None); view.app.done = true; view.acknowledge_capture_if_done(); window.remove_window(); return;
                }
                if key == "enter" || key == "return" {
                    view.app.commit(true); view.acknowledge_capture_if_done();
                    if view.app.done { window.remove_window(); } else { cx.notify(); }
                    return;
                }
                let modifiers = &event.keystroke.modifiers;
                if (modifiers.control || modifiers.platform) && key.eq_ignore_ascii_case("v") {
                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                        if let Ok(text) = clipboard.get_text() { view.app.filename = text.trim().to_string(); view.app.auto_name = None; }
                    }
                } else if key == "backspace" { view.app.filename.pop(); view.app.auto_name = None; }
                else if !modifiers.control && !modifiers.platform && !modifiers.alt {
                    if let Some(ch) = &event.keystroke.key_char { if !ch.chars().any(char::is_control) { view.app.filename.push_str(ch); view.app.auto_name = None; } }
                }
                cx.notify();
            }));
        let download = div().id("confirm-download").px_4().py_2().bg(rgb(0x3b82f6)).hover(|s| s.bg(rgb(0x5793f8)))
            .rounded_sm().cursor_pointer().text_sm().font_weight(gpui::FontWeight::SEMIBOLD)
            .child(if self.app.waiting_app.is_some() { "Starting…" } else { "Download now" })
            .on_click(cx.listener(|view, _, window, cx| { view.app.commit(true); view.acknowledge_capture_if_done(); if view.app.done { window.remove_window(); } else { cx.notify(); } }));
        let later = div().id("confirm-later").px_3().py_2().hover(|s| s.bg(rgb(0x20242b)))
            .rounded_sm().cursor_pointer().text_sm().text_color(rgb(0xc0c5ce)).child("Add to queue")
            .on_click(cx.listener(|view, _, window, cx| { view.app.commit(false); view.acknowledge_capture_if_done(); if view.app.done { window.remove_window(); } else { cx.notify(); } }));
        let cancel = div().id("confirm-cancel").px_2().py_2().hover(|s| s.bg(rgb(0x20242b)))
            .rounded_sm().cursor_pointer().text_sm().text_color(rgb(0x8b929e)).child("Cancel")
            .on_click(cx.listener(|view, _, window, _| { view.app.stop_prestart(None); view.app.done = true; view.acknowledge_capture_if_done(); window.remove_window(); }));
        div().size_full().flex().items_center().justify_center().bg(rgb(0x0b0c0e)).text_color(rgb(0xf0f3f6))
            .child(div().w(px(480.0)).flex().flex_col().gap_3().p_4().bg(rgb(0x111317)).border_1().border_color(rgb(0x21262d)).rounded_md()
                .child(div().flex().items_center().justify_between()
                    .child(div().flex().items_center().gap_3()
                        .child(div().w(px(34.0)).h(px(34.0)).flex().items_center().justify_center().rounded_sm().bg(rgb(0x1c2d45)).text_color(rgb(0x78aef8)).child("↓"))
                        .child(div().flex().flex_col().gap_1()
                            .child(div().text_base().font_weight(gpui::FontWeight::SEMIBOLD).child("Download file"))
                            .child(div().text_xs().text_color(rgb(0x8b929e)).font_family("monospace").child(size_text))))
                    .child(cancel))
                .child(div().text_xs().font_weight(gpui::FontWeight::SEMIBOLD).text_color(rgb(0x8b929e)).child("FILE NAME"))
                .child(filename)
                .child(div().flex().flex_col().gap_1()
                    .child(div().text_xs().font_weight(gpui::FontWeight::SEMIBOLD).text_color(rgb(0x8b929e)).child("SAVE TO"))
                    .child(div().text_sm().child(dir)))
                .when(!url.is_empty(), |card| card.child(div().text_xs().font_family("monospace").text_color(rgb(0x69717d)).child(url)))
                .when(!notice.is_empty(), |card| card.child(div().text_xs().text_color(rgb(0xe4ad55)).child(notice)))
                .child(div().h(px(1.0)).w_full().bg(rgb(0x21262d)))
                .child(div().flex().justify_end().items_center().gap_2().child(later).child(download)))
    }
}

pub fn run_gpui() {
    let Some((app, _slot, runtime)) = build_confirm_app() else { return; };
    let guard = runtime.clone();
    Application::new().run(move |cx: &mut gpui::App| {
        let _ = cx.text_system().add_fonts(vec![std::borrow::Cow::Borrowed(&include_bytes!("../assets/fonts/MaterialIcons-Regular.ttf")[..])]);
        cx.on_window_closed(|cx| { if cx.windows().is_empty() { cx.quit(); } }).detach();
        let state = std::sync::Arc::new(std::sync::Mutex::new(Some((app, runtime))));
        let state_for_window = state.clone();
        let bounds = Bounds::centered(None, size(px(520.0), px(380.0)), cx);
        let window = cx.open_window(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from("Add download")),
                ..Default::default()
            }),
            // A normal window is required here: GPUI popup windows are
            // tool/notification windows on Windows and X11, which suppress
            // the OS titlebar controls regardless of the titlebar option.
            kind: gpui::WindowKind::Normal,
            is_resizable: true,
            is_movable: true,
            is_minimizable: true,
            window_decorations: Some(gpui::WindowDecorations::Server),
            ..Default::default()
        }, move |_, cx| {
            let (app, runtime) = state_for_window.lock().unwrap_or_else(|e| e.into_inner()).take().expect("confirmation state");
            let _ = runtime;
            cx.new(|cx| GpuiConfirmView { app, _confirm_slot: _slot, filename_focus: cx.focus_handle(), capture: None, capture_acknowledged: false })
        }).expect("open GPUI confirmation window");
        let refresh = window.clone();
        cx.spawn(async move |cx| loop {
            Timer::after(Duration::from_millis(200)).await;
            if refresh.update(cx, |view, window, cx| { view.tick(); if view.is_done() { window.remove_window(); } else { cx.notify(); } }).is_err() { break; }
        }).detach();
        cx.activate(true);
    });
    match std::sync::Arc::try_unwrap(guard) {
        Ok(runtime) => runtime.shutdown_timeout(Duration::from_millis(200)),
        Err(runtime) => { let _ = std::thread::spawn(move || drop(runtime)); }
    }
}
