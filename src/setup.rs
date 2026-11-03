//! "Get ready" — what Download Craft needs, in one small window.
//!
//! `download_craft.exe --setup [--auto]`, its own process like the progress
//! and links windows, so it is never clipped by the main window. One row per
//! piece: the browser connection (native host), the browser extension, and
//! the helpers yt-dlp, FFmpeg and Deno. Each row says what it is for, whether
//! it is ready, and has a button that fixes it; "Fix everything" runs every
//! automatic fix in turn. The main window opens it at start-up only when
//! something is missing (`--auto` starts the automatic fixes at once); the
//! Help menu opens it any time.
//!
//! Every check and fix runs on a worker thread: version probes and downloads
//! never freeze the window.

use crate::app::pl;
use crate::dm::*;
use crate::fonts::{reg, semi};
use crate::model::claim_window;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

const SW: f32 = 520.0;
/// Opening height; the window then fits itself to its rows.
const SH: f32 = 470.0;
pub(crate) use crate::setup_service::{
    check, extension_directory, helper_lines, something_missing, run_checks, run_fixes, Msg, Part, State,
};

/// Open the window (its own process). `auto` starts the automatic fixes.
pub fn spawn(auto: bool) {
    let Ok(exe) = crate::platform::launch_path() else { return };
    let mut cmd = crate::platform::silent_command(exe);
    cmd.arg("--setup");
    if auto {
        cmd.arg("--auto");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008);
    }
    let _ = cmd.spawn();
}

struct SetupApp {
    states: Vec<(Part, State)>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    busy: bool,
    show_at_start: bool,
    placed: bool,
    last_ext_check: Instant,
    note: String,
    /// The height last asked of the window.
    win_h: f32,
}

impl SetupApp {
    fn state(&self, part: Part) -> &State {
        &self.states.iter().find(|(p, _)| *p == part).expect("every part has a state").1
    }
    fn set(&mut self, part: Part, s: State) {
        if let Some(e) = self.states.iter_mut().find(|(p, _)| *p == part) {
            e.1 = s;
        }
    }
    fn missing_automatic(&self) -> Vec<Part> {
        self.states
            .iter()
            .filter(|(p, s)| p.automatic() && matches!(s, State::Missing(_) | State::Failed(_)))
            .map(|(p, _)| *p)
            .collect()
    }
    fn fix_parts(&mut self, parts: Vec<Part>) {
        if parts.is_empty() || self.busy {
            return;
        }
        self.busy = true;
        run_fixes(self.tx.clone(), parts);
    }
}

fn close_soon(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(400));
        std::process::exit(0);
    });
}

pub fn run() -> eframe::Result<()> {
    let Some(_slot) = claim_window("setup") else {
        return Ok(());
    };
    let auto = std::env::args().any(|a| a == "--auto");
    let (tx, rx) = channel();
    // Checks first; with --auto the automatic fixes follow as soon as the
    // checks say what is missing (see `update`).
    run_checks(tx.clone(), Part::ALL.to_vec());
    let app = SetupApp {
        states: Part::ALL.iter().map(|p| (*p, State::Checking)).collect(),
        tx,
        rx,
        busy: true,
        show_at_start: crate::model::ui_pref_bool("setup_at_start", false),
        placed: false,
        last_ext_check: Instant::now(),
        win_h: SH,
        note: if auto { "auto".into() } else { String::new() },
    };
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Get ready — Download Craft")
        .with_inner_size([SW, SH])
        .with_resizable(false)
        .with_decorations(crate::platform::system_title_bar())
        .with_transparent(!crate::platform::system_title_bar());
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    eframe::run_native(
        crate::model::APP_NAME,
        eframe::NativeOptions { viewport, ..Default::default() },
        Box::new(|cc| {
            crate::fonts::install(cc, true);
            crate::app::apply_theme(&cc.egui_ctx, &crate::app::stored_theme());
            Ok(Box::new(app))
        }),
    )
}

impl eframe::App for SetupApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::State(p, s) => self.set(p, s),
                Msg::Idle => {
                    self.busy = false;
                    // --auto: once the first checks are in, fix what the app
                    // can fix by itself, straight away.
                    if self.note == "auto" {
                        self.note.clear();
                        let parts = self.missing_automatic();
                        self.fix_parts(parts);
                    }
                }
            }
        }
        // The extension turns green as soon as the browser connects.
        if self.last_ext_check.elapsed() >= Duration::from_secs(2) && !self.busy {
            self.last_ext_check = Instant::now();
            if !self.state(Part::Extension).ready() {
                let s = check(Part::Extension);
                self.set(Part::Extension, s);
            }
        }
        ctx.request_repaint_after(Duration::from_millis(if self.busy { 100 } else { 1000 }));
        if !self.placed {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            if crate::model::center_window(ctx, egui::vec2(SW, SH)) {
                self.placed = true;
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close_soon(ctx);
        }
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| self.paint(ui));
    }
}

impl SetupApp {
    fn paint(&mut self, ui: &mut egui::Ui) {
        let t = pl();
        let ctx = ui.ctx().clone();
        let full = ui.max_rect();
        let p = ui.painter().clone();
        p.rect(full.shrink(0.5), 12.0, t.modal_bg, egui::Stroke::new(1.0, t.card_bd), egui::StrokeKind::Inside);
        let o = full.min;
        let x0 = o.x + 22.0;
        let w = full.width() - 44.0;

        // ---- header: title, drag, close ----
        let head = egui::Rect::from_min_size(o, egui::vec2(full.width(), 64.0));
        let drag = ui.interact(head.with_max_x(head.max.x - 48.0), egui::Id::new("setup-drag"), egui::Sense::click_and_drag());
        if drag.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        let ready = self.states.iter().filter(|(_, s)| s.ready()).count();
        let all = self.states.len();
        p.text(egui::pos2(x0, o.y + 24.0), egui::Align2::LEFT_CENTER, "Get ready", semi(17.0), t.txt);
        let sub = if ready == all {
            "Everything is ready.".to_string()
        } else {
            format!("{ready} of {all} ready — fix the rest with one click.")
        };
        p.text(egui::pos2(x0, o.y + 46.0), egui::Align2::LEFT_CENTER, sub, reg(12.0), t.dim);
        let close = egui::Rect::from_center_size(egui::pos2(full.max.x - 28.0, o.y + 28.0), egui::vec2(30.0, 30.0));
        let cr = ui.interact(close, egui::Id::new("setup-close"), egui::Sense::click()).on_hover_text("Close");
        if cr.hovered() {
            p.rect_filled(close, 6.0, t.surface_lt);
        }
        dm_icon(&p, close.shrink(9.0), DmIcon::Close, if cr.hovered() { t.txt } else { t.dim }, 2.0);
        if cr.clicked() {
            close_soon(&ctx);
        }

        // ---- rows ----
        let mut y = o.y + 76.0;
        let parts: Vec<(Part, State)> = self.states.clone();
        let mut to_fix: Option<Part> = None;
        for (part, state) in parts {
            let tall = part == Part::Extension && !state.ready();
            let h = if tall { 150.0 } else { 54.0 };
            let card = egui::Rect::from_min_size(egui::pos2(x0, y), egui::vec2(w, h));
            p.rect(card, 10.0, t.surface, egui::Stroke::new(1.0, t.card_bd), egui::StrokeKind::Inside);
            // Status dot / tick.
            let c = egui::pos2(card.min.x + 22.0, card.min.y + 27.0);
            match &state {
                State::Ready(_) => {
                    p.circle_filled(c, 11.0, DM_OK.gamma_multiply(0.18));
                    dm_icon(&p, egui::Rect::from_center_size(c, egui::vec2(12.0, 12.0)), DmIcon::Check, DM_OK, 2.6);
                }
                State::Working(_) | State::Checking => {
                    let a = ui.input(|i| i.time) as f32 * 300.0;
                    dm_arc(&p, c, 8.0, a, a + 260.0, egui::Stroke::new(2.2, t.primary));
                }
                State::Failed(_) => {
                    p.circle_filled(c, 11.0, t.warning.gamma_multiply(0.18));
                    p.text(c, egui::Align2::CENTER_CENTER, "!", semi(13.0), t.warning);
                }
                State::Missing(_) => {
                    p.circle_stroke(c, 9.0, egui::Stroke::new(1.6, DM_YEL));
                }
            }
            let tx = card.min.x + 44.0;
            p.text(egui::pos2(tx, card.min.y + 18.0), egui::Align2::LEFT_CENTER, part.title(), semi(13.5), t.txt);
            let detail = match &state {
                State::Checking => "Checking…".to_string(),
                State::Ready(s) | State::Missing(s) | State::Working(s) => s.clone(),
                State::Failed(e) => format!("Could not fix: {e}"),
            };
            let line2 = if state.ready() { detail } else { format!("{} · {}", part.purpose(), detail) };
            // The extension's buttons sit under its steps, so its line is
            // free to the edge; other rows keep room for their button.
            let room = card.width() - 44.0 - if tall { 16.0 } else { 130.0 };
            let l2 = crate::app::truncate_to_width(&p, &line2, reg(11.5), t.dim, room);
            let resp_detail = ui.interact(egui::Rect::from_min_size(egui::pos2(tx, card.min.y + 29.0), egui::vec2(room, 16.0)), egui::Id::new(("setup-detail", part as u8)), egui::Sense::hover());
            if l2 != line2 {
                resp_detail.on_hover_text(&line2);
            }
            p.text(egui::pos2(tx, card.min.y + 37.0), egui::Align2::LEFT_CENTER, l2, reg(11.5), if matches!(state, State::Failed(_)) { t.warning } else { t.dim });
            // The row's button.
            if part.automatic() && !state.ready() && !matches!(state, State::Working(_) | State::Checking) {
                let label = if matches!(state, State::Failed(_)) { "Try again" } else if part == Part::Connection { "Set up" } else { "Install" };
                let br = egui::Rect::from_min_size(egui::pos2(card.max.x - 106.0, card.min.y + 11.0), egui::vec2(92.0, 32.0));
                let r = ui.interact(br, egui::Id::new(("setup-fix", part as u8)), egui::Sense::click());
                p.rect_filled(br, 7.0, if r.hovered() && !self.busy { t.primary.gamma_multiply(0.85) } else { t.primary });
                p.text(br.center(), egui::Align2::CENTER_CENTER, label, semi(12.0), egui::Color32::WHITE);
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !self.busy {
                    to_fix = Some(part);
                }
            }
            if tall {
                // How to add it: three steps and two buttons.
                let steps = [
                    "1. Open chrome://extensions and turn on Developer mode.",
                    "2. Click Load unpacked.",
                    "3. Paste the folder path (Ctrl+V) and choose Select Folder.",
                ];
                for (k, s) in steps.iter().enumerate() {
                    let line = crate::app::truncate_to_width(&p, s, reg(11.5), t.txt, card.width() - 60.0);
                    p.text(egui::pos2(tx, card.min.y + 60.0 + k as f32 * 18.0), egui::Align2::LEFT_CENTER, line, reg(11.5), t.txt);
                }
                let folder = crate::platform::extension_directory(false).ok();
                let by = card.max.y - 44.0;
                let b1 = egui::Rect::from_min_size(egui::pos2(tx, by), egui::vec2(140.0, 32.0));
                let b2 = egui::Rect::from_min_size(egui::pos2(tx + 148.0, by), egui::vec2(110.0, 32.0));
                for (r, label, k) in [(b1, "Open folder", 0u8), (b2, "Copy path", 1u8)] {
                    let resp = ui.interact(r, egui::Id::new(("setup-ext", k)), egui::Sense::click());
                    btn_flat(&p, r, resp.hovered(), label);
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        if let Some(f) = &folder {
                            if k == 0 {
                                crate::app::open_path(f, false);
                            } else {
                                ctx.copy_text(f.to_string_lossy().to_string());
                                self.note = "Path copied — paste it in Load unpacked.".into();
                            }
                        }
                    }
                }
            }
            y += h + 8.0;
        }
        if let Some(part) = to_fix {
            self.fix_parts(vec![part]);
        }

        // ---- footer ----
        // Fit the window to its rows (no empty space under them).
        let want = (y - o.y) + 58.0;
        if (want - self.win_h).abs() > 1.0 {
            self.win_h = want;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(SW, want)));
        }
        let fy = full.max.y - 32.0;
        // Show at start-up when something is missing.
        let cb = egui::Rect::from_min_size(egui::pos2(x0, fy - 9.0), egui::vec2(18.0, 18.0));
        let lab = "Show at start when something is missing";
        let cbr = ui.interact(cb.union(egui::Rect::from_min_size(egui::pos2(cb.max.x + 6.0, fy - 9.0), egui::vec2(240.0, 18.0))), egui::Id::new("setup-cb"), egui::Sense::click());
        p.rect(cb, 4.0, if self.show_at_start { t.primary } else { t.bg }, egui::Stroke::new(1.0, if self.show_at_start { t.primary } else { t.card_bd }), egui::StrokeKind::Inside);
        if self.show_at_start {
            dm_icon(&p, cb.shrink(4.0), DmIcon::Check, egui::Color32::WHITE, 2.6);
        }
        p.text(egui::pos2(cb.max.x + 8.0, fy), egui::Align2::LEFT_CENTER, lab, reg(11.5), if cbr.hovered() { t.txt } else { t.dim });
        if cbr.clicked() {
            self.show_at_start = !self.show_at_start;
            crate::model::ui_pref_set_bool("setup_at_start", self.show_at_start);
        }
        if !self.note.is_empty() && self.note != "auto" {
            p.text(egui::pos2(x0, fy - 28.0), egui::Align2::LEFT_CENTER, &self.note, reg(11.5), DM_OK);
        }
        let missing = self.missing_automatic();
        let (label, primary) = if self.busy {
            ("Working…", true)
        } else if missing.is_empty() {
            ("Done", true)
        } else {
            ("Fix everything", true)
        };
        let bw = 132.0;
        let br = egui::Rect::from_min_size(egui::pos2(full.max.x - 22.0 - bw, fy - 18.0), egui::vec2(bw, 36.0));
        let r = ui.interact(br, egui::Id::new("setup-main"), egui::Sense::click());
        let _ = primary;
        p.rect_filled(br, 8.0, if r.hovered() && !self.busy { t.primary.gamma_multiply(0.85) } else { t.primary.gamma_multiply(if self.busy { 0.6 } else { 1.0 }) });
        p.text(br.center(), egui::Align2::CENTER_CENTER, label, semi(13.0), egui::Color32::WHITE);
        if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !self.busy {
            if missing.is_empty() {
                close_soon(&ctx);
            } else {
                self.fix_parts(missing);
            }
        }
    }
}
