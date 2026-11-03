//! IDM-style standalone "Download File Info" dialog.
//!
//! Launched as a separate OS window (always on top) when the browser
//! extension captures a download:
//!   `download_craft.exe --confirm-download --url <url> [--filename ..]
//!    [--referer ..] [--cookie ..] [--mime ..]`
//!
//! Unlike the in-canvas Add modal in the main window, this pops in front of
//! whatever the user is doing — one window per captured download, like IDM.
//! It writes straight to the shared `queue.json` (+ `wake.json` for
//! Start Download), so it works even when the main app is closed.

use crate::app::pl;
use crate::dm::*;
use crate::engine::{self, EngineOpts};
use crate::model::{
    atomic_write, category_of, config_dir, filename_from_url, format_bytes, is_download_url,
    DownloadItem,
};
pub(crate) use crate::confirm_core::{build_confirm_app, claim_inbox_at, ConfirmApp, PreStart, CATEGORIES, NOT_RUNNING, STARTING, START_WAIT};
pub(crate) use crate::capture_service::{
    gpui_wake_main_app, launch_main_app, load_prefs, main_app_running, save_remember_category,
    wake_ids, UiPrefs,
};
use std::sync::mpsc::Receiver;

/// Seconds → "3:26" badge for browser-known media durations.
fn format_dur(secs: u64) -> String {
    if secs == 0 || secs >= 86400 {
        return String::new();
    }
    format!("{}:{:02}", secs / 60, secs % 60)
}

// ---------- concept geometry (tokens live in `crate::dm`) ----------
// The capture popup is the same "Add download" card as the in-canvas modal,
// so it reuses `dm`'s tokens and row offsets verbatim: 562x253 at rest, with
// the header's window controls, the size chip and the gear panel in the same
// places.
/// Advanced (gear) panel height: category + description, referer + cookie,
/// connections, remember-path. The concept draws this collapsed.
const PANEL_H: f32 = 178.0;
/// Height of one dim note line inside the panel.
const NOTE_H: f32 = 12.0;

/// Loads Segoe UI — the family the concept asks for — plus a semibold cut for
/// its 600-weight title and primary button. Missing system fonts fall back to
/// the bundled defaults, so this is safe on any machine.
fn setup_popup_fonts(cc: &eframe::CreationContext<'_>) {
    // `true`: this whole window is the concept, so Segoe UI also becomes its
    // default proportional face.
    crate::fonts::install(cc, true);
}

/// Concept "+" square: accent fill, white plus (adds a custom category).
fn plus_square(ui: &mut egui::Ui, rect: egui::Rect, salt: u64) -> egui::Response {
    let r = ui.interact(rect, egui::Id::new(("popup-plus", salt)), egui::Sense::click());
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let p = ui.painter();
    p.rect(
        rect,
        DM_RAD,
        if r.hovered() { DM_ACC2 } else { DM_ACC },
        egui::Stroke::NONE,
        egui::StrokeKind::Inside,
    );
    let c = rect.center();
    let st = egui::Stroke::new(2.0, egui::Color32::WHITE);
    p.line_segment([egui::pos2(c.x - 6.0, c.y), egui::pos2(c.x + 6.0, c.y)], st);
    p.line_segment([egui::pos2(c.x, c.y - 6.0), egui::pos2(c.x, c.y + 6.0)], st);
    r
}

/// Floating option list anchored under `resp` — the concept's dropdown body,
/// shared by the category combo and the path field's recent folders, so both
/// menus look and behave identically.
fn options_menu(
    resp: &egui::Response,
    options: &[String],
    open: &mut bool,
    selected: &str,
) -> Option<String> {
    if options.is_empty() {
        *open = false;
        return None;
    }
    let mut picked = None;
    egui::Popup::from_response(resp)
        .open_bool(&mut *open)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .align(egui::RectAlign::BOTTOM_START)
        .gap(6.0)
        .frame(
            egui::Frame::new()
                .fill(pl().dm_card)
                .stroke(egui::Stroke::new(1.0_f32, pl().dm_line))
                .corner_radius(DM_RAD)
                .inner_margin(egui::Margin::symmetric(4, 4)),
        )
        .show(|ui| {
            ui.set_min_width(280.0);
            egui::ScrollArea::vertical()
                .max_height(220.0)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(0.0, 2.0);
                    let w = ui.available_width().max(272.0);
                    for o in options {
                        let (r, ir) =
                            ui.allocate_exact_size(egui::vec2(w, 24.0), egui::Sense::click());
                        if ir.hovered() {
                            ui.painter().rect_filled(r, 4.0, pl().dm_hover);
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        ui.painter().text(
                            egui::pos2(r.left() + 8.0, r.center().y),
                            egui::Align2::LEFT_CENTER,
                            elide_left(o, 52),
                            reg(11.0),
                            if o.as_str() == selected { DM_ACC } else { pl().dm_txt },
                        );
                        if ir.clicked() {
                            picked = Some(o.clone());
                        }
                    }
                });
        });
    if picked.is_some() {
        *open = false;
    }
    picked
}

/// Concept dropdown: field box, current value, trailing caret, option list.
fn combo(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    id: &str,
    current: &str,
    options: &[String],
    open: &mut bool,
) -> Option<String> {
    let resp = ui.interact(rect, egui::Id::new(("popup-combo", id)), egui::Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let p = ui.painter();
    dm_box(p, rect);
    p.text(
        egui::pos2(rect.left() + 13.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        elide_left(current, 26),
        reg(11.0),
        pl().dm_txt,
    );
    dm_icon(
        p,
        egui::Rect::from_center_size(
            egui::pos2(rect.right() - 16.0, rect.center().y),
            egui::Vec2::splat(11.0),
        ),
        DmIcon::Chevron,
        pl().dm_dim,
        2.0,
    );
    if resp.clicked() {
        *open = !*open;
    }
    options_menu(&resp, options, open, current)
}

/// Concept checkbox row: 16px box plus label.
fn check_row(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    id: &str,
    checked: bool,
    label: &str,
) -> egui::Response {
    let label = crate::i18n::t(label);
    let r = ui.interact(rect, egui::Id::new(("popup-check", id)), egui::Sense::click());
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let box_r = egui::Rect::from_min_size(
        egui::pos2(rect.left(), rect.center().y - 8.0),
        egui::Vec2::splat(16.0),
    );
    dm_check(ui.painter(), box_r, checked);
    ui.painter().text(
        egui::pos2(box_r.right() + 8.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        reg(11.0),
        if r.hovered() { pl().dm_txt } else { pl().dm_dim },
    );
    r
}

/// Concept slider with its drag handling on the same rect, so painting and
/// input can never disagree. Returns the new fraction when it changed.
fn slider_row(ui: &mut egui::Ui, rect: egui::Rect, id: &str, frac: f32) -> Option<f32> {
    let r = ui.interact(rect, egui::Id::new(("popup-slider", id)), egui::Sense::click_and_drag());
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let mut out = None;
    if r.dragged() || r.clicked() {
        if let Some(pos) = ui.ctx().pointer_interact_pos() {
            let f = ((pos.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
            if (f - frac).abs() > 0.001 {
                out = Some(f);
            }
        }
    }
    let shown = out.unwrap_or(frac);
    slider(ui.painter(), rect, r.hovered(), shown, rect.left(), rect.width());
    out
}



pub fn run() -> eframe::Result<()> {
    let Some((app, _confirm_slot, _rt_keepalive)) = build_confirm_app() else {
        return Ok(());
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Add download")
        .with_inner_size([DM_W, DM_H])
        .with_resizable(false)
        .with_decorations(crate::platform::system_title_bar())
        .with_transparent(!crate::platform::system_title_bar())
        .with_always_on_top();
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    eframe::run_native(
        "Add download",
        eframe::NativeOptions {
            viewport,
            ..Default::default()
        },
        Box::new(|cc| {
            setup_popup_fonts(cc);
            crate::app::apply_theme(&cc.egui_ctx, &crate::app::stored_theme());
            Ok(Box::new(app))
        }),
    )
}

impl eframe::App for ConfirmApp {
    /// The window is transparent and frameless: the concept's rounded card is
    /// painted by hand, so the surface behind it must stay clear.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    /// Same bulletproofing as the progress window: Close can no-op, so back
    /// it with a delayed exit once the dialog is done.
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(text) = crate::i18n::take_font_text() {
            crate::fonts::ensure_text(ctx, &text);
        }
        crate::fonts::ensure_text(ctx, &self.filename);
        crate::fonts::ensure_text(ctx, &self.description);
        // Say up front when the app is not running (Download needs it), and
        // drop the note by itself once the app is started.
        let now = ctx.input(|i| i.time);
        let last = ctx.data_mut(|d| *d.get_temp_mut_or_insert_with(egui::Id::new("run-check"), || -10.0f64));
        if now - last >= 2.0 {
            ctx.data_mut(|d| d.insert_temp(egui::Id::new("run-check"), now));
            let note = crate::i18n::t(NOT_RUNNING).to_string();
            let running = main_app_running();
            if !running && self.notice.is_empty() && self.waiting_app.is_none() {
                self.notice = note;
            } else if running && self.notice == note {
                self.notice.clear();
            }
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(2));
        if let Some(since) = self.waiting_app {
            if main_app_running() {
                self.notice.clear();
                self.commit(true);
            } else if since.elapsed() > START_WAIT {
                self.waiting_app = None;
                self.notice = crate::i18n::t(NOT_RUNNING).to_string();
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(300));
            }
        }
        if self.done || ctx.input(|i| i.viewport().close_requested()) {
            self.stop_prestart(None);
            self.done = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            if !self.exited {
                self.exited = true;
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    std::process::exit(0);
                });
            }
            return;
        }
        self.maybe_prestart();
        // Middle of the work area (taskbar excluded), once the monitor
        // geometry is known; Windows would otherwise cascade the window.
        if !self.centered && crate::model::center_window(ctx, egui::vec2(DM_W, DM_H)) {
            self.centered = true;
        }
        // Collect probe result.
        if let Some(rx) = self.probe_rx.as_ref() {
            while let Ok((sz, nm)) = rx.try_recv() {
                self.probing = false;
                // A refused re-probe (off-session 403) must not erase the
                // size the browser measured in its own session.
                self.probe_size = sz.or(self.browser_size);
                if let Some(n) = nm {
                    // Prefer the server/video title while our name is still a
                    // guess we made (or a last-resort fallback whose timestamp
                    // drifted since dialog open) — never over a name the
                    // extension chose or the user typed.
                    let untouched_guess = self.auto_name.as_deref() == Some(self.filename.as_str());
                    if untouched_guess
                        || self.filename == filename_from_url(&self.url)
                        || crate::model::is_fallback_filename(&self.filename)
                    {
                        self.filename = crate::model::apply_rename_template(
                            &self.rename_template,
                            &n,
                            &self.url,
                        );
                        self.category = category_of(&self.filename).to_string();
                        if untouched_guess {
                            self.auto_name = Some(self.filename.clone());
                        }
                    }
                }
            }
        }
        if self.probing {
            ctx.request_repaint();
        }
        // Grow the window when the gear panel (or a notice) needs room, so the
        // concept surface itself never changes size or position.
        let notes = self.extra_notes();
        let panel_h = if self.show_adv {
            PANEL_H + if notes.is_empty() { 0.0 } else { 4.0 + NOTE_H * notes.len() as f32 }
        } else {
            0.0
        };
        let adv_h = if self.show_adv { panel_h + 14.0 } else { 0.0 };
        let mut want_h = DM_H + adv_h;
        if !self.notice.is_empty() {
            want_h += 16.0;
        }
        if (self.win_h - want_h).abs() > 0.5 {
            self.win_h = want_h;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(DM_W, want_h)));
        }
        // Esc closes, unless a menu is open (those close on the same key).
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !self.cat_open && !self.recent_open {
            self.done = true;
        }

        let panel = egui::CentralPanel::default().frame(egui::Frame::NONE);
        panel.show(ctx, |ui| {
            // The concept card IS the window: 562x253, 1px #1e293b border,
            // 8px radius, painted edge to edge over the clear surface.
            let card = ui.max_rect();
            let o = card.min;
            let p = ui.painter().clone();
            let at = |x: f32, y: f32, w: f32, h: f32| {
                egui::Rect::from_min_size(egui::pos2(o.x + x, o.y + y), egui::vec2(w, h))
            };
            p.rect(
                card,
                8.0,
                pl().dm_card,
                egui::Stroke::new(1.0, pl().dm_line),
                egui::StrokeKind::Inside,
            );
            // ---- header: mark + title + window controls (drag to move) ----
            dm_mark(&p, at(15.0, 11.0, 18.0, 18.0));
            p.text(
                egui::pos2(o.x + 41.0, o.y + 20.0),
                egui::Align2::LEFT_CENTER,
                "Download File Info",
                semi(13.0),
                pl().dm_txt,
            );
            p.line_segment(
                [
                    egui::pos2(o.x + 1.0, o.y + 39.5),
                    egui::pos2(o.x + DM_W - 1.0, o.y + 39.5),
                ],
                egui::Stroke::new(1.0, pl().dm_line),
            );
            if ui
                .interact(
                    at(0.0, 0.0, DM_W - 100.0, 39.0),
                    egui::Id::new("popup-header"),
                    egui::Sense::click_and_drag(),
                )
                .drag_started()
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            let ctl = |x: f32| {
                egui::Rect::from_center_size(
                    egui::pos2(o.x + x, o.y + 20.0),
                    egui::Vec2::splat(12.0),
                )
            };
            let (min_r, max_r, close_r) = (ctl(485.0), ctl(513.0), ctl(541.0));
            let min_b = ui.interact(min_r, egui::Id::new("popup-min"), egui::Sense::click());
            let max_b = ui.interact(max_r, egui::Id::new("popup-max"), egui::Sense::click());
            let close_b = ui.interact(close_r, egui::Id::new("popup-close"), egui::Sense::click());
            for (r, hov) in [
                (min_r, min_b.hovered()),
                (max_r, max_b.hovered()),
                (close_r, close_b.hovered()),
            ] {
                if hov {
                    p.rect_filled(r.expand(4.0), DM_RAD, pl().dm_hover2);
                }
            }
            if min_b.hovered() || max_b.hovered() || close_b.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            dm_icon(&p, min_r, DmIcon::Minimize, pl().dm_dim, 2.0);
            dm_icon(&p, max_r, DmIcon::Maximize, pl().dm_dim, 2.0);
            dm_icon(&p, close_r, DmIcon::Close, pl().dm_dim, 2.0);
            if min_b.clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            if max_b.clicked() {
                let max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!max));
            }
            if close_b.clicked() {
                self.done = true;
            }
            // ---- row 1: URL + clipboard (copies the captured link) ----
            let url_box = at(DM_X, DM_ROW1, DM_W_IN, DM_BOX_H);
            let clip_btn = egui::Rect::from_min_size(
                egui::pos2(url_box.max.x - 33.0, url_box.min.y + 5.0),
                egui::Vec2::splat(28.0),
            );
            dm_field(
                ui,
                "popup_url",
                url_box,
                clip_btn.min.x - 6.0,
                &mut self.url,
                "https://…",
                false,
            );
            let clip_b = ui.interact(clip_btn, egui::Id::new("popup-clip"), egui::Sense::click());
            if clip_b.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            icon_btn(&p, clip_btn, clip_b.hovered(), DmIcon::Clipboard, 14.0, 1.5, pl().dm_dim);
            if clip_b.clicked() {
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    let _ = cb.set_text(self.url.clone());
                }
            }
            // ---- row 2: save folder | size chip ----
            let path_box = at(DM_X, DM_ROW2, DM_PATH_W, DM_BOX_H);
            let caret_btn = egui::Rect::from_min_size(
                egui::pos2(path_box.max.x - 32.0, path_box.min.y + 5.0),
                egui::Vec2::splat(28.0),
            );
            let folder_btn = egui::Rect::from_min_size(
                egui::pos2(caret_btn.min.x - 30.0, caret_btn.min.y),
                egui::Vec2::splat(28.0),
            );
            dm_field(
                ui,
                "popup_dir",
                path_box,
                folder_btn.min.x - 6.0,
                &mut self.save_base,
                "Save folder",
                false,
            );
            if self.remember_path {
                // The field holds the base folder; say where the rule sends it.
                ui.interact(
                    path_box,
                    egui::Id::new("popup_dir_tip"),
                    egui::Sense::hover(),
                )
                .on_hover_text(format!("→ {}", self.effective_dir()));
            }
            let f_b = ui.interact(folder_btn, egui::Id::new("popup-folder"), egui::Sense::click());
            icon_btn(&p, folder_btn, f_b.hovered(), DmIcon::Folder, 14.0, 1.5, pl().dm_dim);
            let c_b = ui.interact(caret_btn, egui::Id::new("popup-caret"), egui::Sense::click());
            icon_btn(&p, caret_btn, c_b.hovered(), DmIcon::Chevron, 10.0, 2.0, pl().dm_dim);
            if f_b.hovered() || c_b.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if c_b.clicked() {
                self.recent_open = !self.recent_open;
            }
            let recents = self.recent_dirs();
            if let Some(dir) = options_menu(
                &c_b,
                &recents,
                &mut self.recent_open,
                &self.save_base,
            ) {
                self.save_base = dir;
            }
            if f_b.clicked() {
                let mut dlg = rfd::FileDialog::new();
                if !self.save_base.trim().is_empty() {
                    dlg = dlg.set_directory(&self.save_base);
                }
                if let Some(d) = dlg.pick_folder() {
                    self.save_base = d.to_string_lossy().to_string();
                }
            }
            // size chip: grid mark, size (with duration when known), tick
            let chip = at(DM_X + DM_PATH_W + 8.0, DM_ROW2, DM_SIZE_W, DM_BOX_H);
            dm_box(&p, chip);
            dm_icon(
                &p,
                egui::Rect::from_center_size(
                    egui::pos2(chip.min.x + 21.0, chip.center().y),
                    egui::Vec2::splat(16.0),
                ),
                DmIcon::Grid,
                DM_ACC,
                0.0,
            );
            let known = !self.probing && self.probe_size.filter(|s| *s > 0).is_some();
            let sz_txt: String = if self.probing {
                "…".to_string()
            } else {
                match self.probe_size.filter(|s| *s > 0) {
                    Some(sz) if self.duration_secs > 0 => {
                        format!("{} • {}", format_dur(self.duration_secs), format_bytes(sz))
                    }
                    Some(sz) => format_bytes(sz),
                    // Streams (HLS/DASH) have no size until assembled; the
                    // duration is the useful fact, as IDM shows it.
                    None if self.duration_secs > 0 => format_dur(self.duration_secs),
                    None => "—".to_string(),
                }
            };
            let g = p.layout_no_wrap(sz_txt, reg(11.0), pl().dm_txt);
            let tw = g.size().x;
            let tx = chip.min.x + 37.0;
            p.galley(egui::pos2(tx, chip.center().y - g.size().y * 0.5), g, pl().dm_txt);
            if known {
                dm_icon(
                    &p,
                    egui::Rect::from_min_size(
                        egui::pos2(tx + tw + 8.0, chip.min.y + 13.0),
                        egui::Vec2::splat(12.0),
                    ),
                    DmIcon::Check,
                    DM_OK,
                    2.5,
                );
            }
            // ---- row 3: filename + refresh + advanced ----
            let file_box = at(DM_X, DM_ROW3, DM_FILE_W, DM_BOX_H);
            let refr_btn = egui::Rect::from_min_size(
                egui::pos2(file_box.max.x + 8.0, file_box.min.y),
                egui::Vec2::splat(38.0),
            );
            let gear_btn = egui::Rect::from_min_size(
                egui::pos2(refr_btn.max.x + 8.0, refr_btn.min.y),
                egui::Vec2::splat(38.0),
            );
            dm_field(
                ui,
                "popup_file",
                file_box,
                file_box.max.x - 13.0,
                &mut self.filename,
                "Filename",
                false,
            );
            let r_b = ui
                .interact(refr_btn, egui::Id::new("popup-refresh"), egui::Sense::click())
                .on_hover_text("Check the size and file name again");
            let g_b = ui
                .interact(gear_btn, egui::Id::new("popup-gear"), egui::Sense::click())
                .on_hover_text(if self.show_adv {
                    "Hide more options"
                } else {
                    "More options: category, description, referer, cookie"
                });
            // Square field-surface buttons, so they read as part of the
            // filename row (bare glyphs looked like stray icons).
            icon_btn_box(&p, refr_btn, r_b.hovered(), DmIcon::Refresh, 14.0, 1.5, pl().dm_dim);
            icon_btn_box(
                &p,
                gear_btn,
                g_b.hovered(),
                DmIcon::Settings,
                14.0,
                1.5,
                if self.show_adv { DM_ACC } else { pl().dm_dim },
            );
            if r_b.hovered() || g_b.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if r_b.clicked() {
                self.start_probe();
            }
            if g_b.clicked() {
                self.show_adv = !self.show_adv;
            }
            // ---- gear panel: the rows the concept has no place for ----
            if self.show_adv {
                let panel = at(DM_X, DM_ACT, DM_W_IN, panel_h);
                dm_box(&p, panel);
                let cx = panel.min.x + 13.0;
                let cw = panel.width() - 26.0;
                let half = (cw - 8.0) / 2.0;
                let mut cy = panel.min.y + 13.0;
                let cat_r = egui::Rect::from_min_size(
                    egui::pos2(cx, cy),
                    egui::vec2(half - 34.0, DM_BOX_H),
                );
                let plus_r = egui::Rect::from_min_size(
                    egui::pos2(cx + half - 30.0, cy),
                    egui::vec2(30.0, DM_BOX_H),
                );
                let desc_r = egui::Rect::from_min_size(
                    egui::pos2(cx + half + 8.0, cy),
                    egui::vec2(half, DM_BOX_H),
                );
                let mut cats: Vec<String> = CATEGORIES.iter().map(|c| c.to_string()).collect();
                cats.extend(self.custom_cats.iter().cloned());
                if self.adding_cat {
                    let name_r = egui::Rect::from_min_size(
                        cat_r.min,
                        egui::vec2(cat_r.width() - 48.0, DM_BOX_H),
                    );
                    let add_r = egui::Rect::from_min_size(
                        egui::pos2(cat_r.max.x - 44.0, cy),
                        egui::vec2(44.0, DM_BOX_H),
                    );
                    dm_field(
                        ui,
                        "popup_newcat",
                        name_r,
                        name_r.max.x - 10.0,
                        &mut self.new_cat,
                        "New category",
                        false,
                    );
                    let a_b = ui.interact(add_r, egui::Id::new("popup-catadd"), egui::Sense::click());
                    btn_flat(&p, add_r, a_b.hovered(), "Add");
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if a_b.clicked() || enter {
                        let name = self.new_cat.trim().to_string();
                        if !name.is_empty() {
                            if !self
                                .custom_cats
                                .iter()
                                .any(|c| c.eq_ignore_ascii_case(&name))
                            {
                                self.custom_cats.push(name.clone());
                            }
                            self.category = name;
                            self.new_cat.clear();
                            self.adding_cat = false;
                        }
                    }
                } else if let Some(pick) =
                    combo(ui, cat_r, "cat", &self.category, &cats, &mut self.cat_open)
                {
                    self.category = pick;
                }
                let p_b = plus_square(ui, plus_r, 1);
                if p_b.clicked() {
                    self.adding_cat = !self.adding_cat;
                }
                dm_field(
                    ui,
                    "popup_desc",
                    desc_r,
                    desc_r.max.x - 13.0,
                    &mut self.description,
                    "Description (optional)",
                    false,
                );
                cy += DM_BOX_H + 8.0;
                let ref_r = egui::Rect::from_min_size(egui::pos2(cx, cy), egui::vec2(half, DM_BOX_H));
                let cok_r = egui::Rect::from_min_size(
                    egui::pos2(cx + half + 8.0, cy),
                    egui::vec2(half, DM_BOX_H),
                );
                dm_field(
                    ui,
                    "popup_ref",
                    ref_r,
                    ref_r.max.x - 13.0,
                    &mut self.referer,
                    "Referer (optional)",
                    false,
                );
                dm_field(
                    ui,
                    "popup_cookie",
                    cok_r,
                    cok_r.max.x - 13.0,
                    &mut self.cookie,
                    "Cookie (optional)",
                    false,
                );
                cy += DM_BOX_H + 8.0;
                let row = egui::Rect::from_min_size(egui::pos2(cx, cy), egui::vec2(cw, 30.0));
                p.text(
                    egui::pos2(row.min.x, row.center().y),
                    egui::Align2::LEFT_CENTER,
                    "Connections",
                    reg(11.0),
                    pl().dm_dim,
                );
                let srect = egui::Rect::from_min_size(
                    egui::pos2(row.min.x + 92.0, row.min.y + 5.0),
                    egui::vec2(row.width() - 144.0, 20.0),
                );
                let frac = ((self.connections as f32 - 1.0) / 31.0).clamp(0.0, 1.0);
                if let Some(nf) = slider_row(ui, srect, "conns", frac) {
                    self.connections = ((nf * 31.0).round() as usize + 1).clamp(1, 32);
                }
                p.text(
                    egui::pos2(row.max.x, row.center().y),
                    egui::Align2::RIGHT_CENTER,
                    format!("{}", self.connections),
                    reg(11.0),
                    pl().dm_txt,
                );
                cy += 30.0 + 8.0;
                let rem = egui::Rect::from_min_size(egui::pos2(cx, cy), egui::vec2(cw, 22.0));
                if check_row(
                    ui,
                    rem,
                    "remember",
                    self.remember_path,
                    "Remember this path (category subfolder)",
                )
                .clicked()
                {
                    self.remember_path = !self.remember_path;
                }
                cy += 22.0;
                for n in &notes {
                    p.text(
                        egui::pos2(cx, cy + 6.0),
                        egui::Align2::LEFT_CENTER,
                        n,
                        reg(11.0),
                        pl().dm_dim,
                    );
                    cy += NOTE_H;
                }
            }
            // ---- actions: Add (queue only) | Download (start) | Cancel ----
            let act_y = DM_ACT + adv_h;
            let add_r = at(DM_X, act_y, 100.0, 36.0);
            let dl_r = egui::Rect::from_min_size(
                egui::pos2(add_r.max.x + 8.0, add_r.min.y),
                egui::vec2(140.0, 36.0),
            );
            let cancel_r = egui::Rect::from_min_size(
                egui::pos2(o.x + DM_W - 17.0 - 100.0, add_r.min.y),
                egui::vec2(100.0, 36.0),
            );
            let a_b = ui.interact(add_r, egui::Id::new("popup-add"), egui::Sense::click());
            let d_b = ui.interact(dl_r, egui::Id::new("popup-start"), egui::Sense::click());
            let x_b = ui.interact(cancel_r, egui::Id::new("popup-cancel"), egui::Sense::click());
            btn_flat(&p, add_r, a_b.hovered(), "Add");
            dm_gradient(&p, dl_r, DM_RAD, DM_ACC, DM_ACC2);
            if d_b.hovered() {
                p.rect_filled(dl_r, DM_RAD, egui::Color32::from_white_alpha(22));
            }
            p.text(
                dl_r.center(),
                egui::Align2::CENTER_CENTER,
                crate::i18n::t("Download"),
                semi(13.0),
                egui::Color32::WHITE,
            );
            btn_flat(&p, cancel_r, x_b.hovered(), "Cancel");
            if a_b.hovered() || d_b.hovered() || x_b.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if a_b.clicked() {
                self.commit(false);
            }
            if d_b.clicked() {
                self.commit(true);
            }
            if x_b.clicked() {
                self.stop_prestart(None);
                self.done = true;
            }
            // ---- notice (the window grew 16px for it) ----
            if !self.notice.is_empty() {
                let shown: String = self.notice.chars().take(78).collect();
                p.text(
                    egui::pos2(o.x + DM_X, act_y + 36.0 + 10.0),
                    egui::Align2::LEFT_CENTER,
                    shown,
                    reg(11.0),
                    pl().dm_warn,
                );
            }
        });
    }
}

#[cfg(test)]
mod audit_inbox_tests {
    #[test]
    fn claiming_a_capture_returns_private_cookies_and_preserves_other_entries() {
        let dir=std::env::temp_dir().join(format!("dc-inbox-audit-{}",std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();let path=dir.join("inbox.json");
        let data=serde_json::json!([
            {"url":"https://example.com/a","received_at":"2026-10-20","cookie":"session=secret"},
            {"url":"https://example.com/b","received_at":"2026-10-20"}
        ]);
        super::atomic_write(&path,serde_json::to_string(&data).unwrap().as_bytes()).unwrap();
        let capture=super::claim_inbox_at(&path,"https://example.com/a",None).unwrap();
        assert_eq!(capture.cookie.as_deref(),Some("session=secret"));
        let left:Vec<crate::native_host::InboxItem>=serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(left.len(),1);assert_eq!(left[0].url,"https://example.com/b");
        std::fs::write(&path,b"{broken").unwrap();
        assert!(super::claim_inbox_at(&path,"https://example.com/b",None).is_none());
        assert_eq!(std::fs::read(&path).unwrap(),b"{broken");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
